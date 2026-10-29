//! SQL-level integration tests: LOAD the built `.duckdb_extension` into a real
//! in-memory DuckDB and exercise the full function surface (9 scalars).
//!
//! Run: `cargo test --release` (after `make release` + `append_metadata`).
//!
//! Bundled test DuckDB is v1.1.0: `x'..'` BLOB literals and `to_blob()` are
//! v2-only, so every BSON doc is built via `bson_from_json('{Extended-JSON}')`
//! (the function under test, producing real BSON bytes) and read back with
//! `CAST(from_hex(to_hex(blob)) AS BLOB)` / `from_hex(to_hex(...))`.
//! Struct `-> 'field'` returns a JSON-typed value on v1.1.0, so STRUCT field
//! assertions CAST to VARCHAR and match the exact rendering.

use quack_rs::testing::InMemoryDb;

fn db() -> InMemoryDb {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/release/bson.duckdb_extension");
    assert!(path.exists(), "build first: make release -> {path:?}");
    let d = InMemoryDb::open_unsigned().expect("open in-memory db");
    d.execute_batch(&format!("LOAD '{}'", path.display())).expect("load extension");
    d
}

/// A small nested Extended-JSON v2 document (real BSON after `bson_from_json`).
const DOC: &str = r#"{"a":{"x":1,"arr":[1,2]},"s":"hi"}"#;
/// `bson_from_json('{DOC}')` materialized once into a temp table's hex column.
fn materialize(d: &InMemoryDb) {
    d.execute_batch(
        &format!("CREATE OR REPLACE TEMP TABLE t AS SELECT to_hex(bson_from_json('{DOC}')) AS h"),
    )
    .unwrap();
}
/// Read the materialized doc back as a BLOB expression.
fn blob(d: &InMemoryDb) -> String {
    format!("CAST(from_hex((SELECT h FROM t)) AS BLOB)")
}

#[test]
fn from_json_produces_valid_bson() {
    let d = db();
    materialize(&d);
    let valid: bool = d.query_one(&format!("SELECT bson_is_valid({})", blob(&d))).unwrap();
    assert!(valid);
    let ok: String = d
        .query_one(&format!(
            "SELECT CAST(bson_well_formed({}) -> 'ok' AS VARCHAR)",
            blob(&d)
        ))
        .unwrap();
    assert_eq!(ok, "true");
    let kind: Option<String> = d
        .query_one(&format!(
            "SELECT CAST(bson_well_formed({}) -> 'kind' AS VARCHAR)",
            blob(&d)
        ))
        .unwrap();
    assert_eq!(kind, Some("null".to_string()));
}

#[test]
fn decode_canonical_and_relaxed() {
    let d = db();
    materialize(&d);
    let canon: String = d
        .query_one(&format!(
            "SELECT bson_to_json({}, 'canonical')",
            blob(&d)
        ))
        .unwrap();
    // Canonical Extended-JSON v2 wraps int32/64 as {"$numberInt": ...}.
    assert!(canon.contains("$numberInt"), "{canon}");
    assert!(canon.contains("\"s\":\"hi\""), "{canon}");
    let relaxed: String = d
        .query_one(&format!("SELECT bson_to_json({}, 'relaxed')", blob(&d)))
        .unwrap();
    assert!(relaxed.contains("\"x\":1"), "{relaxed}");
    assert!(!relaxed.contains("$numberInt"), "{relaxed}");
    // bson_decode is the canonical path.
    let dec: String = d
        .query_one(&format!("SELECT bson_decode({})", blob(&d)))
        .unwrap();
    assert_eq!(dec, canon);
}

#[test]
fn field_projection_and_type_of() {
    let d = db();
    materialize(&d);
    let f_s: String = d
        .query_one(&format!("SELECT bson_field({}, 's', '')", blob(&d)))
        .unwrap();
    assert_eq!(f_s, "hi");
    let f_ax: String = d
        .query_one(&format!("SELECT bson_field({}, 'a.x', '')", blob(&d)))
        .unwrap();
    assert_eq!(f_ax, "1");
    // Absent path -> NULL (not an error).
    let none: Option<String> = d
        .query_one(&format!("SELECT bson_field({}, 'no.such.path', '')", blob(&d)))
        .unwrap();
    assert!(none.is_none());
    // Top-level type_of with empty path -> 'object'.
    let top: String = d
        .query_one(&format!("SELECT bson_type_of({}, '')", blob(&d)))
        .unwrap();
    assert_eq!(top, "object");
    let t_arr: String = d
        .query_one(&format!("SELECT bson_type_of({}, 'a.arr')", blob(&d)))
        .unwrap();
    assert_eq!(t_arr, "array");
    let t_x: String = d
        .query_one(&format!("SELECT bson_type_of({}, 'a.x')", blob(&d)))
        .unwrap();
    assert_eq!(t_x, "int");
}

#[test]
fn objectid_hex_roundtrip() {
    let d = db();
    // A fixed 12-byte ObjectId (0x64636261605f4e3d2c1b0a09).
    let hex: String = d
        .query_one("SELECT bson_objectid_hex(from_hex('64636261605f4e3d2c1b0a09'))")
        .unwrap();
    assert_eq!(hex, "64636261605f4e3d2c1b0a09");
    let rt: String = d
        .query_one("SELECT to_hex(bson_objectid_from_hex('64636261605f4e3d2c1b0a09'))")
        .unwrap();
    assert_eq!(rt.to_uppercase(), "64636261605F4E3D2C1B0A09");
    // Bad hex (wrong length) -> NULL.
    let n: Option<String> = d
        .query_one("SELECT to_hex(bson_objectid_from_hex('abc'))")
        .unwrap();
    assert!(n.is_none());
    // Non-12-byte blob -> NULL.
    let n2: Option<String> = d
        .query_one("SELECT bson_objectid_hex(from_hex('0011'))")
        .unwrap();
    assert!(n2.is_none());
}

#[test]
fn null_and_malformed_flow_to_null() {
    let d = db();
    // Malformed BSON (a 0-length doc is not a valid document) -> NULL.
    let n: Option<String> = d
        .query_one("SELECT bson_decode(CAST(from_hex('00000000') AS BLOB))")
        .unwrap();
    assert!(n.is_none());
    // NULL blob -> NULL across the surface.
    let nb = "CAST(NULL AS BLOB)";
    for q in [
        format!("SELECT bson_to_json({nb}, 'canonical')"),
        format!("SELECT bson_decode({nb})"),
        format!("SELECT bson_is_valid({nb})"),
        format!("SELECT bson_well_formed({nb})"),
        format!("SELECT bson_field({nb}, 'a', '')"),
        format!("SELECT bson_type_of({nb}, 'a')"),
        format!("SELECT bson_objectid_hex({nb})"),
    ] {
        let v: Option<String> = d.query_one(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert!(v.is_none(), "{q} must be NULL");
    }
}

#[test]
fn bad_modes_are_errors() {
    let d = db();
    materialize(&d);
    let err = d
        .query_one::<String>(&format!(
            "SELECT bson_to_json({}, 'bogus')",
            blob(&d)
        ))
        .expect_err("unknown to_json mode must be a SQL error");
    assert!(err.to_string().to_lowercase().contains("mode"), "{err}");
    let err = d
        .query_one::<String>(&format!(
            "SELECT bson_field({}, 'a', 'nope')",
            blob(&d)
        ))
        .expect_err("unknown field coercion must be a SQL error");
    assert!(err.to_string().to_lowercase().contains("coercion"), "{err}");
}

#[test]
fn objectid_field_coercion() {
    let d = db();
    // A document with a real ObjectId field, built from Extended-JSON v2.
    d.execute_batch(
        "CREATE OR REPLACE TEMP TABLE o AS SELECT to_hex(bson_from_json('{\"o\":{\"$oid\":\"64636261605f4e3d2c1b0a09\"}}')) AS h",
    )
    .unwrap();
    let ob = "CAST(from_hex((SELECT h FROM o)) AS BLOB)";
    let hex: String = d
        .query_one(&format!("SELECT bson_field({ob}, 'o', 'objectid')"))
        .unwrap();
    assert_eq!(hex, "64636261605f4e3d2c1b0a09");
}
