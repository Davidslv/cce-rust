# Code Context Engine (CCE) — Rust

A local command-line tool that indexes a source-code repository so a program (or
an LLM) can **search for the most relevant code snippets** instead of reading
whole files. `cce` walks a directory, AST-chunks each file with tree-sitter,
embeds every chunk, and stores a vector + keyword index on disk as JSON. Queries
are answered with hybrid **vector + BM25** retrieval fused by Reciprocal Rank
Fusion.

Since **v2.0** language support is a set of pluggable **language packs**: the
core engine holds zero language-specific knowledge, and six packs ship in the box
— **Python, JavaScript, Ruby, Rust, TypeScript, and C** (see
[Supported languages](#supported-languages)).

```
index a directory → walk → AST-chunk → embed → store (vectors + BM25 + import graph)
search a query    → vector + BM25 + RRF fusion → confidence blend → path penalty
                    → per-file diversity cap → optional import-graph expansion → top-K
```

## Provenance: a clean-room experiment

This is a **clean-room reimplementation built test-first** from a single
specification, [`SPEC.md`](SPEC.md) (SPEC v1.0), with no reference to any other
implementation. It is an experiment in whether a precise spec can act as the
program. A sibling implementation in Ruby was built from the *identical* spec and
lives at **[davidslv/cce-ruby](https://github.com/davidslv/cce-ruby)** — the two
are conformance-compatible on the same corpus (see [Conformance](#conformance)).
**v2.0** evolved both implementations, from [`SPEC-V2.md`](SPEC-V2.md), into the
pluggable language-pack architecture — again test-first and independently.

The write-up of the experiment:
[**"The spec was the program"**](https://davidslv.uk/2026/07/05/the-spec-was-the-program.html).

- Repository: <https://github.com/davidslv/cce-rust>
- Ruby sibling: <https://github.com/davidslv/cce-ruby>
- Author / sole maintainer: **David Silva** ([@davidslv](https://github.com/davidslv), <https://davidslv.uk>)

## Walkthrough

![CCE walkthrough — language packs, index, validate, search, stats](docs/walkthrough.gif)

▶ **Interactive version:** open [`docs/presentation/index.html`](docs/presentation/index.html)
in a browser — a self-contained, autoplaying terminal cast (no dependencies, no network).

## Installation & environment setup

`cce` is a single Rust binary. The tree-sitter crates compile their C grammars
from source, so you need a stable Rust toolchain and a working C compiler.
**There are no other system libraries** — the index is plain JSON on disk, so
there is no database (no SQLite) to install.

### macOS

```bash
# 1. Rust (stable) via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. A C toolchain for the tree-sitter grammars
xcode-select --install    # Xcode Command Line Tools (clang, make)

# 3. Build and test
git clone https://github.com/davidslv/cce-rust
cd cce-rust
cargo build --release     # binary at target/release/cce
cargo test                # confirm a green build
```

### Ubuntu / Debian

```bash
# 1. Rust (stable) via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. A C toolchain for the tree-sitter grammars
sudo apt-get update && sudo apt-get install -y build-essential

# 3. Build and test
git clone https://github.com/davidslv/cce-rust
cd cce-rust
cargo build --release     # binary at target/release/cce
cargo test                # confirm a green build
```

### Install the binary on your PATH

```bash
cargo install --path .    # installs `cce` into ~/.cargo/bin
cce --version             # cce 2.0.0
```

### Optional: the semantic embedder (Ollama)

The default **hashing** embedder needs no setup and makes **no network calls**.
A semantic embedder is available if you want model-based vectors — it is
entirely opt-in:

```bash
# Install Ollama (https://ollama.com), then pull the embedding model:
ollama pull nomic-embed-text
```

`cce` talks to a local Ollama server over `localhost` HTTP only when you pass
`--embedder ollama`. If Ollama is unreachable it prints a warning and falls back
to the hash embedder, so indexing never fails because of it.

## Usage

The binary is `cce`. Examples below assume `target/release/cce` is on your PATH
(or substitute the full path).

### Index a directory

```bash
$ cce index ./src
Indexed ./src
  files indexed : 14
  files skipped : 0
  total chunks  : 14
  embedder      : hash
  store         : ./src/.cce/index.json
  elapsed       : 0.004s
```

By default the store is written to `<dir>/.cce/index.json`. Override it with
`--store <path>`, or select the embedder with `--embedder hash|ollama`.

### Search

`search` reopens the store in a fresh process and never re-embeds the corpus.

```bash
$ cce search "how does the hashing embedder work" --dir ./src --top-k 5 --no-graph
 1. [0.845094] config.rs:1-111 (module)
    //! # config — normative constants and runtime configuration
 2. [0.844081] bench.rs:1-278 (module)
    //! # bench — the benchmark runner behind `cce bench`
 3. [0.840160] vector_store.rs:1-75 (module)
    //! # vector_store — exact brute-force cosine ranking
 4. [0.827884] lib.rs:1-31 (module)
    //! # Code Context Engine (CCE) — library root
 5. [0.809263] embedder.rs:1-321 (module)
    //! # embedder — deterministic hashing embedder, cosine, and rounding
```

Add `--json` for machine-readable output:

```bash
$ cce search "cosine similarity ranking" --dir ./src --top-k 3 --no-graph --json
{
  "query_id": "3f9a1c0b7e21",
  "results": [
    {
      "chunk_id": "2d5d9159a130943e",
      "chunk_type": "module",
      "kind": "module",
      "end_line": 75,
      "file_path": "vector_store.rs",
      "rank": 1,
      "score": "0.852287",
      "start_line": 1
    }
  ]
}
```

Since v1.1 the `--json` body is an **object** with a top-level `query_id` (the id
of the recorded search event — see [Dashboard & observability](#dashboard--observability))
wrapping the `results` array. Human output prints the same id on a final
`query-id:` line. Pass `--no-metrics` to skip recording (then `query_id` is null).

Search flags: `--dir <dir>` (resolves `<dir>/.cce`) or `--store <path>`,
`--top-k N` (default 5), `--no-graph` (skip import-graph expansion), `--json`,
`--no-metrics`.

### A worked example (AST chunking)

Every supported language is chunked into its functions/classes. Index the bundled
multi-language sample corpus and search it — results carry both the coarse
`chunk_type` and the exact tree-sitter node `kind`:

```bash
$ cce index test/fixture/samples --store /tmp/s.cce
Indexed test/fixture/samples
  files indexed : 7
  files skipped : 0
  total chunks  : 21
  embedder      : hash

$ cce search "build the index store" --store /tmp/s.cce --top-k 3 --no-graph
 1. [0.79xxxx] rust.rs:3-5 (function/function_item)
    pub fn build_index() -> HashMap<String, u32> {
 2. [0.71xxxx] rust.rs:7-9 (class/struct_item)
    pub struct Store {
 3. [0.65xxxx] rust.rs:11-15 (class/impl_item)
    impl Store {
```

### Statistics

```bash
$ cce stats --store /tmp/s.cce
Store: /tmp/s.cce
  chunks         : 21
  files          : 7
  avg token/chunk: ...
  by language:
    c           : 3
    javascript  : 3
    plaintext   : 1
    python      : 3
    ruby        : 3
    rust        : 4
    typescript  : 4
  by kind:
    class_declaration   : 1
    function_definition : 2
    impl_item           : 1
    module              : 1
    struct_item         : 1
    ...
```

### Language packs

List the registered packs, or run the three-layer validators over every pack:

```bash
$ cce packs
Registered language packs (6):
  python       .py                      1 fn / 1 class types · grammar: ... node kinds
  javascript   .js,.jsx,.mjs,.cjs       4 fn / 1 class types · grammar: ... node kinds
  ruby         .rb                      2 fn / 2 class types · grammar: ... node kinds
  rust         .rs                      1 fn / 5 class types · grammar: ... node kinds
  typescript   .ts,.tsx                 4 fn / 3 class types · grammar: ... node kinds
  c            .c,.h                    1 fn / 3 class types · grammar: ... node kinds

$ cce packs --validate
[pack:python] ok
...
all 6 packs passed validation
```

Adding a language is: add one pack file, register it, and pass validation — no
core edits. See [`docs/adding-a-language.md`](docs/adding-a-language.md).

### Benchmark

Indexes a checked-out repository **whole** (exactly as `cce index`) and runs one
language's labeled query set, writing [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md):

```bash
$ cce bench /path/to/sinatra --lang ruby --name "sinatra/sinatra@v4.1.1"
Benchmark complete (sinatra/sinatra@v4.1.1, ruby, commit 7b50a1b...):
  files/chunks : 287/1337
  index        : 0.167s (7990.0 chunks/s)
  latency      : p50 0.429ms  p95 0.549ms
  recall@5/@10 : 90.0% / 90.0%
  token savings: 72.6%
```

`--lang` selects only the query set and label — the whole repo is indexed either
way, so recall and token-savings match the Ruby sibling exactly. The four active
languages benchmarked are Ruby (sinatra), Rust (hyperfine), TypeScript (zustand),
and C (jq) — see [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Conformance

Emits the cross-implementation conformance file over the seven-file sample corpus
— byte-identical across runs and designed to match the Ruby sibling. Each chunk
carries its `kind` (v2 shape):

```bash
$ cce conformance test/fixture/samples -o conformance.json
wrote conformance.json
```

## Dashboard & observability

Since v1.1, CCE keeps a small **persisted metrics log** so you can see whether
using it is *improving or degrading your experience over time*, from real data.
Every `cce search` and `cce index` appends one JSON line to
`<store-dir>/metrics.jsonl` (best-effort — a metrics failure never affects the
command), and `cce feedback` lets you rate a past result. A local, read-only web
dashboard visualizes two north-stars — **token/cost savings** and **retrieval
quality** — each trended current-vs-prior with an ↑ improving / ↓ degrading / →
flat indicator, plus a recent-searches table. (The base engine and
`conformance.json` are untouched by any of this.)

![CCE dashboard — token/cost savings and retrieval quality, trended](docs/dashboard.png)

```bash
# 1. Index and search as usual — events are recorded automatically.
$ cce index ./src
$ cce search "how does confidence scoring work" --dir ./src
 1. [0.83xxxx] retriever.rs:1-423 (module)
    //! # retriever — the hybrid retrieval pipeline
query-id: 3f9a1c0b7e21  ·  rate with: cce feedback 3f9a1c0b7e21 --helpful|--not-helpful

# 2. Rate that result (optional but powers the "quality" north-star).
$ cce feedback 3f9a1c0b7e21 --helpful --dir ./src
recorded feedback (helpful) for 3f9a1c0b7e21  [event a1b2c3d4e5f6]

# 3. Open the dashboard (loopback only, read-only, fully self-contained).
$ cce dashboard --dir ./src
cce dashboard: serving http://127.0.0.1:8787/  (loopback only, read-only)
```

The server binds `127.0.0.1` only, mutates nothing, and inlines all CSS/JS and
draws its own SVG charts — **no external network, CDN, or fonts**. It exposes
`GET /` (the page), `GET /api/metrics` (the aggregate JSON, recomputed per
request), and `GET /api/health`. Flags: `--dir DIR` / `--store PATH` /
`--metrics PATH` to locate the log, `--port N` (default 8787), `--price N` (USD
per 1M input tokens for the $-saved estimate, default 3.00), `--no-open`.

See [`docs/dashboard.md`](docs/dashboard.md) for the pipeline, event schema, and
the exact aggregation formulas.

## Supported languages

Language support is a set of pluggable **language packs** (SPEC-V2). The core
chunker/importer references no language by name; each pack is one self-contained
file declaring its extensions, grammar, function/class node types, and import
rule, and each is guarded by three validator layers (`cce packs --validate`).

| Pack | Extensions | Chunks | Imports from |
|---|---|---|---|
| `python` | `.py` | functions, classes | `import`, `from … import` |
| `javascript` | `.js`, `.jsx`, `.mjs`, `.cjs` | functions, methods, arrows, classes | `import … from "x"` |
| `ruby` | `.rb` | methods, classes, modules | `require`, `require_relative` |
| `rust` | `.rs` | fns; struct/enum/trait/impl/union | `use` (first segment) |
| `typescript` | `.ts`, `.tsx` | functions, methods, class/interface/enum | `import … from "x"` |
| `c` | `.c`, `.h` | functions; struct/union/enum | `#include <…>` / `"…"` |

Any file no pack claims (or that a pack parses to zero symbols) becomes a single
whole-file `module` fallback chunk. Every chunk records the exact tree-sitter
node type in a `kind` field alongside the coarse `chunk_type`
(`function`/`class`/`module`). Adding a language is a one-file change —
[`docs/adding-a-language.md`](docs/adding-a-language.md).

## What's inside

- **AST-aware chunking** via tree-sitter through six pluggable language packs
  (Python, JavaScript, Ruby, Rust, TypeScript, C); a whole-file `module` fallback
  for every other language.
- **Pack validators** — structural, grammar-binding ("did you mean" node-kind
  suggestions), and behavioural self-test — surfaced by `cce packs --validate`.
- A **deterministic hashing embedder** (FNV-1a, SPEC §5.1), exact brute-force
  cosine, Lucene-form **BM25**, **Reciprocal Rank Fusion**, a confidence blend,
  a test/doc path penalty, a per-file diversity cap, and import-graph expansion —
  all with the exact SPEC constants.
- **On-disk JSON persistence**; `search`, `stats`, and `conformance` reopen the
  store in a fresh process.
- **Determinism** everywhere: scores are rounded to 6 decimals
  (round-half-away-from-zero) and ties break by `chunk_id` ascending (SPEC §5.3),
  so `cce conformance test/fixture/samples` is byte-identical across runs.

## Tests & coverage

```bash
cargo test                                                  # 129 tests
cargo clippy --all-targets --all-features -- -D warnings    # lint gate
cargo fmt --check                                           # format gate
```

The suite is **129 passing tests** (+1 `#[ignore]` Ollama integration test) and
measures **94.76% line coverage** via `cargo llvm-cov`. The default suite is
fully deterministic and makes no network calls — including the metrics subsystem,
whose clock and id source are injected and whose dashboard tests bind an
ephemeral loopback port. A CI test gate runs the three-layer validators over every
language pack, and a guard test asserts the core chunker names no language.

## Documentation

| Doc | What it covers |
|---|---|
| [`SPEC.md`](SPEC.md) | The normative base specification (v1.0) |
| [`DASHBOARD-SPEC.md`](DASHBOARD-SPEC.md) | The dashboard & observability addendum (v1.1) |
| [`SPEC-V2.md`](SPEC-V2.md) | The language-packs evolution spec (v2.0) |
| [`docs/getting-started.md`](docs/getting-started.md) | Install → first index + search |
| [`docs/adding-a-language.md`](docs/adding-a-language.md) | Step-by-step guide to adding a language pack |
| [`docs/architecture.md`](docs/architecture.md) | Design goals, pipeline, language packs, and where it strains |
| [`docs/dashboard.md`](docs/dashboard.md) | Metrics pipeline, event schema, aggregation formulas |
| [`docs/how-to.md`](docs/how-to.md) | Task recipes: index, search, feedback, dashboard, bench, conformance |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | How each spec ambiguity was resolved |
| [`docs/TDD.md`](docs/TDD.md) | The red → green log and coverage |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Measured numbers on a real corpus |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) · [`SECURITY.md`](SECURITY.md) · [`SUPPORT.md`](SUPPORT.md) · [`GOVERNANCE.md`](GOVERNANCE.md) | Project process |

## License

[MIT](LICENSE) © 2026 David Silva.
