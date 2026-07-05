# Code Context Engine (CCE) — Rust

A local command-line tool that indexes a source-code repository so a program (or
an LLM) can **search for the most relevant code snippets** instead of reading
whole files. `cce` walks a directory, AST-chunks each file with tree-sitter,
embeds every chunk, and stores a vector + keyword index on disk as JSON. Queries
are answered with hybrid **vector + BM25** retrieval fused by Reciprocal Rank
Fusion.

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

The write-up of the experiment:
[**"The spec was the program"**](https://davidslv.uk/2026/07/05/the-spec-was-the-program.html).

- Repository: <https://github.com/davidslv/cce-rust>
- Ruby sibling: <https://github.com/davidslv/cce-ruby>
- Author / sole maintainer: **David Silva** ([@davidslv](https://github.com/davidslv), <https://davidslv.uk>)

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
cce --version             # cce 1.0.0
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
[
  {
    "chunk_id": "2d5d9159a130943e",
    "chunk_type": "module",
    "end_line": 75,
    "file_path": "vector_store.rs",
    "rank": 1,
    "score": "0.852287",
    "start_line": 1
  },
  ...
]
```

Search flags: `--dir <dir>` (resolves `<dir>/.cce`) or `--store <path>`,
`--top-k N` (default 5), `--no-graph` (skip import-graph expansion), `--json`.

### A worked example (AST chunking)

Python and JavaScript files are chunked by function/class. Index the bundled
fixture and search it:

```bash
$ cce index test/fixture --store /tmp/fix.cce
Indexed test/fixture
  files indexed : 3
  files skipped : 0
  total chunks  : 7
  embedder      : hash

$ cce search "hash a password" --store /tmp/fix.cce --top-k 3 --no-graph
 1. [0.868519] auth.py:3-4 (function)
    def hash_password(password):
 2. [0.866667] auth.py:6-7 (function)
    def verify_password(password, digest):
 3. [0.568935] README.md:1-2 (module)
    # Demo
```

### Statistics

```bash
$ cce stats --store /tmp/fix.cce
Store: /tmp/fix.cce
  chunks         : 7
  files          : 3
  avg token/chunk: 19.9
  store size     : 9820 bytes
  by language:
    plaintext   : 1
    python      : 6
```

### Benchmark

Runs the pipeline over a checked-out repository and writes
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md):

```bash
$ cce bench /path/to/flask --name "pallets/flask@3.0.3"
Benchmark complete (pallets/flask@3.0.3, commit c12a5d8...):
  files/chunks : 82/1579
  index        : 0.109s (14535.1 chunks/s)
  latency      : p50 0.604ms  p95 0.662ms
  recall@5/@10 : 90.0% / 100.0%
  token savings: 90.0%
```

### Conformance

Emits the cross-implementation conformance file — byte-identical across runs and
designed to match the Ruby sibling on the same fixture:

```bash
$ cce conformance test/fixture -o conformance.json
wrote conformance.json
```

## What's inside

- **AST-aware chunking** via tree-sitter for Python and JavaScript; a whole-file
  `module` fallback for every other language.
- A **deterministic hashing embedder** (FNV-1a, SPEC §5.1), exact brute-force
  cosine, Lucene-form **BM25**, **Reciprocal Rank Fusion**, a confidence blend,
  a test/doc path penalty, a per-file diversity cap, and import-graph expansion —
  all with the exact SPEC constants.
- **On-disk JSON persistence**; `search`, `stats`, and `conformance` reopen the
  store in a fresh process.
- **Determinism** everywhere: scores are rounded to 6 decimals
  (round-half-away-from-zero) and ties break by `chunk_id` ascending (SPEC §5.3),
  so `cce conformance test/fixture` is byte-identical across runs.

## Tests & coverage

```bash
cargo test                                                  # 84 tests
cargo clippy --all-targets --all-features -- -D warnings    # lint gate
cargo fmt --check                                           # format gate
```

The suite is **84 tests** (83 hermetic + 1 `#[ignore]` Ollama integration test)
and measures **95.33% line coverage** via `cargo llvm-cov`. The default suite is
fully deterministic and makes no network calls.

## Documentation

| Doc | What it covers |
|---|---|
| [`SPEC.md`](SPEC.md) | The normative specification (v1.0) — the single source of truth |
| [`docs/getting-started.md`](docs/getting-started.md) | Install → first index + search |
| [`docs/architecture.md`](docs/architecture.md) | Design goals, pipeline, rationale, and where it strains |
| [`docs/how-to.md`](docs/how-to.md) | Task recipes: index, search, bench, conformance, Ollama |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | How each spec ambiguity was resolved |
| [`docs/TDD.md`](docs/TDD.md) | The red → green log and coverage |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Measured numbers on a real corpus |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) · [`SECURITY.md`](SECURITY.md) · [`SUPPORT.md`](SUPPORT.md) · [`GOVERNANCE.md`](GOVERNANCE.md) | Project process |

## License

[MIT](LICENSE) © 2026 David Silva.
