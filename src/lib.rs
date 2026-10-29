// DuckDB loadable extension: BSON / Extended-JSON v2 codec, validation,
// field projection with type coercion, and ObjectId helpers.
//
// Thin quack-rs binding layer over `bson_core` (verbatim from Query-farm's
// vgi-bson `bson-core` crate, MIT — a pure-Rust structural decoder with no
// Arrow/VGI dependency; only the transport layer was replaced with direct
// C-API vector I/O).
//
// Function surface (v1 — flat VARCHAR/BLOB/BOOLEAN/STRUCT returns):
//
//   bson_to_json(doc, mode)         -> VARCHAR  Extended-JSON v2 (canonical|relaxed)
//   bson_decode(doc)                -> VARCHAR  Extended-JSON v2 canonical
//   bson_from_json(json)            -> BLOB     Extended-JSON v2 -> BSON bytes
//   bson_is_valid(doc)              -> BOOLEAN  single well-formed BSON document
//   bson_well_formed(doc)           -> STRUCT(ok BOOLEAN, error VARCHAR, kind VARCHAR)
//   bson_field(doc, path, mode)     -> VARCHAR  leaf at dotted path, coerced
//   bson_type_of(doc, path)         -> VARCHAR  BSON type name at path (or top level)
//   bson_objectid_hex(oid)          -> VARCHAR  12-byte ObjectId -> 24-char hex
//   bson_objectid_from_hex(hex)     -> BLOB     24-char hex -> 12-byte ObjectId
//
// Semantics (inherited from vgi-bson):
//   * NULL doc / NULL json -> NULL result (missing data flows through).
//   * Malformed input -> NULL (decoders are total and bound recursion before
//     the bson crate allocates a tree; a hostile blob fails its own row, never
//     crashes the scan). The one exception: `bson_field` with an unknown
//     coercion `mode` is a caller mistake -> clean SQL error.
//   * `mode`/`path` are regular arguments: an empty/NULL string means the
//     default (mode -> Infer/Canonical, path -> top level). DuckDB broadcasts
//     a literal into the vector, so `bson_field(doc, 'a.b', '')` works.
//
// Deferred (documented in README): `bson_keys` (LIST<VARCHAR>),
// `bson_encode` (any DuckDB type -> BSON), `objectid_timestamp` /
// `timestamp_to_ts` / `timestamp_parts` (TIMESTAMP returns), `mongodump_read`
// + `bson_seq` (table functions).

use quack_rs::connection::{Connection, Registrar};
use quack_rs::error::ExtensionError;
use quack_rs::scalar::{ScalarFunctionBuilder, ScalarFunctionInfo};
use quack_rs::types::{LogicalType, TypeId};
use quack_rs::vector::{StructWriter, VectorReader, VectorWriter};

use bson_core::extjson::{self, JsonMode};
use bson_core::field::Coerce;
use bson_core::objectid;
use bson_core::typeinfo;
use bson_core::validate;

const WELL_FORMED_FIELDS: &[(&str, TypeId)] = &[
    ("ok", TypeId::Boolean),
    ("error", TypeId::Varchar),
    ("kind", TypeId::Varchar),
];

// ─────────────────────────────────────────────────────────────────────────────
// bson_to_json(doc, mode) -> VARCHAR
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_to_json_fn, |info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let mode = VectorReader::new(input, 1);
        let row_count = doc.row_count();
        let mut writer = VectorWriter::new(output);
        let fn_info = ScalarFunctionInfo::new(info);
        for row in 0..row_count {
            if !doc.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            let jm = if mode.is_valid(row) {
                match JsonMode::parse(mode.read_str(row)) {
                    Ok(m) => m,
                    Err(e) => {
                        fn_info.set_error(&e);
                        return;
                    }
                }
            } else {
                JsonMode::Canonical
            };
            match extjson::to_json(doc.read_blob(row), jm) {
                Ok(s) => writer.write_varchar(row, &s),
                Err(_) => writer.set_null(row), // malformed -> NULL
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_decode(doc) -> VARCHAR   (Extended-JSON v2 canonical)
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_decode_fn, |_info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let row_count = doc.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !doc.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match extjson::to_json(doc.read_blob(row), JsonMode::Canonical) {
                Ok(s) => writer.write_varchar(row, &s),
                Err(_) => writer.set_null(row),
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_from_json(json) -> BLOB
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_from_json_fn, |_info, input, output| {
    unsafe {
        let json_col = VectorReader::new(input, 0);
        let row_count = json_col.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !json_col.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match extjson::from_json(json_col.read_str(row)) {
                Ok(b) => writer.write_blob(row, &b),
                Err(_) => writer.set_null(row), // malformed JSON -> NULL
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_is_valid(doc) -> BOOLEAN
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_is_valid_fn, |_info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let row_count = doc.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !doc.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            writer.write_bool(row, validate::is_valid(doc.read_blob(row)));
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_well_formed(doc) -> STRUCT(ok BOOLEAN, error VARCHAR, kind VARCHAR)
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_well_formed_fn, |_info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let row_count = doc.row_count();
        let mut sw = StructWriter::new(output, WELL_FORMED_FIELDS.len());
        for row in 0..row_count {
            if !doc.is_valid(row) {
                sw.set_row_null(row);
                continue;
            }
            let wf = validate::well_formed(doc.read_blob(row));
            sw.write_bool(row, 0, wf.ok);
            match &wf.error {
                Some(e) => sw.write_varchar(row, 1, e),
                None => sw.set_null(row, 1),
            }
            match &wf.kind {
                Some(k) => sw.write_varchar(row, 2, k),
                None => sw.set_null(row, 2),
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_field(doc, path, mode) -> VARCHAR
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_field_fn, |info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let path = VectorReader::new(input, 1);
        let mode = VectorReader::new(input, 2);
        let row_count = doc.row_count();
        let mut writer = VectorWriter::new(output);
        let fn_info = ScalarFunctionInfo::new(info);
        for row in 0..row_count {
            if !doc.is_valid(row) || !path.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            let coerce = if mode.is_valid(row) {
                match Coerce::parse(mode.read_str(row)) {
                    Ok(c) => c,
                    Err(e) => {
                        fn_info.set_error(&e);
                        return;
                    }
                }
            } else {
                Coerce::Infer
            };
            match bson_core::field::field(doc.read_blob(row), path.read_str(row), coerce) {
                Ok(Some(s)) => writer.write_varchar(row, &s),
                Ok(None) => writer.set_null(row), // path absent -> NULL
                Err(_) => writer.set_null(row), // malformed -> NULL
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_type_of(doc, path) -> VARCHAR
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_type_of_fn, |_info, input, output| {
    unsafe {
        let doc = VectorReader::new(input, 0);
        let path = VectorReader::new(input, 1);
        let row_count = doc.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !doc.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            let p = path.is_valid(row) && !path.read_str(row).is_empty();
            let p = p.then(|| path.read_str(row));
            match typeinfo::type_of(doc.read_blob(row), p) {
                Ok(Some(s)) => writer.write_varchar(row, &s),
                _ => writer.set_null(row), // absent path / malformed -> NULL
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_objectid_hex(oid) -> VARCHAR
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_objectid_hex_fn, |_info, input, output| {
    unsafe {
        let oid = VectorReader::new(input, 0);
        let row_count = oid.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !oid.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match objectid::objectid_hex(oid.read_blob(row)) {
                Ok(s) => writer.write_varchar(row, &s),
                Err(_) => writer.set_null(row),
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// bson_objectid_from_hex(hex) -> BLOB
// ─────────────────────────────────────────────────────────────────────────────

quack_rs::scalar_callback!(bson_objectid_from_hex_fn, |_info, input, output| {
    unsafe {
        let hex = VectorReader::new(input, 0);
        let row_count = hex.row_count();
        let mut writer = VectorWriter::new(output);
        for row in 0..row_count {
            if !hex.is_valid(row) {
                writer.set_null(row);
                continue;
            }
            match objectid::objectid_from_hex(hex.read_str(row)) {
                Ok(b) => writer.write_blob(row, &b),
                Err(_) => writer.set_null(row),
            }
        }
    }
});

// ─────────────────────────────────────────────────────────────────────────────
// Registration + entry point
// ─────────────────────────────────────────────────────────────────────────────

unsafe fn register_all(con: &Connection) -> Result<(), ExtensionError> {
    con.register_scalar(
        ScalarFunctionBuilder::new("bson_to_json")
            .param(TypeId::Blob)
            .param(TypeId::Varchar)
            .returns(TypeId::Varchar)
            .function(bson_to_json_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_decode")
            .param(TypeId::Blob)
            .returns(TypeId::Varchar)
            .function(bson_decode_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_from_json")
            .param(TypeId::Varchar)
            .returns(TypeId::Blob)
            .function(bson_from_json_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_is_valid")
            .param(TypeId::Blob)
            .returns(TypeId::Boolean)
            .function(bson_is_valid_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_well_formed")
            .param(TypeId::Blob)
            .returns_logical(LogicalType::struct_type(WELL_FORMED_FIELDS))
            .function(bson_well_formed_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_field")
            .param(TypeId::Blob)
            .param(TypeId::Varchar)
            .param(TypeId::Varchar)
            .returns(TypeId::Varchar)
            .function(bson_field_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_type_of")
            .param(TypeId::Blob)
            .param(TypeId::Varchar)
            .returns(TypeId::Varchar)
            .function(bson_type_of_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_objectid_hex")
            .param(TypeId::Blob)
            .returns(TypeId::Varchar)
            .function(bson_objectid_hex_fn),
    )?;

    con.register_scalar(
        ScalarFunctionBuilder::new("bson_objectid_from_hex")
            .param(TypeId::Varchar)
            .returns(TypeId::Blob)
            .function(bson_objectid_from_hex_fn),
    )?;

    Ok(())
}

quack_rs::entry_point_v2!(bson_init_c_api, |con| register_all(con));
