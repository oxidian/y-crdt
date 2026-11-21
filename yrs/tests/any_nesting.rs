//! An `Any` value from a peer nests as deep as its bytes run, so decoding, encoding and dropping one
//! must not take a level of the native stack per level of nesting.

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use yrs::encoding::read::{Cursor, Error, Read};
use yrs::encoding::write::Write;
use yrs::updates::decoder::Decode;
use yrs::{Any, Array, Doc, GetString, Out, ReadTxn, StateVector, Text, Transact, Update};

/// Far deeper than a level of the stack per level of nesting survives on `SMALL_STACK`.
const LEVELS: usize = 200_000;
const SMALL_STACK: usize = 128 * 1024;

fn on_a_small_stack<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(work)
        .unwrap()
        .join()
        .unwrap()
}

fn decoded(input: &[u8]) -> Result<Any, Error> {
    Any::decode(&mut Cursor::new(input))
}

fn encoded(value: &Any) -> Vec<u8> {
    let mut out = Vec::new();
    value.encode(&mut out);
    out
}

/// The shapes a deep value takes: through the first or the last value of an array, through a map,
/// and through the two in turn.
fn deep_bytes(levels: usize) -> Vec<(&'static str, Vec<u8>)> {
    let mut mixed = Vec::new();
    for level in 0..levels {
        if level % 2 == 0 {
            mixed.extend_from_slice(&[117, 2, 120]);
        } else {
            mixed.extend_from_slice(&[118, 1, 1, b'k']);
        }
    }
    mixed.push(126);
    let mut through_first = [117u8, 1].repeat(levels);
    through_first.push(126);
    let mut through_last = [117u8, 2, 126].repeat(levels);
    through_last.push(126);
    let mut through_maps = [118u8, 1, 1, b'k'].repeat(levels);
    through_maps.push(126);
    vec![
        ("arrays through their first value", through_first),
        ("arrays through their last value", through_last),
        ("maps", through_maps),
        ("arrays and maps in turn", mixed),
    ]
}

/// A null inside `levels` arrays, built without decoding.
fn deep_value(levels: usize) -> Any {
    let mut value = Any::Null;
    for _ in 0..levels {
        value = Any::Array(Arc::from(vec![value]));
    }
    value
}

#[test]
fn a_value_nested_far_past_the_stack_decodes_and_encodes_to_its_bytes() {
    on_a_small_stack(|| {
        for (shape, bytes) in deep_bytes(LEVELS) {
            let value = decoded(&bytes).unwrap_or_else(|error| panic!("{}: {}", shape, error));
            assert!(encoded(&value) == bytes, "{} encode to other bytes", shape);
        }
    });
}

#[test]
fn a_value_nested_far_past_the_stack_drops() {
    on_a_small_stack(|| drop(deep_value(LEVELS)));
}

#[test]
fn a_value_cut_short_far_past_the_stack_is_refused() {
    on_a_small_stack(|| {
        for (shape, mut bytes) in deep_bytes(LEVELS) {
            bytes.pop();
            assert!(
                matches!(decoded(&bytes), Err(Error::EndOfBuffer(_))),
                "{} cut short decode",
                shape
            );
        }
    });
}

#[test]
fn dropping_one_owner_of_a_deep_value_leaves_every_other_owner_whole() {
    on_a_small_stack(|| {
        let bytes = [117u8, 1]
            .repeat(LEVELS)
            .into_iter()
            .chain([126])
            .collect::<Vec<_>>();
        let value = decoded(&bytes).unwrap();
        let shared = value.clone();
        let inner = match &value {
            Any::Array(items) => items[0].clone(),
            _ => unreachable!("the value is an array"),
        };
        drop(value);
        assert!(encoded(&shared) == bytes, "the other owner's value changed");
        drop(shared);
        assert!(
            encoded(&inner) == bytes[2..],
            "the inner value's owner's value changed"
        );
    });
}

fn update_beside_text(value: Any, v2: bool) -> Vec<u8> {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("text");
    let array = doc.get_or_insert_array("array");
    let mut txn = doc.transact_mut();
    text.insert(&mut txn, 0, "text beside a deep value");
    array.push_back(&mut txn, value);
    drop(txn);
    let txn = doc.transact();
    if v2 {
        txn.encode_state_as_update_v2(&StateVector::default())
    } else {
        txn.encode_state_as_update_v1(&StateVector::default())
    }
}

#[test]
fn a_value_nested_far_past_the_stack_round_trips_through_a_v1_and_a_v2_update() {
    on_a_small_stack(|| {
        for v2 in [false, true] {
            let update = update_beside_text(deep_value(LEVELS), v2);
            let target = Doc::with_client_id(2);
            let decoded = if v2 {
                Update::decode_v2(&update)
            } else {
                Update::decode_v1(&update)
            };
            target
                .transact_mut()
                .apply_update(decoded.unwrap())
                .unwrap();
            let text = target.get_or_insert_text("text");
            let array = target.get_or_insert_array("array");
            let txn = target.transact();
            assert_eq!(text.get_string(&txn), "text beside a deep value");
            match array.get(&txn, 0) {
                Some(Out::Any(value)) => {
                    assert!(
                        encoded(&value) == encoded(&deep_value(LEVELS)),
                        "v2: {}",
                        v2
                    )
                }
                other => panic!("v2: {}: the array holds {:?}", v2, other),
            }
        }
    });
}

/// Upstream's decoding, which recurses a level at a time.
fn decoded_by_recursion<R: Read>(decoder: &mut R) -> Result<Any, Error> {
    Ok(match decoder.read_u8()? {
        127 => Any::Undefined,
        126 => Any::Null,
        125 => Any::Number(yrs::any::Number::Int(decoder.read_var::<i64>()?)),
        124 => Any::Number(yrs::any::Number::Float(decoder.read_f32()? as f64)),
        123 => Any::Number(yrs::any::Number::Float(decoder.read_f64()?)),
        122 => Any::Number(yrs::any::Number::Int(decoder.read_i64()?)),
        121 => Any::Bool(false),
        120 => Any::Bool(true),
        119 => Any::String(Arc::from(decoder.read_string()?)),
        118 => {
            let len: usize = decoder.read_var()?;
            let mut map = HashMap::new();
            map.try_reserve(len)?;
            for _ in 0..len {
                let key = decoder.read_string()?.to_owned();
                map.insert(key, decoded_by_recursion(decoder)?);
            }
            Any::Map(Arc::new(map))
        }
        117 => {
            let len: usize = decoder.read_var()?;
            let mut arr = Vec::new();
            arr.try_reserve(len)?;
            for _ in 0..len {
                arr.push(decoded_by_recursion(decoder)?);
            }
            Any::Array(Arc::from(arr))
        }
        116 => Any::Buffer(Arc::from(decoder.read_buf()?)),
        _ => return Err(Error::UnexpectedValue),
    })
}

/// Upstream's encoding, which recurses a level at a time.
fn encoded_by_recursion(value: &Any, out: &mut Vec<u8>) {
    match value {
        Any::Array(items) => {
            out.write_u8(117);
            out.write_var(items.len() as u64);
            for item in items.iter() {
                encoded_by_recursion(item, out);
            }
        }
        Any::Map(entries) => {
            out.write_u8(118);
            out.write_var(entries.len() as u64);
            for (key, entry) in entries.iter() {
                out.write_string(key);
                encoded_by_recursion(entry, out);
            }
        }
        leaf => leaf.encode(out),
    }
}

fn values() -> impl Strategy<Value = Any> {
    let leaf = prop_oneof![
        Just(Any::Null),
        Just(Any::Undefined),
        any::<bool>().prop_map(Any::Bool),
        any::<i32>().prop_map(|n| Any::from(n as i64)),
        any::<i64>().prop_map(|n| Any::Number(yrs::any::Number::Int(n))),
        (-1e12f64..1e12f64).prop_map(Any::from),
        "[a-z]{0,3}".prop_map(|s| Any::from(s.as_str())),
        prop::collection::vec(any::<u8>(), 0..4).prop_map(Any::from),
    ];
    leaf.prop_recursive(6, 64, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4)
                .prop_map(|items| Any::Array(Arc::from(items))),
            prop::collection::hash_map("[a-c]{0,2}", inner, 0..4)
                .prop_map(|entries| Any::Map(Arc::new(entries))),
        ]
    })
}

/// A value's bytes, and those bytes with one byte changed or cut short at a point drawn from
/// `at`, so the readings are compared on input that is no value as well as on values.
fn inputs() -> impl Strategy<Value = Vec<u8>> {
    (values(), any::<prop::sample::Index>(), any::<u8>(), 0u8..3).prop_map(
        |(value, at, byte, damage)| {
            let mut bytes = Vec::new();
            encoded_by_recursion(&value, &mut bytes);
            let at = at.index(bytes.len());
            match damage {
                0 => {}
                1 => bytes[at] = byte,
                _ => bytes.truncate(at),
            }
            bytes
        },
    )
}

/// Whether two values are the same, a float the same as itself whatever its bits spell.
fn same(left: &Any, right: &Any) -> bool {
    use yrs::any::Number;
    match (left, right) {
        (Any::Number(Number::Float(left)), Any::Number(Number::Float(right))) => {
            left.to_bits() == right.to_bits()
        }
        (Any::Array(left), Any::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| same(left, right))
        }
        (Any::Map(left), Any::Map(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, left)| right.get(key).is_some_and(|right| same(left, right)))
        }
        (left, right) => left == right,
    }
}

/// `value` inside `levels` arrays and maps in turn, so its nesting crosses the levels dropping
/// recurses into.
fn wrapped(mut value: Any, levels: usize) -> Any {
    for level in 0..levels {
        value = if level % 2 == 0 {
            Any::Array(Arc::from(vec![Any::Null, value]))
        } else {
            Any::Map(Arc::new(HashMap::from([("k".to_string(), value)])))
        };
    }
    value
}

/// Every value inside `value`, and `value` itself, each a clone sharing what it holds.
fn parts_of(value: &Any) -> Vec<Any> {
    let mut parts = vec![value.clone()];
    let mut at = 0;
    while at < parts.len() {
        let inside: Vec<Any> = match &parts[at] {
            Any::Array(items) => items.iter().cloned().collect(),
            Any::Map(entries) => entries.values().cloned().collect(),
            _ => Vec::new(),
        };
        parts.extend(inside);
        at += 1;
    }
    parts
}

proptest! {
    #[test]
    fn dropping_a_value_leaves_every_part_held_elsewhere_as_it_was(
        value in values(),
        levels in 0usize..200,
        kept in prop::collection::vec(any::<prop::sample::Index>(), 0..4),
    ) {
        let value = wrapped(value, levels);
        let parts = parts_of(&value);
        let kept: Vec<Any> = kept.iter().map(|at| parts[at.index(parts.len())].clone()).collect();
        let before: Vec<Vec<u8>> = kept.iter().map(encoded).collect();
        drop(parts);
        drop(value);
        for (part, bytes) in kept.iter().zip(before) {
            prop_assert_eq!(encoded(part), bytes);
        }
    }

    #[test]
    fn a_value_encodes_to_the_bytes_recursion_writes(value in values()) {
        let mut expected = Vec::new();
        encoded_by_recursion(&value, &mut expected);
        prop_assert_eq!(encoded(&value), expected);
    }

    #[test]
    fn any_bytes_decode_as_recursion_reads_them(bytes in inputs()) {
        let mut cursor = Cursor::new(&bytes);
        let expected = decoded_by_recursion(&mut cursor).map(|value| (value, cursor.next));
        let mut cursor = Cursor::new(&bytes);
        let actual = Any::decode(&mut cursor).map(|value| (value, cursor.next));
        match (actual, expected) {
            (Ok((actual, actual_end)), Ok((expected, expected_end))) => {
                prop_assert!(same(&actual, &expected), "{:?} is not {:?}", actual, expected);
                prop_assert_eq!(actual_end, expected_end);
            }
            (actual, expected) => prop_assert_eq!(format!("{:?}", actual), format!("{:?}", expected)),
        }
    }
}
