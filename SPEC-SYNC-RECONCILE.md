# CCE Sync — artifact reconciliation (v2.3 final canonical format)

The two engines diverged on the interchange artifact, so cross-engine
byte-identity (SPEC-SYNC §10 — a hard requirement) does not hold. This pins the
**single canonical format**. Align your artifact to it EXACTLY. Keep everything
else (git remote, LFS, CLI, offline-first) unchanged.

## Canonical artifact (byte-exact)

A UTF-8 stream, **LF (`\n`) after every line, including the last**. All JSON is
**compact** (no insignificant whitespace) with **keys sorted lexicographically**.

**Line 1 — manifest**, exactly these keys (sorted): 
`cce_version` (string, `"2.3"`) · `checksum` (see below) · `chunk_count` (int) ·
`embedder` (string, `"hash"`) · `file_tokens` (object `{"<path>": <int>}`, keys
sorted) · `pack_set_id` (string) · `repo_id` (string) · `sha` (string).
**No `built_at`, no `built_by`, no other keys.** (Provenance is REMOVED entirely
— it is what made the file non-reproducible.)

**Then one line per chunk**, chunks sorted by `(file_path, start_line, id)`, each
a compact sorted-key object with **exactly** these keys:
`chunk_type` · `content` · `embedding` · `end_line` · `file_path` · `id` ·
`kind` · `language` · `start_line` · `token_count`.
(INCLUDE `language`.) `embedding` = standard **base64 with padding, no newlines**
of the 256 little-endian IEEE-754 `f64` values (2048 bytes → base64).

**Last line — graph**, a compact sorted-key object `{"edges":[...],"nodes":[...]}`
using the node/edge fields you already store, with **nodes sorted by `id`** and
**edges sorted by `(source, target, type)`** (or your edge fields), and object
keys sorted. Deterministic ordering is mandatory.

**`pack_set_id`** = the sorted, comma-joined lowercase pack names
(`c,javascript,python,ruby,rust,typescript`).

## Checksum rule (identical both engines)

`checksum` = lowercase-hex SHA-256 over the ENTIRE canonical stream built with the
manifest's `checksum` value set to the empty string `""`. I.e. serialize with
`checksum:""`, hash the whole stream, then write the real hex into the `checksum`
field of the final artifact. Verify = read artifact, set `checksum` to `""`,
re-hash, compare. There is no provenance to special-case.

## Shared golden (both MUST reproduce identically)

Index `test/fixture/samples` (byte-identical in both repos), then build the
artifact with **`repo_id = "cce/demo"`** and
**`sha = "0000000000000000000000000000000000000000"`** (force these — do not use a
real git sha; add a test-only path / CLI override if needed).

Do two things:
1. Update your golden test to assert the resulting **checksum** on this input.
2. When that golden test runs, ALSO write the raw artifact bytes to
   `/tmp/cce_artifact_ruby.cce` (Ruby) or `/tmp/cce_artifact_rust.cce` (Rust), so
   the orchestrator can `diff` the two files byte-for-byte.

The two files MUST end up byte-identical and the two checksums MUST be equal.

## Gates (unchanged)

Keep the full suite green (Ruby `rake test` ≥93%; Rust `test`+`clippy`+`fmt`
≥92%), single-repo `conformance.json` byte-identical, offline-first intact. Do
NOT push. Report your golden checksum, the emitted `/tmp/...` path, and that gates
are green.
