# Releasing

Releases are **tag-driven and fully automated**. Pushing a `vX.Y.Z` tag runs
[`release.yml`](.github/workflows/release.yml), which re-runs every CI gate on the
tagged commit and then publishes a GitHub Release — you never build or upload
artifacts by hand.

## The process

1. **Land the version bump on `main` first** (this is the existing convention —
   every release PR already bumps `Cargo.toml` and adds a `## [X.Y.Z] - YYYY-MM-DD`
   section to `CHANGELOG.md`):

   ```bash
   grep -m1 '^version' Cargo.toml     # confirm main says the version you mean
   ```

2. **Tag that commit and push the tag:**

   ```bash
   git checkout main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

3. **CI does the rest.** The workflow:
   - re-runs `cargo fmt --check`, `clippy -D warnings`, and the full test suite
     on the tagged commit;
   - **fails the release** if the tag doesn't match `Cargo.toml`'s `version`, or if
     `CHANGELOG.md` has no `## [X.Y.Z]` section (so a mis-tag can't ship);
   - builds release binaries for macOS (Apple Silicon + Intel) and Linux
     (x86_64 + arm64);
   - publishes the GitHub Release using the CHANGELOG section as the notes, with
     the four tarballs and a combined `SHA256SUMS`.

## What a release contains

```
cce-vX.Y.Z-aarch64-apple-darwin.tar.gz      # macOS Apple Silicon
cce-vX.Y.Z-x86_64-apple-darwin.tar.gz       # macOS Intel
cce-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz  # Linux x86_64
cce-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz # Linux arm64
SHA256SUMS
```

Each tarball holds the `cce` binary plus `LICENSE`, `README.md`, and `CHANGELOG.md`.

> **The asset layout is a compatibility contract.** `cce update` (#75) consumes
> exactly these assets from released binaries in the field: it discovers the
> latest version by fetching `releases/latest/download/SHA256SUMS` and parsing
> the `cce-vX.Y.Z-<target>.tar.gz` names, verifies the tarball against that
> file, and extracts `cce-vX.Y.Z-<target>/cce` (it also reads the tarball's
> `CHANGELOG.md` to print the post-update delta). Renaming the assets, the
> `SHA256SUMS` file, the tarball's inner directory, or the target triples breaks
> the upgrade path for every already-shipped binary — change them only with a
> migration plan (`src/update.rs` documents the consuming side).

## Fixing a bad release

Delete the release and the tag, fix the problem on `main`, and cut the **next**
patch version — never re-tag the same version:

```bash
gh release delete vX.Y.Z --yes
git push origin :refs/tags/vX.Y.Z
```

## Deliberately out of scope

- **crates.io** — not published there for now; installing from a release tarball or
  `cargo install --path .` covers the current audience. Revisit if demand appears.
- **Windows binaries** — the tree-sitter C grammars should compile fine with MSVC,
  but it's untested; add a `windows-latest` matrix entry when someone needs it.
- **Homebrew tap / install script** — a follow-up once release cadence settles.
