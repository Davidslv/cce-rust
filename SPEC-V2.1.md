# CCE v2.1 — Secret & Sensitive-File Protection (Specification v2.1)

**Status:** Normative. Evolution spec — deltas only. Base = `SPEC.md` (v1.0),
`DASHBOARD-SPEC.md` (v1.1), `SPEC-V2.md` (v2.0), all already implemented in this
repo. Everything not mentioned here is unchanged; all existing tests stay green.

**How this is built:** you are a fresh sub-agent working **in your own existing
repository** (not a clean room — read and refactor the code). Branch
**`feat/secret-scrubbing`**. Commit there; **do not push, do not open a PR** — the
orchestrator pushes, opens the PR, and merges when CI is green. Do **not** read
the sibling-language repo (keep the two independent). This is a **minor release:
v2.1.0** (additive, secure-by-default; not a breaking API change).

**Why:** today the indexer reads and stores whatever UTF-8 text files it walks —
including `.env` and credential files — into the local store. It is local-only,
but sensitive values sit in `.cce/…` on disk. This spec makes indexing
**secret-safe by default** in two layers.

---

## 1. Constants (normative — spell these exactly; both languages must match)

**Sensitive file extensions** (Layer 1; compare the file's final extension,
case-insensitive, without the dot):
```
pem  key  p12  pfx  keystore  jks  ppk  der  asc
```

**Sensitive exact basenames** (Layer 1; compare the whole file name,
case-insensitive):
```
credentials.json  credentials.yml  credentials.yaml
secrets.json      secrets.yml      secrets.yaml
.netrc  .pgpass  .htpasswd  .dockercfg  kubeconfig
id_rsa  id_dsa  id_ecdsa  id_ed25519
```

**Dotenv rule** (Layer 1): a file is sensitive if its basename is `.env` OR
starts with `.env.` (case-insensitive) — **except** when it ends with one of
`.example`, `.sample`, `.template`, `.dist` (case-insensitive), which are safe
templates and must be indexed normally.

**Redaction patterns** (Layer 2; apply in this order; replace the matched value —
not surrounding text — with `[REDACTED:<LABEL>]`). Matching is case-sensitive
unless noted.

| # | Label | Pattern (regex) |
|---|---|---|
| 1 | `PRIVATE_KEY` | `-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----` (replace the whole block) |
| 2 | `ANTHROPIC_KEY` | `sk-ant-[A-Za-z0-9_-]{20,}` |
| 3 | `OPENAI_KEY` | `sk-[A-Za-z0-9]{32,}` |
| 4 | `STRIPE_KEY` | `sk_live_[A-Za-z0-9]{16,}` |
| 5 | `GITHUB_TOKEN` | `gh[pousr]_[A-Za-z0-9]{36,}` |
| 6 | `SLACK_TOKEN` | `xox[baprs]-[A-Za-z0-9-]{10,}` |
| 7 | `AWS_ACCESS_KEY` | `AKIA[0-9A-Z]{16}` |
| 8 | `GOOGLE_API_KEY` | `AIza[0-9A-Za-z_-]{35}` |
| 9 | `JWT` | `eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}` |
| 10 | `SECRET_ASSIGNMENT` | see below (generic key = value) |

Run 1–9 first (specific), then 10 (generic).

**Pattern 10 (generic assignment):** match a secret-ish key, an `=`/`:` operator,
and a quoted-or-bare value; redact only the **value**. Key (case-insensitive):
one of `password`, `passwd`, `secret`, `token`, `api[_-]?key`,
`access[_-]?key`, `secret[_-]?key`, `auth[_-]?token`, `private[_-]?key`.
Operator: optional spaces, then `=` or `:`, then optional spaces, then an optional
quote. Value: the run of characters up to the closing quote / whitespace / line
end, **length ≥ 8**. Replace the value with `[REDACTED:SECRET]`.

**Placeholder guard (applies to pattern 10 only):** do **not** redact if the value
(lowercased) matches any of — starts with `your`, `my-`, `the-`, `example`,
`changeme`, `placeholder`, `dummy`, `test`, `sample`, `xxx`; OR is `<...>` /
`${...}` / `{{...}}` interpolation; OR is one of `null nil none true false`; OR is
a single repeated character. (Guards prevent redacting docs/examples like
`API_KEY="your-api-key-here"`.)

---

## 2. Behaviour

**Layer 1 (walker):** before reading a file, test its basename against §1
(extensions, exact basenames, dotenv rule). If sensitive, **do not read it** and
count it separately as `sensitive_skipped` (distinct from the existing
size/non-UTF-8 `skipped`). Directory ignore rules are unchanged.

**Layer 2 (indexer, before chunking):** run the §1 redaction over each indexed
file's content. The **redacted** content is what gets chunked, embedded, and
stored — so the store never contains the secret, and `chunk_id`/`token_count`
derive from the redacted text. Redaction is deterministic.

**Opt-out:** a global flag `--allow-secrets` (default off ⇒ protection on)
disables **both** layers for that run. Applies to `index` (and any command that
indexes). Document it.

**Reporting:** the `index` summary reports the sensitive-skip count (e.g.
`sensitive skipped : N`). `stats` may show it if convenient. When `--allow-secrets`
is set, print a one-line warning that protection is disabled.

**Untouched:** the six sample fixtures contain no secrets and no sensitive
filenames, so Layer 1/2 are no-ops on them — `conformance.json` MUST remain
byte-identical. Re-verify.

---

## 3. Fixture & cross-language check

The secrets corpus below is **generated into a temp directory at test runtime**,
not committed — a committed file containing a contiguous, real-format secret is
blocked by source-hosting push-protection scanners. So the tests assemble each
secret from split fragments (`concat!`/string joins) that reconstruct the full
value at runtime. To keep this document itself secret-free, the literals here are
shown **split with `" + "`** (e.g. `"AKIA" + "IOSFODNN7EXAMPLE"`); the file the
test writes contains the joined, contiguous value.

Generate `test/fixture/secrets/` (at runtime) with these files:

**`.env`** (must be SKIPPED — never indexed):
```
AWS_ACCESS_KEY_ID="AKIA" + "IOSFODNN7EXAMPLE"
DATABASE_URL=postgres://user:hunter2@localhost/app
```

**`.env.example`** (must be INDEXED normally — safe template):
```
AWS_ACCESS_KEY_ID=your-access-key-here
DATABASE_URL=postgres://user:password@localhost/app
```

**`id_rsa`** (must be SKIPPED — sensitive basename):
```
-----BEGIN OPENSSH PRIVATE " + "KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAA
-----END OPENSSH PRIVATE " + "KEY-----
```

**`config.rb`** (must be INDEXED, with redaction applied):
```ruby
module Config
  AWS = "AKIA" + "IOSFODNN7EXAMPLE"
  API_KEY = "your-api-key-here"
  STRIPE = "sk" + "_live_" + "4eC39HqLyjWDarjtT1zdp7dc"
end
```

**Expected (assert in tests):**
- `.env` and `id_rsa` are counted as `sensitive_skipped` and produce **no**
  chunks / no store rows.
- `.env.example` is indexed (a fallback `module` chunk).
- `config.rb` is indexed; in its stored chunk content, the AWS access-key literal
  → `[REDACTED:AWS_ACCESS_KEY]` and the Stripe `sk_live_…` literal →
  `[REDACTED:STRIPE_KEY]`, but `your-api-key-here` is **unchanged** (placeholder
  guard).
- With `--allow-secrets`, `.env`/`id_rsa` ARE indexed and `config.rb` is stored
  verbatim (no redaction).

Also add a direct unit test of the redactor: given the input
`token = "ghp" + "_0123456789abcdefghijklmnopqrstuvwx01"` (assembled) it returns
`token = "[REDACTED:GITHUB_TOKEN]"`; given `key = "your-api-key"` it returns the
input unchanged. Since the patterns are specified exactly, both implementations
must produce identical redaction output on identical input.

---

## 4. Tests, docs, release

- **Test-first.** Cover: each Layer-1 category (extension, exact basename, dotenv
  vs `.env.example`), the redactor unit (each label + a placeholder-guard
  negative), the fixture end-to-end (skips + redactions + `--allow-secrets`
  bypass), and a re-assertion that `conformance.json` is byte-identical.
- **Gates stay green:** Ruby `bundle exec rake test` (coverage ≥ 93%); Rust
  `cargo test` + `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo fmt --check` (coverage ≥ 92%).
- **Docs:** update `README.md` — replace any "no secret scrubbing" caveat with
  the new secure-by-default behaviour and the `--allow-secrets` opt-out. Update
  `SECURITY.md` threat model (files read → sensitive files skipped + secrets
  redacted by default; residual risk is the local-only store; `--allow-secrets`
  disables it). Add a `CHANGELOG.md` `2.1.0` entry (Keep a Changelog). Bump the
  version to **2.1.0** where declared (Ruby: `lib/cce.rb` + `CITATION.cff`; Rust:
  `Cargo.toml` + `CITATION.cff`). A short note in `docs/architecture.md` is
  welcome.
- Record any ambiguity resolutions in `docs/DECISIONS.md`.

**When done, report:** the two layers built; new test count + coverage;
confirmation the secrets fixture behaves as specified and `--allow-secrets`
bypasses it; `conformance.json` unchanged; all gates green; and the
`feat/secret-scrubbing` commit hash.
