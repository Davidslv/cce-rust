# Decisions

Every ambiguity in `SPEC.md` resolved to the simplest reasonable interpretation
(SPEC §0 rule 4), with rationale.

## D1 — Fallback chunk line count
**Ambiguity:** SPEC §4.2 says a fallback `module` chunk's `end_line` is "number
of lines", which is ambiguous for trailing newlines.
**Decision:** `end_line = max(1, content.lines().count())` — Rust's `.lines()`
counts logical lines and ignores a single trailing newline, matching the
`splitlines()` semantics most implementations use. The fixture `README.md`
("# Demo\nPayment and authentication utilities.\n") yields `end_line = 2`, as
expected. Empty content yields 1.

## D2 — Persistence format
**Decision:** A single JSON file (`<store>/index.json`) via serde. Chosen for
determinism and debuggability over SQLite/bincode; corpora are small (SPEC §1.2).
Embeddings are stored inline so `search` needs no re-embedding of the corpus.

## D3 — Re-index is a full rebuild
**Ambiguity:** SPEC §7 requires idempotent re-indexing that "replaces prior data
for changed/removed files."
**Decision:** `index` rebuilds the whole store each run. Because chunk IDs are
deterministic, a full rebuild is idempotent and trivially handles changed/removed
files. Incremental indexing was out of scope for the benefit.

## D4 — Query intent phrase heuristics
**Ambiguity:** SPEC §6.1 lists regex-ish triggers `where is `, `find .* function`,
`.* defined`.
**Decision:** Implemented as: contains `"where is "`; contains `"defined"`;
contains `"find"` with `"function"` occurring after it. The keyword set
{function, class, method, def} is matched as whole tokens via the shared
tokenizer. None of the three conformance queries trigger CODE_LOOKUP, so this
interpretation does not affect the conformance gate.

## D5 — File hints for keyword_distance
**Ambiguity:** SPEC §6.5 mentions "file-hint" extraction but permits treating it
as none.
**Decision:** File hints = whitespace-split query terms containing a `.`
(lowercased), checked as substrings of `file_path`. The substring-of-content rule
still applies. Conformance queries contain no dotted terms, so this is inert
there.

## D6 — Lowercasing for substring checks
**Decision:** `keyword_distance` lowercases chunk content and file paths with
ASCII case-folding (`to_ascii_lowercase`), consistent with the tokenizer's
ASCII-only lowercasing (SPEC §4.1). Query tokens are already ASCII-lowercased.

## D7 — Empty / whitespace query
**Ambiguity:** SPEC §9 says invalid/empty inputs must not crash.
**Decision:** A query that tokenizes to nothing (empty or all-separator) returns
an empty result list rather than ranking by a zero vector.

## D8 — `cce bench` indexes Python sources only
**Ambiguity:** SPEC §10.1 says "Index the repo's Python sources," while `cce index`
indexes all text files.
**Decision:** `cce bench` filters the walk to `*.py` (via
`Index::build_from_dir_filtered`). This matches the spec's wording and keeps the
recall/token-savings numbers comparable to the Ruby implementation. General
`cce index` still indexes everything (module fallback for non-parsed files).

## D9 — JS import specifier segmentation
**Ambiguity:** SPEC §4.2 gives `"react" → react` and `"./auth" → auth`.
**Decision:** Split the specifier on `/`, drop empty / `.` / `..` segments, take
the first remaining. Scoped packages (`@scope/pkg`) therefore resolve to
`@scope`; an acceptable edge case not exercised by the spec.

## D10 — Ollama backend at search time
**Ambiguity:** `cce search` has no `--embedder` flag (SPEC §9) but ollama vectors
are model-specific.
**Decision:** The store records which embedder built it. `search` uses the same
backend; if the index used ollama and the server is now unreachable, it warns and
embeds the query with the hash embedder (degraded but non-fatal). Hash indexes
(the default and conformance path) always use the hash embedder.

## D11 — Coverage tool
**Decision:** `cargo llvm-cov` for line/region coverage. CLI wiring (`main.rs`)
and the Ollama HTTP paths (network) are intentionally the least-covered; core
non-CLI logic exceeds the ≥85% target (SPEC §12).

## D12 — Conformance JSON layout
**Decision:** `serde` `#[derive(Serialize)]` structs with fields declared in the
exact order shown in the SPEC §8.3 example, emitted with `to_string_pretty`.
serde serializes struct fields in declaration order, giving a deterministic,
spec-shaped layout every run (verified byte-identical across two runs).

## D13 — Coverage-hardening pass (86.95% → 95.33% lines)
**Decision:** Raised line coverage well above target by adding meaningful,
hermetic tests only — no behavior changed and `conformance.json` is byte-identical
to before. The gaps closed were the CLI wiring in `main.rs` (driven end-to-end
through the built binary in `tests/cli.rs`: default store-path resolution, human
vs JSON output, empty-query "(no results)", `--embedder ollama` fallback,
empty/missing-store `stats`, and `bench`/`conformance` against a tiny local temp
repo — never the flask corpus), plus `config`/`store` edge functions.
**Ollama graceful-failure path:** exercised **without ever contacting a real
server** by pointing `OllamaEmbedder` at a closed local port (`127.0.0.1:1`,
instant connection-refused). This covers `try_embed_batch`'s error branch,
`healthy() == false`, and the empty-vector fallbacks in `embed`/`embed_batch`.
The Ollama HTTP **success** path (response parsing) remains uncovered by design —
it requires a live model server and is out of scope for the hermetic suite, so
`embedder.rs` is intentionally the last file below 100%.

## D14 — Reconstructed `rustfmt.toml`
**Ambiguity:** This clean-room package shipped without a `rustfmt.toml`, yet the
committed sources use a compact style (wide single-line calls/struct literals,
if/else wrapping ~60 cols) and a logical — not alphabetical — module order. Under
stock rustfmt 1.8.0 the tree is therefore not `cargo fmt --check`-clean, so the
formatting gate cannot pass as delivered.
**Decision:** Restored a minimal `rustfmt.toml`
(`reorder_imports=false`, `reorder_modules=false`, `use_small_heuristics="Max"`,
`single_line_if_else_max_width=60`) that best reproduces the intended house style,
then ran `cargo fmt`. This makes `cargo fmt --check` genuinely clean while
preserving the compact style and module ordering rather than blowing them away to
stock defaults. Formatting-only; no logic or spec behavior changed.
