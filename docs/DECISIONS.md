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

## D15 — Dashboard & observability (SPEC v1.1)

The following ambiguities in `DASHBOARD-SPEC.md` were resolved to the simplest
reasonable reading and are recorded here.

**HTTP server = hand-rolled on `std::net::TcpListener`.** The spec offers a
choice between a minimal crate (e.g. `tiny_http`) and raw `std::net`. Raw std is
the smallest thing that works, adds no dependency (nothing new to pin or let
Dependabot chase), and the server surface is tiny (one request line, four routes,
`Connection: close`). Charts are hand-drawn inline SVG as required.

**`.jsonl` files are excluded from indexing.** The spec says to ship
`test/fixture/metrics_sample.jsonl`, but the conformance harness indexes
`test/fixture/` and must keep producing exactly 7 chunks with a byte-identical
`conformance.json`. A `.jsonl` file is runtime log data, never source to be
chunked, so the walker now skips `.jsonl` (like it skips `.cce/`). This reconciles
both requirements: the fixture ships at the spec'd path and `conformance.json` is
unchanged. Verified byte-identical before and after.

**`--json` search output becomes an object.** DASHBOARD-SPEC §5 says to add a
top-level `query_id` field to `--json`. A "top-level field" implies an object, so
`cce search --json` now emits `{"query_id": "...", "results": [ ... ]}` instead of
a bare array. This is the documented v1.1 shape; `query_id` is `null` when metrics
are disabled.

**Feedback for an unknown id warns but still records (exit 0).** The spec allows
either behaviour. Recording anyway is the more forgiving choice — feedback is
cheap, ids are opaque, and a later re-index or log inspection can still make use
of it. A warning is printed to stderr.

**Metrics location derives from the store.** The log lives beside the index at
`<store-dir>/metrics.jsonl`. `--store PATH` points at the index file, so the log
is `PATH`'s parent directory plus `metrics.jsonl`; `--dir D` uses `D/.cce/`; and
`--metrics PATH` overrides both (dashboard/feedback only).

**`metrics.enabled` config key vs `--no-metrics`.** The base engine (SPEC v1.0)
ships no config-file loader — all configuration is compile-time constants — so
the runtime switch is the `--no-metrics` flag on `index`/`search`. A config-file
`metrics.enabled=false` is honoured in spirit by that flag; wiring an actual
config file is out of scope for v1.1 and would be a separate change.

**Delta direction uses the rounded window means.** `delta_ratio` /
`delta_top_score` are computed from the 6-decimal-rounded current/prior means, so
both language implementations reach an identical value and direction from the same
inputs (no last-ULP divergence). This matches the §4.1 anchor exactly.

## v2.0 — language packs (SPEC-V2)

**Only named AST nodes become chunks.** SPEC-V2 §1 says "for every node whose type
is in `function_types`/`class_types` emit a chunk". Some grammars name a
definition node the same string as its keyword token — e.g. tree-sitter-ruby's
`class` definition node and the anonymous `class` keyword both report
`node.kind() == "class"`. Emitting for every matching node would double-count the
class (one chunk for the keyword token at its single line). The chunker therefore
guards on `node.is_named()`, so only real AST nodes are candidates. This is
correct for every pack (all function/class definition nodes are named) and keeps
counts sane. Ruby needs it; the others are unaffected.

**A class node's references are emitted too (structural, not semantic).** A pack
declares node *types*, not predicates. So a C `struct Node *n` parameter — a
bodyless `struct_specifier` reference — is emitted as a class chunk just like the
`struct Node { … }` definition. This keeps packs declarative and identical across
languages; the cost is a few noisy reference chunks. Both implementations follow
the same rule, so conformance stays byte-identical.

**Conformance v2 drops the query section.** SPEC-V2 §7 permits keeping or dropping
the base query section and makes the chunk array the equivalence gate. We drop it:
the samples are a multi-language corpus for which the old Python-specific queries
(`"hash password"`, …) are meaningless, and the orchestrator diffs the chunk
arrays. `spec_version` is `"2.0"`, and each chunk gains `kind`.

**`spec_version` for conformance is a dedicated constant.** The persisted index
tag stays `SPEC_VERSION = "1.0"` (internal, not cross-checked); conformance emits
`CONFORMANCE_SPEC_VERSION = "2.0"` so both implementations agree on the v2 gate.

**The base v1 fixture moved to `test/fixture/base/`.** SPEC-V2 §6 places the
samples under `test/fixture/samples/`. Since the walker recurses, leaving the v1
fixture (`auth.py`, `payments.py`, `README.md`, `metrics_sample.jsonl`) at
`test/fixture/` would fold the samples into every base-fixture test. Relocating the
v1 fixture into a sibling `base/` keeps the two corpora independent; file paths
inside each stay root-relative and unchanged.

**Import extraction is per-pack and NOT part of conformance.** Imports feed only
the graph and each pack's own `expected.imports` self-test; they are absent from
`conformance.json`. So import rules need not agree byte-for-byte across languages,
only satisfy each pack's sample. Rust's optional `mod name;` import is therefore
omitted (the sample does not need it), keeping the rule to the first `use`-path
segment.

**`kind` on the dashboard.** SPEC-V2 §3 lists the dashboard among `kind`'s
surfaces. The dashboard aggregates query-level *metrics events*, which carry no
per-chunk data, so there is no natural per-chunk `kind` to show without changing
the event schema and its cross-language §4.1 anchor. `kind` is therefore surfaced
where chunks are surfaced — `search` (human + `--json`), `stats` (by-kind
breakdown), and conformance — and carried through persistence; the dashboard is
left byte-stable. Recorded here as the resolution of that ambiguity.

**Grammar crate versions.** July-2026 latest that share the pinned `tree-sitter`
core's `tree-sitter-language` 0.1.x ABI: `tree-sitter-ruby 0.23.1`,
`tree-sitter-rust 0.24.2`, `tree-sitter-typescript 0.23.2`, `tree-sitter-c 0.24.2`
(TypeScript exposes `LANGUAGE_TYPESCRIPT`; the pack binds that, not the TSX
variant). All pinned with `=` in `Cargo.toml`.

## v2.1 — secret & sensitive-file protection (SPEC-V2.1)

The following ambiguities in `SPEC-V2.1.md` were resolved to the simplest
reasonable reading and are recorded here.

**Generic pattern 10 never re-redacts an already-redacted value.** SPEC-V2.1 §1
runs the nine specific patterns, then the generic `key = value` assignment. Its
example — `token = "ghp_…"` → `token = "[REDACTED:GITHUB_TOKEN]"` (not
`[REDACTED:SECRET]`) — is only reachable if the generic step skips a value the
specific step already turned into `[REDACTED:…]`. So the placeholder guard also
treats a leading `[REDACTED:` as "leave alone". This is forced by the spec's own
worked example, keeps redaction idempotent, and preserves the more precise label.

**A leading-dot-only filename has no "extension".** SPEC-V2.1 §1 compares "the
file's final extension". For a name like `.env` or `.key` we follow OS convention
(`std::path::Path::extension`), which reports *no* extension for a leading-dot
name. `.env` is handled by the dotenv rule and `.pgpass`/`id_rsa`/… by the
exact-basename rule; a bare `.key` (a hidden file, not a `*.key` secret) is not
treated as sensitive. This avoids surprising matches while covering every case the
fixture and spec name.

**Layers are threaded through one builder, on by default.** `Index::build_protected`
takes a `protect_secrets` bool; `build_from_dir`/`build_from_dir_filtered` keep
their signatures and pass `true`, so every existing caller (conformance, bench,
tests) stays secure-by-default with no change. Only `cce index --allow-secrets`
passes `false`. `conformance` therefore runs with protection **on**; because the
samples contain no secrets it is a no-op there and `conformance.json` stays
byte-identical (re-verified).

**`--allow-secrets` scope = `index`.** SPEC-V2.1 §2 says the flag applies to
`index` "and any command that indexes". The only user-facing indexing command is
`index`; `conformance` and `bench` index deterministically over fixtures with no
secrets, so they need no opt-out and keep protection on. Adding the flag solely to
`index` satisfies the requirement without widening the surface.

**`regex` crate, pinned.** The redaction patterns need real regex (lazy `[\s\S]*?`
for the private-key block, a closure over the generic match). `regex = "=1.12.4"`
was already resolved transitively, so promoting it to a direct, `=`-pinned
dependency adds no new code to the tree.

## v2.2 — workspace mode (SPEC-V2.2)

The following `SPEC-V2.2.md` ambiguities were resolved to the simplest reasonable
reading.

**Federation is realised as a union corpus with member-namespaced paths.** The
spec defines a workspace search as *exactly* the §6 retrieval over the union of
members' chunks, with a `(member, file_path)` diversity key. Rather than fork the
retriever, `retriever::search` is split into `rank_core` (the §6 pipeline without
graph expansion) and a thin `search` wrapper; federation builds a combined `Index`
whose chunk paths are namespaced `<member>/<rel>` and calls the *same* `rank_core`.
Namespacing makes the diversity key naturally `(member, file_path)` and lets BM25
statistics span the union — so the "single index over A+B" equivalence holds by
construction, and the namespace is stripped for output.

**The combined graph is the union of per-member graphs, not a rebuild over the
union.** Building one import graph over all namespaced files could resolve a module
name in member A to a same-stemmed file in member B, inventing a cross-member file
edge the spec does not want. Instead each member's own intra-store graph is unioned
(namespaced) via `Graph::{out_pairs,from_pairs}`; the *only* cross-member links are
the declared dependency edges, applied by a separate member-level expansion step.

**The §8 fixture carries the engine/tsconfig markers the assertions require.** The
§8 sketch lists `billing` as a `ruby-engine` and `web` as `typescript`, but the §3
detection rules only yield those types given a `lib/**/engine.rb` (engine) and a
`tsconfig.json` (typescript). The shipped fixture therefore includes
`engines/billing/lib/billing/engine.rb` and `web/tsconfig.json` — the minimal
markers that make the normative detection produce the types §8 asserts.

**Workspace `[<dir>]` is an optional positional for `search`/`stats`/`dashboard`.**
`index --workspace [<dir>]` already had a positional dir; to match the spec's
`[<dir>]` notation on the other workspace commands (whose single-repo forms use
`--dir`/`--store`), an optional positional `DIR` was added, preferred over `--dir`
in workspace mode. Single-repo behaviour is untouched.

**Workspace search is read-only over member stores (no metrics write).** A
federated search reads the members' stores and logs but does not append its own
`search` event to any member's `metrics.jsonl` (which member would own it?). The
`--json` output still carries a top-level `query_id` for shape compatibility; it is
generated, not persisted. The federated dashboard aggregates each member's existing
per-member events.

**`serde_yaml` for reading, a hand-rolled writer for byte-determinism.** The
manifest is emitted by a small canonical writer so the exact bytes are under our
control (and match across languages); `serde_yaml = "=0.9.34"` parses hand-written
manifests back. Emitting via `serde_yaml` was avoided because its formatting is not
guaranteed stable across versions or identical to another language's YAML library.
