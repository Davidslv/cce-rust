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
cargo test                # 301 tests — confirms a green build
```

Optionally put the binary on your PATH:

```bash
cargo install --path .    # installs `cce` into ~/.cargo/bin
cce --version             # cce 2.0.0
```

The rest of this guide writes `cce`; if you did not install it, use
`./target/release/cce` instead.

## 3. Your first index

The repo ships a tiny multi-language sample corpus (one file per pack). Index it
to a scratch store:

```bash
$ cce index test/fixture/samples --store /tmp/s.cce
Indexed test/fixture/samples
  files indexed : 7
  files skipped : 0
  total chunks  : 21
  embedder      : hash
  store         : /tmp/s.cce
  elapsed       : 0.002s
```

Files in a supported language (Python, JavaScript, Ruby, Rust, TypeScript, C) are
chunked per function/class by tree-sitter through **language packs**; everything
else becomes a single whole-file `module` chunk. Run `cce packs` to see the packs.

## 4. Your first search

`search` reopens the store in a fresh process — no re-embedding. Each result shows
the coarse `chunk_type` and the exact node `kind`:

```bash
$ cce search "build the index store" --store /tmp/s.cce --top-k 3 --no-graph
 1. [0.79xxxx] rust.rs:3-5 (function/function_item)
    pub fn build_index() -> HashMap<String, u32> {
 2. [0.71xxxx] rust.rs:7-9 (class/struct_item)
    pub struct Store {
 3. [0.65xxxx] rust.rs:11-15 (class/impl_item)
    impl Store {
```

That is the whole loop: **index a directory, then search it.** The top hit is the
`build_index` function — exactly the snippet you would feed to a model instead of
the whole file.

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
- [`adding-a-language.md`](adding-a-language.md) — add a language pack in one file.
- [`architecture.md`](architecture.md) — how the pipeline is built and why.
- [`../SPEC.md`](../SPEC.md) + [`../SPEC-V2.md`](../SPEC-V2.md) — the authoritative
  behaviour reference (base engine + language packs).
