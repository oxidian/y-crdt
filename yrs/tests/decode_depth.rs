//! Decoding input from a peer must stop nesting before it exhausts the stack.

use std::collections::HashMap;
use std::sync::Arc;
use yrs::encoding::read::{Cursor, Error};
use yrs::encoding::write::Write;
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{Any, Array, Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

fn varint(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.write_var(value);
    out
}

fn decode_any(input: &[u8]) -> Result<Any, Error> {
    Any::decode(&mut Cursor::new(input))
}

/// A v1 update holding one client, one block and the given item content.
fn v1_item(content_ref: u8, content: &[u8]) -> Vec<u8> {
    let mut input = vec![
        1, // clients
        1, // blocks
        1, // client id
        0, // clock
        content_ref,
        1, // the parent is a root type, named below
        1,
        b'a',
    ];
    input.extend_from_slice(content);
    input
}

/// The separate streams of a v2 update describing one block with a root type parent.
fn v2_update(main: &[u8]) -> Vec<u8> {
    let empty: &[u8] = &[];
    // string payload "a" followed by its lengths
    let strings: &[u8] = &[1, b'a', 1];
    let streams: [&[u8]; 9] = [empty, &[1], empty, empty, &[8], strings, &[1], empty, &[1]];
    let mut out = vec![0]; // feature flag
    for stream in streams {
        out.extend(varint(stream.len() as u64));
        out.extend_from_slice(stream);
    }
    out.extend_from_slice(main);
    out
}

fn deep_array_bytes(levels: usize) -> Vec<u8> {
    let mut input = [117u8, 1].repeat(levels);
    input.push(126);
    input
}

/// Nesting that continues through the last element rather than the first.
fn deep_array_through_last_element_bytes(levels: usize) -> Vec<u8> {
    let mut input = [117u8, 2, 126].repeat(levels);
    input.push(126);
    input
}

fn deep_map_bytes(levels: usize) -> Vec<u8> {
    let mut input = [118u8, 1, 1, b'k'].repeat(levels);
    input.push(126);
    input
}

fn deep_mixed_bytes(levels: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for level in 0..levels {
        if level % 2 == 0 {
            input.extend_from_slice(&[117, 1]);
        } else {
            input.extend_from_slice(&[118, 1, 1, b'k']);
        }
    }
    input.push(126);
    input
}

fn deep_value(levels: usize) -> Any {
    let mut value = Any::Null;
    for level in 0..levels {
        value = if level % 2 == 0 {
            Any::Array(Arc::from(vec![value]))
        } else {
            Any::Map(Arc::new(HashMap::from([("k".to_string(), value)])))
        };
    }
    value
}

fn assert_refused(result: Result<impl std::fmt::Debug, Error>) {
    match result {
        Err(Error::UnexpectedValue) => {}
        other => panic!("expected UnexpectedValue, got {:?}", other),
    }
}

/// Deep enough to overflow any thread stack this suite could be given.
const BOMB_LEVELS: usize = 200_000;

#[test]
fn deeply_nested_array_is_refused() {
    assert_refused(decode_any(&deep_array_bytes(BOMB_LEVELS)));
}

#[test]
fn array_nested_through_its_last_element_is_refused() {
    assert_refused(decode_any(&deep_array_through_last_element_bytes(
        BOMB_LEVELS,
    )));
}

#[test]
fn deeply_nested_map_is_refused() {
    assert_refused(decode_any(&deep_map_bytes(BOMB_LEVELS)));
}

#[test]
fn alternating_arrays_and_maps_are_refused() {
    assert_refused(decode_any(&deep_mixed_bytes(BOMB_LEVELS)));
}

#[test]
fn deep_value_inside_a_v1_update_is_refused() {
    let mut content = vec![1]; // one value
    content.extend(deep_array_bytes(BOMB_LEVELS));
    assert_refused(Update::decode_v1(&v1_item(8, &content)));
}

#[test]
fn deep_value_inside_a_v2_update_is_refused() {
    let mut main = vec![1, 1, 0]; // clients, blocks, clock
    main.extend(deep_map_bytes(BOMB_LEVELS));
    assert_refused(Update::decode_v2(&v2_update(&main)));
}

#[test]
fn nesting_limit_is_exact() {
    let limit = Any::MAX_DECODE_DEPTH;
    let at_limit = decode_any(&deep_array_bytes(limit)).unwrap();
    let mut encoded = Vec::new();
    at_limit.encode(&mut encoded);
    assert_eq!(encoded, deep_array_bytes(limit));
    decode_any(&deep_map_bytes(limit)).unwrap();
    assert_refused(decode_any(&deep_array_bytes(limit + 1)));
    assert_refused(decode_any(&deep_map_bytes(limit + 1)));
}

/// The limit has to leave a value that everything else can recurse over, on a stack the size
/// wasm gets by default. Dropping the value is the operation that matters most, because it cannot
/// be refused.
#[test]
fn value_at_the_limit_is_safe_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let value = deep_value(Any::MAX_DECODE_DEPTH);
            let mut encoded = Vec::new();
            value.encode(&mut encoded);
            let decoded = decode_any(&encoded).unwrap();
            assert_eq!(decoded, value);
            let mut json = String::new();
            decoded.to_json(&mut json);
            drop(decoded);
            drop(value);
        })
        .unwrap()
        .join()
        .unwrap();
}

fn sample_doc() -> Doc {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("text");
    let array = doc.get_or_insert_array("array");
    let mut txn = doc.transact_mut();
    text.insert(&mut txn, 0, "text beside a deep value");
    array.push_back(&mut txn, deep_value(Any::MAX_DECODE_DEPTH));
    drop(txn);
    doc
}

fn apply_full_state(source: &Doc, encoding: Encoding) -> Doc {
    let target = Doc::with_client_id(2);
    let everything = StateVector::default();
    let update = {
        let txn = source.transact();
        match encoding {
            Encoding::V1 => Update::decode_v1(&txn.encode_state_as_update_v1(&everything)),
            Encoding::V2 => Update::decode_v2(&txn.encode_state_as_update_v2(&everything)),
        }
    };
    target.transact_mut().apply_update(update.unwrap()).unwrap();
    target
}

enum Encoding {
    V1,
    V2,
}

fn assert_same_content(left: &Doc, right: &Doc) {
    let left_text = left.get_or_insert_text("text");
    let right_text = right.get_or_insert_text("text");
    let left_array = left.get_or_insert_array("array");
    let right_array = right.get_or_insert_array("array");
    let (left_txn, right_txn) = (left.transact(), right.transact());
    assert_eq!(
        left_text.get_string(&left_txn),
        right_text.get_string(&right_txn)
    );
    assert_eq!(right_array.len(&right_txn), 1);
    assert_eq!(
        left_array.to_json(&left_txn),
        right_array.to_json(&right_txn)
    );
}

#[test]
fn update_holding_a_value_at_the_limit_round_trips_in_v1() {
    let source = sample_doc();
    assert_same_content(&source, &apply_full_state(&source, Encoding::V1));
}

#[test]
fn update_holding_a_value_at_the_limit_round_trips_in_v2() {
    let source = sample_doc();
    assert_same_content(&source, &apply_full_state(&source, Encoding::V2));
}
