# Architecture

This is the canonical architecture document for **cce-rust**. The authoritative
description of *behaviour* is [`SPEC.md`](../SPEC.md) (base engine),
[`DASHBOARD-SPEC.md`](../DASHBOARD-SPEC.md) (dashboard), and
[`SPEC-V2.md`](../SPEC-V2.md) (the v2 language-pack architecture); this document
explains how the implementation is shaped, why, and where the design would strain.

## Design goals

1. **The spec is the program.** Every observable behaviour derives from
   [`SPEC.md`](../SPEC.md) v1.0 and nothing else (clean-room). Ambiguities are
   resolved to the simplest reasonable reading and recorded in
   [`DECISIONS.md`](DECISIONS.md).
2. **Cross-language determinism.** The same corpus and query must yield the same
   ranked results here and in the Ruby sibling. This drives the rounding and
   tie-break rules below and is verified by `cce conformance`.
3. **One concern per file.** Each algorithm lives in its own module with a
   why/what/responsibilities header, so the code maps directly onto the spec.
4. **Test-first, hermetic.** Behaviour is pinned by tests before it is written;
   the default suite touches no network and no wall clock.
5. **Offline by default.** The tool runs with zero network access; the semantic
   embedder is strictly opt-in.

## Module map

One concern per file (SPEC §2). Every source file opens with a why/what/
responsibilities header.

| File | Concern | Key items |
|---|---|---|
| `src/config.rs` | Normative constants (SPEC §3) + runtime config | `EMBED_DIM`, `RRF_K`, weights, `EmbedderKind` |
| `src/tokenizer.rs` | The one shared byte-exact tokenizer (SPEC §4.1) | `tokenize` |
| `src/embedder.rs` | Hashing embedder, cosine, rounding, Ollama (SPEC §5, §11) | `fnv1a64`, `HashEmbedder`, `OllamaEmbedder`, `cosine`, `round6`, `score_key`, `format6`, `Embedder` trait |
| `src/chunker.rs` | Generic tree-sitter chunking, chunk IDs, `kind` (SPEC §4.2–4.4, SPEC-V2 §3–4) | `Chunk`, `Chunker`, `chunk_id`, `token_count`, `chunk_with_pack` |
| `src/packs/mod.rs` | The `LanguagePack` trait + `Registry` (SPEC-V2 §1) | `LanguagePack`, `PackExpected`, `Registry`, `default_registry` |
| `src/packs/{python,javascript,ruby,rust,typescript,c}.rs` | One pack per language (SPEC-V2 §2) | `PythonPack`, `RubyPack`, … |
| `src/packs/validators.rs` | Three-layer pack validators (SPEC-V2 §5) | `validate_pack`, `validate_all`, `startup_check` |
| `src/vector_store.rs` | Exact brute-force cosine ranking (SPEC §6.2) | `rank_by_cosine` |
| `src/keyword_store.rs` | Lucene-form BM25 (SPEC §6.3) | `Bm25Index` |
| `src/graph_store.rs` | Import graph + neighbor lookup (SPEC §6.7) | `Graph` |
| `src/retriever.rs` | The hybrid pipeline (SPEC §6) | `search`, `is_code_lookup`, `SearchResult` |
| `src/walker.rs` | Filesystem walk + ignore rules (SPEC §7.1) | `walk` |
| `src/store.rs` | Index assembly + JSON persistence, whole-file token counts (SPEC §7, DASH §3) | `Index`, `build_from_dir`, `save`, `load`, `baseline_tokens` |
| `src/conformance.rs` | `conformance.json` emitter, v2 shape with `kind` (SPEC-V2 §7) | `generate` |
| `src/bench.rs` | Per-language benchmark runner (SPEC-V2 §8) | `run`, `BenchReport` |
| `src/metrics.rs` | Persisted metrics event log; injected clock/id source (DASH §2) | `MetricsWriter`, `read_log`, `parse_log`, `Clock`, `IdSource`, `parse_iso` |
| `src/aggregator.rs` | Pure aggregate: totals, north-stars, series, deltas (DASH §4) | `aggregate`, `Aggregate`, `direction` |
| `src/dashboard.rs` | Loopback-only, read-only, self-contained web server (DASH §6) | `run`, `serve`, `route` |
| `src/main.rs` | CLI (SPEC §9, DASH §5) | clap command tree |

The metrics/dashboard modules (`DASH` = [`DASHBOARD-SPEC.md`](../DASHBOARD-SPEC.md),
v1.1) are the one part of the system that uses real wall-clock time; the clock and
id source are injected, and the aggregator is a pure function of `(events, now,
price)`. The full metrics pipeline — log → aggregator → API → page — its event
schema, and the aggregation formulas live in [`dashboard.md`](dashboard.md).

## Data flow

### Indexing (`cce index`)

```
dir ──walker::walk──▶ [(rel_path, content)]         # ignore rules, UTF-8, ≤2 MB
      │
      └─ per file ─ chunker::chunk_file ─▶ Chunk[]   # registry.pack_for(path); pack grammar or module fallback
                                        └▶ imports[] # pack.extract_imports(root, src)
      │
      └─ per chunk ─ embedder.embed(content) ─▶ [f64; 256]
      │
      ▼
   store::Index { chunks, file_imports }             # BM25 + graph derived
      │
      └─ Index::save ─▶ <store>/index.json            # JSON, embeddings included
```

The BM25 index and the import graph are **derived** structures — recomputed on
load, not persisted (SPEC §7 allows this).

### Search (`cce search`, fresh process)

```
store/index.json ──Index::load──▶ Index (+ recomputed BM25, graph)
query ──▶ retriever::search:
   1. classify intent           → fts_weight (1.5 CODE_LOOKUP else 1.0)   §6.1
   2. embed query, cosine to all → vector candidates (top_k×3), vrank    §6.2
   3. BM25 over unique q-tokens  → keyword candidates (top_k×3), frank   §6.3
   4. RRF fuse, normalize        → norm_rrf per candidate                §6.4
   5. confidence (vector+keyword+recency=0)                              §6.5
   6. final = 0.5·conf + 0.5·norm_rrf; ×0.8 if test/doc path            §6.6
   7. sort (rounded score desc, chunk_id asc); per-file cap 3; keep top_k
   8. if graph_enabled: pull chunks from imported neighbor files        §6.7
```

## Determinism strategy (SPEC §5.3)

Cross-language reproducibility hinges on three rules applied uniformly:

1. **Round-half-away-from-zero to 6 decimals** at every comparison/sort/emit —
   implemented once as `score_key` (integer key for sorting) and `format6`
   (fixed-string for output) in `embedder.rs`.
2. **Tie-break by `chunk_id` ascending** everywhere a sort could tie.
3. **Struct field order == spec order** in `conformance.rs`, so serde emits the
   documented JSON layout deterministically.

## Key type: `Chunk`

Carries everything persistence needs to reconstruct the index: `chunk_id`,
`file_path` (root-relative, `/` separators), `start_line`/`end_line` (1-based),
`chunk_type` (`function`/`class`/`module`), `kind` (the exact tree-sitter node
type, or `module` for the fallback), `language`, `content`, `token_count`, and
the `embedding` vector. `kind` is **not** part of `chunk_id`.

## Language packs (SPEC-V2)

Language support is factored into pluggable **packs** so the core chunker/importer
holds zero language-specific knowledge. Adding a language is a one-file change:
add a pack, register it, pass validation — no core edits. A guard test asserts the
core chunker names no language and no extension literal.

**The interface (`LanguagePack`).** Each pack is one struct implementing a trait
that declares: `name`, `extensions` (leading-dot, lowercase), `grammar()` (the
tree-sitter `Language`), `function_types` / `class_types` (node-type sets),
`import_node_types` (for the grammar-binding lint), `extract_imports`, a `sample`
snippet, and its `expected` self-test contract.

**The registry.** `Registry::register` rejects a pack whose extension is already
claimed; `pack_for(path)` resolves a file to its pack by lowercased extension;
`all()` lists them for `cce packs` and validation. `default_registry()` wires the
six shipped packs in a stable order. The generic chunker asks
`registry.pack_for(path)`; on `None` it emits the language-neutral module
fallback, otherwise it parses with `pack.grammar()`, walks the tree emitting a
chunk for every **named** node whose type is in the pack's function/class sets
(nested included), and records `kind = node.kind()`.

**The taxonomy.** Two levels: the coarse `chunk_type`
(`function`/`class`/`module`) that retrieval ranks on, and the exact `kind` (e.g.
`struct_specifier`, `trait_item`, `interface_declaration`, `method`) carried
through persistence, search, stats, and conformance. `kind` is deterministic
(straight from the node type), so both language implementations agree trivially.

**The validators (three layers).** (1) *Structural* — name/extension well-formed
and unique. (2) *Grammar-binding* — every declared node type is a real kind in the
grammar; on a miss it suggests the nearest valid kind by edit distance ("did you
mean"). (3) *Behavioural* — chunk the pack's own `sample` and assert it meets
`expected` (min function/class counts, the set of kinds present, and
`extract_imports == expected.imports` exactly). Surfaced by `cce packs
--validate`, a CI test gate over all packs, and cheap fail-fast startup checks
(duplicate extension, unloadable grammar) when the engine is constructed.

## Design rationale

- **Why a single JSON store, not a database.** Corpora are small (SPEC §1.2) and
  the overriding requirement is byte-for-byte determinism and easy diffing across
  two language implementations. A plain JSON file gives that with zero external
  dependencies — no SQLite, no server, no schema migration. Embeddings are stored
  inline so `search` never re-embeds the corpus. (See [`DECISIONS.md`](DECISIONS.md) D2.)
- **Why derived structures are recomputed on load.** The BM25 index and import
  graph are pure functions of the chunks. Persisting them would add a second
  source of truth to keep consistent; recomputing on load keeps the on-disk
  format minimal and impossible to desynchronise. It is cheap at these corpus
  sizes.
- **Why a hashing embedder is the default.** A deterministic FNV-1a embedder
  needs no model and no network, produces identical vectors in any language, and
  is therefore what the conformance gate and benchmarks stand on. Semantic
  quality is available via the optional Ollama embedder, but it is deliberately
  outside the deterministic core (its vectors are model-dependent).
- **Why full rebuild instead of incremental indexing.** Chunk IDs are content-
  derived, so a full rebuild is idempotent and trivially handles changed and
  removed files. Incremental indexing would add cache-invalidation complexity for
  little benefit at these sizes. (See [`DECISIONS.md`](DECISIONS.md) D3.)
- **Why exact brute-force cosine, not an ANN index.** Exactness is a determinism
  and simplicity win, and linear scan is fast enough for the target corpus size.
  An approximate index would introduce nondeterminism and a large dependency for
  no correctness gain here.
- **Why the algorithm logic lives in `lib`, not `main`.** The CLI is a thin
  argument-parsing and formatting shell; every algorithm is library code so the
  same fully-tested functions back both the binary and the test suite.

## Where this design would strain

Being honest about the edges of the design:

- **Large repositories.** Everything is in memory and the store is one JSON file
  with embeddings inline. On a very large corpus the file grows large, load time
  and memory scale linearly, and exact cosine scans every chunk per query. This
  design is built for the small-to-medium corpora the spec targets, not for a
  monorepo of millions of lines.
- **No incremental/partial reindex.** Every `cce index` rebuilds the whole store.
  For a huge repo where only a few files changed, that is wasteful — the design
  trades incremental efficiency for idempotent simplicity.
- **Semantic quality of the default embedder.** The hashing embedder is
  essentially lexical: retrieval reflects token overlap, not meaning. Queries
  phrased differently from the code will underperform. Real semantic search
  requires opting into Ollama, which then falls outside the deterministic
  conformance guarantee.
- **Language coverage.** Six packs ship (Python, JavaScript, Ruby, Rust,
  TypeScript, C); every other language falls back to a single whole-file `module`
  chunk, which is coarse and dilutes retrieval precision for those files. Adding a
  language is now a one-file pack, but it still requires a tree-sitter grammar.
- **One extension → one pack.** The registry maps each extension to exactly one
  pack, so it cannot serve two languages that share an extension (e.g. `.h` for
  both C and C++, or `.ts` for TypeScript vs. certain other tools). Nor can it do
  **per-file dialect detection** (JSX-in-`.js`, TSX-in-`.ts`, or content-sniffed
  variants) — resolution is purely by extension. A pack whose grammar needs
  per-file mode selection would have to pick one grammar per extension.
- **Structural, not semantic, node selection.** A pack lists node *types*; it
  cannot express "a `struct_specifier` only when it has a body", so e.g. a C
  struct **reference** in a parameter is emitted as a (bodyless) class chunk. This
  keeps packs declarative and cross-language-identical at the cost of a few noisy
  chunks.
- **Concurrency and freshness.** There is no locking or watch mode. The store is
  a point-in-time snapshot; concurrent indexers writing the same store, or a repo
  changing under a running search, are out of scope.
- **Cross-language drift risk.** Determinism across Rust and Ruby depends on both
  applying the exact rounding and tie-break rules identically. A subtle
  floating-point or ordering difference in either implementation would surface as
  a conformance mismatch — which is precisely why `conformance.json` is a gate,
  not an afterthought.
