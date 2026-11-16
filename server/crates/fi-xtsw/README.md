# fi-xtsw

The `bin/xtsw` codec of Forza Horizon 2's Xls web services: SER-3.1 in
[milestones/server.md](../../docs/milestones/server.md), step 2 of
[SER-M3_PORT_MAP.md](../../docs/server/SER-M3_PORT_MAP.md) §6. The game-plane catch-all (step 3) and
the ported handlers (steps 4 to 9) build on it. Library only: no I/O, no clock, no logging.

The game posts `bin/xtsw` bodies to `.../o.xtsw` with the class in `X-ClassName`
(`Forza.WebServices.User, Forza.WebServices`) and the method in `X-Method`. The request body is the
method's `_Parameters` struct; the reply is a 12-byte header `[status][code][message length]`
(big-endian u32) followed, on success (status 0), by the `_Results` struct, or, on error (status 2,
HRESULT `0x80510000 | code`), by the message.

| Item | What |
| --- | --- |
| `Schema::fh2()` | The game's schema (game data `fh2/xtsw-schema.json`, `FI_GAME_DATA_DIR`, parsed once): 30 classes, 201 methods, 571 types |
| `Schema::from_json` | Any schema in the extractor's shape; every type reference must resolve |
| `Schema::method`, `type_def` | Class (with or without the assembly) + method → `_Parameters` / `_Results` type; type descriptors |
| `Value` | One variant per wire kind (integer widths, `f32` against `f64`, NaN bits, guid bytes, blob bytes, datetime strings); `to_json()`, `into_json()` (consuming: a large body is not held twice) |
| `Schema::value_from_json` | JSON → `Value`, lenient like the Beta's handlers (missing fields are defaults, hex or `0x` integers, -1 for unsigned) |
| `decode_params` / `encode_params` | Request bodies, `Limits::PARAMS` (blobs up to 32 MiB: uploads) |
| `decode_results` / `encode_results` | Reply bodies, `Limits::RESULTS` (what the game's reader accepts: blobs 0x80000, datetimes `valid_datetime`) |
| `valid_datetime` | The game reader's datetime format, `YYYY-MM-DD[THH:MM:SS[.mmm]][Z\|±hh:mm]`, with calendar and range checks (an invalid one fails the whole reply in the game) |
| `ok_envelope`, `error_envelope`, `parse_envelope` | The 12-byte reply header |
| `truncate_utf16` | Cuts a string to a field's `max_chars` (encoding refuses longer strings) |

Encoders take a `Value` (strict: kinds must match, every field present) or a `serde_json::Value`.

## Guarantees

- `encode(decode(bytes)) == bytes` for every body the decoder accepts, by the typed path and by the
  JSON path (except NaN payloads, which JSON carries as `"NaN"`).
- Hostile input never panics: lengths and counts are checked against the remaining input, the
  limits (strings 0x2000 bytes, arrays 0x2000 and the field's capacity, blobs 0x80000 in replies,
  32 MiB in requests, strings within `max_chars` UTF-16 units) and nesting against `MAX_DEPTH`
  before anything is allocated; bodies with trailing bytes, invalid UTF-8 or bool bytes above 1 are
  refused.
- Errors (`XtswError`) carry type and field paths, offsets, lengths and kinds, never decoded values.
- Kinds 0x08, 0x0b and 0x0f (Live, Thunderhead and Xbox Music types: 21 methods of 5 classes) are
  `Kind::Unsupported`; decoding or encoding them fails with `UnsupportedKind`. The game reaches
  those services through the Xbox Live JSON hosts instead.

Differences from the Beta's `server/src/xtsw.rs`, on purpose: strings must be UTF-8 (the Beta
replaced bad bytes) and fit `max_chars` (the Beta truncated replies and logged); encoding a value
of the wrong JSON kind fails (the Beta wrote `""` or `0`); integers outside the signed and unsigned
range of the field fail (the Beta wrapped); bool bytes above 1 are refused; array counts and blob
lengths are checked against the field's capacity when decoding too; a method without
`_Parameters` decodes to `{}` (the Beta recorded `null`).

## Sources and OD-32

The schema is not in the crate (T-1115): it is the game data file `fh2/xtsw-schema.json` (and
`fm5/xtsw-schema.json`) of the directory `FI_GAME_DATA_DIR` names, checked against the directory's
`manifest.json` and parsed once (crate `fi-gamedata`). `tools/gamedata` rebuilds it from the
operator's own `forza_durango_release_final.exe`. The official copy (SHA-256
`3229363bca2a0fcaf2534393d9ce0ee7bb2cea40238f1b852eb759fe69ca72fb`) is in this private repository's
`gamedata/`, never exported. The format
notes are the Beta's `notes/local-docs/xtsw-format.md`; the reference codec is
`deprecated/beta/server/src/xtsw.rs` (never built).

## Tests

```sh
cargo test -p fi-xtsw                     # schema, synthetic round-trips, limits, envelopes, generated values
FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-xtsw --test golden -- --ignored --nocapture
```

The golden test reads the Beta server's capture files (`server/captures/*.jsonl`) at run time.
They hold real player identities: they stay outside the repository, and the test prints only
counts and error strings. For every Forza RPC it checks that the request decodes and re-encodes
byte for byte (from the `Value`, from its JSON and from the Beta's recorded JSON), and that each
`bin/xtsw` reply re-encodes byte for byte (success envelopes from the `Value` and its JSON, error
envelopes from their code and message). On 2026-10-18, two capture files (2750 RPCs of 41
methods, 2693 success and 17 error replies) all round-tripped.
