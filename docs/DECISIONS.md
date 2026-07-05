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

## CCE Sync (v2.3.0, SPEC-SYNC + SPEC-SYNC-RECONCILE)

The two engines first diverged on the interchange artifact, so
[`SPEC-SYNC-RECONCILE.md`](../SPEC-SYNC-RECONCILE.md) pinned a single canonical
format. The decisions below reflect that reconciled format.

**The interchange artifact is a hand-built canonical stream, not serde's default.**
Cross-engine byte-identity (SPEC-SYNC §10) is a hard requirement. The artifact is
assembled explicitly: the manifest line, one object per chunk (sorted by
`(file_path, start_line, id)`), then the graph line — each an **LF-terminated**
compact JSON object, **including the last line**. Sorted keys come free from
`serde_json`'s default `Map` (a `BTreeMap`) plus `to_string` (no whitespace); the
`preserve_order` feature is deliberately not enabled. The stream is a pure function
of its content.

**Provenance is REMOVED entirely — no `built_at`, no `built_by`.** An earlier draft
carried a git-derived `built_at` and a neutral `built_by`; the reconciliation
dropped both. Any provenance risks non-reproducibility and there is no need for it —
the checksum plus `(repo_id, sha)` already identify the artifact. Removing it makes
the manifest exactly `{cce_version, checksum, chunk_count, embedder, file_tokens,
pack_set_id, repo_id, sha}` (sorted), which both engines produce identically.

**The checksum covers the whole stream with `checksum:""`.** `checksum` =
lowercase-hex SHA-256 over the entire canonical stream serialized with the
manifest's `checksum` field set to the empty string; the real hex is then written
in. Verify sets `checksum` to `""`, re-hashes, and compares. This is simpler than
the earlier "omit the field" rule and identical across engines (there is no
provenance to special-case). Independently reproduced with a standalone Python
SHA-256 over the emitted bytes.

**Embeddings are standard base64 (with padding) of little-endian `f64` bytes, never
decimals.** Float→string formatting differs across languages, so serializing the
256-d vector as decimals would break byte-identity even though the vectors are
bit-equal (the hash embedder is deterministic). Encoding the raw IEEE-754 bytes
(`f64::to_le_bytes`) as RFC-4648 base64 **with padding** sidesteps formatting
entirely. `base64 = "=0.22.1"` is pinned; the standard alphabet + padding is
identical across engines (2048 bytes → 2732 base64 chars).

**`file_tokens` lives in the MANIFEST (not the graph line).** The dashboard's
baseline-tokens counterfactual (DASH §3) needs each file's whole-file token count,
which cannot be recomputed from chunks after import. It is fully deterministic
(`max(1, bytes/4)`, a SPEC §3 constant both engines share). The canonical format
places it in the manifest as a sorted-key `{path: int}` object, keeping the
export→import round-trip lossless.

**The graph line is `{"edges":[…],"nodes":[…]}` over the RESOLVED imports (base SPEC
§6.7).** `nodes` are every indexed file (`{"id": path}`, sorted by id — derived from
`file_tokens`' keys); `edges` are the **resolved** `file → file` edges
(`{"source", "target", "type":"import"}`, sorted by `(source, target, type)`): an
edge `A → B` exists only when a module imported by `A` resolves — by the same
stem-matching the retriever's graph expansion uses — to a corpus file `B`. External /
unresolved imports (`os`, `fs`, `std`, …) produce **no** edge, so `samples` yields
`edges:[]`. On import, `file_imports` is reconstructed by mapping each resolved
`target` back to a module name (its file stem) and grouping by `source`; re-`build`ing
the graph over those stems reproduces the identical file→file edges, so
search-expansion behaviour is preserved (the dropped external imports never produced
a hop). An earlier draft emitted the raw `file → module` edges; the reconciliation
pinned resolved file→file to match Ruby byte-for-byte.

**`pack_set_id` is the literal sorted, comma-joined pack names.** Not a hash: the
reconciled format uses the string `c,javascript,python,ruby,rust,typescript`
verbatim, so it is human-legible and trivially identical across engines (both
register the same six packs).

**A per-branch ref pointer implements `latest`; content is addressed by sha.**
Distinct shas are distinct files, so they never conflict in content. To resolve
"latest main," `push` also writes `…/<repo_id>/refs/<branch>` = sha in the same
commit, and `pull --latest` reads it. Only ref advancement can race; the git backend
handles it with fetch-rebase-retry (proven by a two-clone race test).

**`git` is invoked via `std::process`, not a git library.** The remote is "just a
git repo," and shelling out keeps CCE dependency-light and uses the user's real git
credentials/transport for free. Commits carry a fixed identity (`-c user.name/email`)
so they work in a bare CI/test environment.

**git-LFS is default-on but never required by the core.** `sync init` writes the
`*.cce` LFS `.gitattributes` and runs `git lfs install` when LFS is on. But the
whole test suite exercises artifact/push/pull/verify over **plain git**, so it needs
no `git-lfs` binary; LFS lives behind one smoke test that SKIPS gracefully when
`git-lfs` is absent (SPEC-SYNC §11).

**The working-clone home is `$CCE_HOME/sync` (or `~/.cce/sync`), overridable for
tests.** Hermetic tests point `CCE_HOME` at a temp dir so a working clone never
touches the real `~/.cce`. Because `CCE_HOME` is process-global and Cargo runs tests
in parallel threads, the sync tests serialize env access through one shared mutex.

**The shared golden is on `test/fixture/samples` with a forced identity.** The
cross-engine anchor indexes `samples` and builds the artifact with
`repo_id = "cce/demo"`, `sha = "0"*40`. The test asserts the checksum
(`581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48`, confirmed equal
to Ruby's) and writes the raw bytes to `/tmp/cce_artifact_rust.cce` so the
orchestrator can `diff` it against Ruby byte-for-byte.

**The branch-overlay for WIP is deferred (v1 fallback = full local index).** If the
working tree differs from the pulled sha, `pull` says so and the user runs a normal
`cce index`. The incremental "reindex only changed files on top of the pulled base"
is a documented fast-follow, out of scope for v1 (SPEC-SYNC §7/§12).

## CCE MCP (v2.4.0, SPEC-MCP)

**Hand-rolled JSON-RPC 2.0 over stdio, no MCP SDK crate.** The MCP stdio transport is
newline-delimited JSON-RPC 2.0. Rather than add an unvetted, larger MCP SDK, the
server hand-rolls exactly the slice it needs with `serde_json` (already a dependency)
— the same choice the rest of the engine makes for its hand-rolled HTTP/YAML writers.
This keeps every wire byte under our control, the dependency set pinned and minimal,
and the protocol trivially testable by piping strings. `src/mcp/protocol.rs` owns
request parsing and success/error encoding; `src/mcp/server.rs` owns the dispatch.

**Protocol version pinned to `2025-06-18`.** The server advertises this MCP revision
in `initialize` and both engines pin the same value, so an agent negotiates an
identical protocol regardless of backend. The dispatch loop is transport-generic
(`run<R: BufRead, W: Write>`), so unit tests drive it in-process and the integration
suite pipes JSON-RPC to the real binary's stdin.

**The dispatch loop is transport-generic; `serve()` wires it to process stdio.** This
separation is what makes the server hermetically testable: `handle_line` is pure
(string in, optional string out), `run` loops over any reader/writer, and only
`serve` touches `std::io::stdin/stdout` (after the best-effort sync warm).

**A missing index / unknown tool is a *tool result*, not a protocol error.**
`context_search` over an unbuilt index returns a friendly "run `cce index`" text with
`isError: false` (it is a normal state, not a failure); an unknown tool name returns
`isError: true`. Only a malformed call (no `query`, no `helpful`, no tool `name`) is a
real error. This keeps an agent's session alive and steers it, rather than crashing.

**`context_search` reuses the CLI's exact retrieval and logs an identical metrics
event.** `retriever::build_search_record` was lifted out of `main.rs` into the library
so the CLI `search` and the MCP tool emit a byte-identical `cce.metrics/v1` event —
that identity is what lets `cce dashboard` surface agent usage the same way it
surfaces CLI use. The MCP default `top_k` is **8** (tighter than the CLI's 10) because
an agent pays per token. Workspace metrics land in the workspace-root log so the root
`cce dashboard --workspace` sees them.

**`cce init` merges idempotently by owning a stable key/marker.** `.mcp.json` keeps
its `mcpServers.cce` entry (other servers preserved); `CLAUDE.md` keeps a single
`<!-- BEGIN CCE MCP --> … <!-- END CCE MCP -->` block whose region is replaced in
place. Re-running produces byte-identical files. Workspace detection is
`.cce/workspace.yml` exists → the server args become `["mcp", "--workspace"]`.

**Sync is a soft dependency, gated on config.** On startup `cce mcp` warms the index
via `sync pull --latest` only when a remote is configured **and** `sync.auto_pull` is
on, with `force = false` (never clobber a WIP local cache) and every error swallowed
— offline, no-remote, cache-miss, and sha-mismatch all fall back silently to the
local index. `index_status` freshness (source/sha/behind-remote) is a new
`sync::commands::freshness` that touches the network only when a remote is configured.
MCP works fully with no Sync present.

**The sync artifact format version is decoupled from the app version.** The artifact
format did **not** change in v2.4 (CCE MCP is purely additive), so its compatibility
version must not move with the release. The old `cce_version_minor()` derived it from
the crate version, which would have made every release invalidate everyone's cache and
diverge from Ruby. It is replaced by a dedicated constant `SYNC_FORMAT_VERSION = "2.3"`
that names the *artifact format*, used everywhere the sync layer stamps the version
(the content address `hash/2.3/…` and the manifest `cce_version` field). It moves only
when the artifact bytes actually change shape — then both engines bump it in lockstep.
The shared golden checksum on `test/fixture/samples` therefore stays
`581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48`, equal to Ruby's.
The app/crate version is 2.4.1 (`Cargo.toml`, `CITATION.cff`) — the v2.4.1 dashboard
refresh + docs sweep is additive and does **not** touch `SYNC_FORMAT_VERSION`, so the
golden checksum above is unchanged; `conformance.json` is independent of both and stays
byte-identical.

## v2.4.1 — dashboard refresh & offline-first docs sweep

**The metrics schema grows only by adding fields.** The reader already tolerated
unknown/absent fields, so v2.4.1 extends it in place rather than versioning it:
`search` events gain `source`, `index` events gain `sha`/`source`/`sensitive_skipped`.
A pre-v2.4.1 log still parses — a search with no `source` normalises to `"cli"`, an
index event to `"local"`. No `cce.metrics/v2`; the schema tag stays `cce.metrics/v1`.

**Agent-vs-human bucketing is a single crisp rule.** Only the exact value `"mcp"`
counts as an agent search; every other `source` (`"cli"`, empty, unknown) is a human
search. This keeps the CLI path (`cce search` → `"cli"`) and the MCP path
(`context_search` → `"mcp"`) as the two buckets, and makes an ambiguous/old event fall
into the human bucket deterministically — the same rule in both engines. The top-level
key is `by_source` (the reconciled cross-engine name).

**`index_freshness` is PURELY log-derived — the dashboard makes zero network calls.**
Its shape is exactly `{indexes, source, sha, indexed_ts}`, computed from the latest index
event, so both engines reproduce it identically and `cce dashboard` stays self-contained
and fully offline. It deliberately carries **no** `remote_latest`/`behind_remote`: a
live remote comparison would mean a git fetch on the request path, which breaks the
offline/self-contained posture. That comparison lives only in `cce sync status` and MCP
`index_status`, which are expected to consult the remote. To make the *pulled* state
observable without a live lookup, `cce sync pull` records a `source: "sync-pull"` index
event in the log (with the pulled sha) — so `index_freshness.source` reads `"local"`
(built by `cce index`) or `"sync-pull"` (installed by `cce sync pull`) straight from the
log.

**The dashboard stays self-contained and offline.** The four new panels
(agent-vs-human, per-package, index-freshness, secret-safety) render from the same
`/api/metrics` body with inline JS/SVG — no new endpoint, no external asset, no network
call, still loopback-only and read-only. `secret_safety.sensitive_skipped` sums the
index events' skip counts (the only source of that datum), so it needs the additive
`index.sensitive_skipped` field. `by_package` (workspace only) is an array of objects,
each with a `package` field, **sorted by package** for deterministic cross-engine order,
and gains `mean_top_score` so the per-member panel shows quality, not just savings.
