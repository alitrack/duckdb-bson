# duckdb-bson

**BSON / Extended-JSON v2, in SQL.** Decode a BSON document to MongoDB's
Extended JSON, build one back, validate it (with a classified error
taxonomy), project any dotted path with a type coercion, and read/write
ObjectIds — all in SQL, right where you query.

Built as a DuckDB loadable extension with the [quack-rs](https://crates.io/crates/quack-rs)
C-API binding layer. The engine (`bson_core/`) is a verbatim port of the
[Query-farm `vgi-bson` bson-core crate](https://github.com/Query-farm/vgi-bson)
(MIT) — a pure-Rust structural decoder over the official `bson` crate, with
**no Arrow, no VGI, and no `mongodb` driver**; only the transport layer was
replaced with direct C-API vector I/O.

## Install

Once accepted into [DuckDB's community extensions](https://github.com/duckdb/community-extensions):

```sql
INSTALL bson FROM community;
LOAD bson;
```

Until it lands in the community registry, build from source:

```bash
git clone https://github.com/alitrack/duckdb-bson && cd duckdb-bson
git submodule update --init --recursive
make release          # -> build/release/bson.duckdb_extension
```

```sql
LOAD 'path/to/bson.duckdb_extension';
```

## Functions (v1)

| Function | Signature | Returns |
|---|---|---|
| `bson_to_json` | `(doc BLOB, mode VARCHAR)` | VARCHAR — Extended-JSON v2 (`canonical` \| `relaxed`) |
| `bson_decode` | `(doc BLOB)` | VARCHAR — Extended-JSON v2 canonical |
| `bson_from_json` | `(json VARCHAR)` | BLOB — Extended-JSON v2 → BSON bytes |
| `bson_is_valid` | `(doc BLOB)` | BOOLEAN — single well-formed BSON document |
| `bson_well_formed` | `(doc BLOB)` | STRUCT(ok BOOLEAN, error VARCHAR, kind VARCHAR) |
| `bson_field` | `(doc BLOB, path VARCHAR, mode VARCHAR)` | VARCHAR — leaf at dotted path, coerced |
| `bson_type_of` | `(doc BLOB, path VARCHAR)` | VARCHAR — BSON type name at path (or top level) |
| `bson_objectid_hex` | `(oid BLOB)` | VARCHAR — 12-byte ObjectId → 24-char hex |
| `bson_objectid_from_hex` | `(hex VARCHAR)` | BLOB — 24-char hex → 12-byte ObjectId |

`mode` for `bson_to_json`: `canonical` (default) | `relaxed`
`mode` for `bson_field` coercion: `` (inference) | `objectid` | `decimal` |
`uuid` | `timestamp` | `datetime` | `blob` | `json`
`path` for `bson_type_of`/`bson_field`: dotted path, `''`/NULL = top level

## Try it

```sql
LOAD 'bson.duckdb_extension';

-- Build a BSON doc from Extended-JSON v2, then read it back.
CREATE VIEW d AS SELECT bson_from_json('{"a":{"x":1},"s":"hi","o":{"$oid":"64636261605f4e3d2c1b0a09"}}') AS b;

-- Canonical Extended-JSON v2 (integers wrapped as {"$numberInt": ...}).
SELECT bson_decode(b) FROM d;
-- Relaxed (human-readable: plain 1, "$dateISO" stays, "$oid" → {"$oid":...}).
SELECT bson_to_json(b, 'relaxed') FROM d;
-- Validate: total, per-row, never aborts.
SELECT bson_is_valid(b), bson_well_formed(b) FROM d;
-- Field projection (dotted path) + type.
SELECT bson_field(b, 'a.x', '') AS x,        -- 1
       bson_field(b, 's', '')   AS s,        -- hi
       bson_field(b, 'o', 'objectid') AS o,  -- 64636261605f4e3d2c1b0a09
       bson_type_of(b, '') AS top,           -- object
       bson_type_of(b, 'a') AS a_type;       -- object
-- ObjectId helpers.
SELECT bson_objectid_hex(CAST(from_hex('64636261605f4e3d2c1b0a09') AS BLOB));
SELECT to_hex(bson_objectid_from_hex('64636261605f4e3d2c1b0a09'));
```

## Semantics

- **NULL flows through**: NULL doc / NULL json → NULL result.
- **Malformed input → NULL**: every decode path funnels through a bounded
  structural validator (hard `MAX_NESTING` recursion limit, fully
  bounds-checked reads) **before** the `bson` crate is allowed to allocate a
  recursive value tree. A lying multi-gigabyte length prefix, a NUL-less
  cstring, a 5000-level-deep document → that row fails with a classified
  `kind` (`truncated`, `trailing_bytes`, `bad_length`, `nesting_depth`, …),
  it never OOMs or stack-overflows the worker.
- **Two loud errors** (caller mistakes, not dirty data): `bson_to_json` with an
  unknown `mode`, and `bson_field` with an unknown coercion `mode` → clean SQL
  error.
- **`bson_field` path** is dotted (`a.b.c`); empty/NULL path means the root.
  An absent path → NULL (not an error).

## Deferred (v2 — binding surface growth)

- `bson_keys` → `LIST<VARCHAR>` (top-level field names, document order)
- `bson_encode` (any DuckDB type → BSON)
- `objectid_timestamp` / `timestamp_to_ts` / `timestamp_parts` (TIMESTAMP returns)
- `mongodump_read`, `bson_seq` (table functions)

All deferred surfaces are available in the `bson_core/` engine; only the
quack-rs binding is outstanding.

## Build

```bash
cargo build --release
cargo run --bin append_metadata --release \
  -- target/release/libbson.so target/release/bson.duckdb_extension
cargo test --release --test integration   # compiles a bundled in-memory DuckDB
```

CI builds linux_amd64 / linux_arm64 / osx_amd64 / osx_arm64 / windows_amd64
via `extension-ci-tools`.

## Dependencies

| crate | role |
|---|---|
| `bson 3.1` (no `mongodb` driver) | official MongoDB BSON codec + dynamic value model |
| `serde_json` (`preserve_order`) | Extended-JSON v2, field-order preserving |
| `hex` | ObjectId hex encoding |

## License

MIT (engine ported from Query-farm vgi-bson, MIT).
