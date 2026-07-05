# Getting started

This guide takes you from nothing to your **first successful index and search**
with `cce`. It should be self-serve; if a step fails, that is a bug in this guide
— please [open an issue](https://github.com/davidslv/cce-rust/issues).

## 1. Prerequisites

`cce` is a single Rust binary whose tree-sitter dependencies compile C grammars
from source. You need:

- A **stable Rust toolchain** (via [rustup](https://rustup.rs)).
- A **C compiler** (Xcode Command Line Tools on macOS; `build-essential` on
  Ubuntu/Debian).

There is **no database and no other system library** to install — the index is
just JSON on disk.

### macOS

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
xcode-select --install
```

### Ubuntu / Debian

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
sudo apt-get update && sudo apt-get install -y build-essential
```

## 2. Build and verify

```bash
git clone https://github.com/davidslv/cce-rust
cd cce-rust
cargo build --release     # binary at target/release/cce
cargo test                # 113 tests — confirms a green build
```

Optionally put the binary on your PATH:

```bash
cargo install --path .    # installs `cce` into ~/.cargo/bin
cce --version             # cce 1.1.0
```

The rest of this guide writes `cce`; if you did not install it, use
`./target/release/cce` instead.

## 3. Your first index

The repo ships a tiny Python fixture. Index it to a scratch store:

```bash
$ cce index test/fixture --store /tmp/fix.cce
Indexed test/fixture
  files indexed : 3
  files skipped : 0
  total chunks  : 7
  embedder      : hash
  store         : /tmp/fix.cce
  elapsed       : 0.001s
```

Python (and JavaScript) files are chunked per function/class by tree-sitter;
everything else becomes a single whole-file `module` chunk.

## 4. Your first search

`search` reopens the store in a fresh process — no re-embedding:

```bash
$ cce search "hash a password" --store /tmp/fix.cce --top-k 3 --no-graph
 1. [0.868519] auth.py:3-4 (function)
    def hash_password(password):
 2. [0.866667] auth.py:6-7 (function)
    def verify_password(password, digest):
 3. [0.568935] README.md:1-2 (module)
    # Demo
```

That is the whole loop: **index a directory, then search it.** The top hit is the
`hash_password` function — exactly the snippet you would feed to a model instead
of the whole file.

## 5. Try it on your own code

Point `cce` at any project. By default the store lives at `<dir>/.cce/index.json`,
so you can search with `--dir` instead of `--store`:

```bash
cce index ./my-project
cce search "where is the request handler" --dir ./my-project --top-k 5
cce stats --dir ./my-project
```

Add `--json` to `search` for machine-readable results.

## 6. Optional: semantic embeddings

The default hashing embedder is offline and deterministic. If you want
model-based vectors, install [Ollama](https://ollama.com), pull the model, and
pass `--embedder ollama`:

```bash
ollama pull nomic-embed-text
cce index ./my-project --embedder ollama
```

If Ollama is not running, `cce` warns and falls back to the hash embedder — it
never fails because of it.

## Where to next

- [`how-to.md`](how-to.md) — task recipes for every command.
- [`architecture.md`](architecture.md) — how the pipeline is built and why.
- [`../SPEC.md`](../SPEC.md) — the authoritative behaviour reference.
