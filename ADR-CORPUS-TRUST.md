# ADR — Corpus trust posture: trust-the-pusher, private-repo ACL, one publisher per corpus

**Status:** Accepted (2026-07-14). Records the decision **before any hardening
code**; it is a posture, not a feature — no behaviour changes with this ADR. The
current tooling already implements exactly this posture (see *Consequences*).

**Traceability:** Epic [Davidslv/signal-engine#8](https://github.com/Davidslv/signal-engine/issues/8),
ticket **U6.3** — constraint **C22**; gap **G18**; resolves **Open Q5**. The
authoritative dossier and requirement text live in
[`notes/context-engine/unified-proposal.md`](https://github.com/Davidslv/notes/blob/main/context-engine/unified-proposal.md)
(Operator decision **OD6**). This ADR is cce's local record of that decision and
the trust boundary it draws; it does not restate the proposal.

---

## Context — who may push a corpus, to which remote, under which ACL (G18)

A knowledge corpus reaches its consumers the same way a code index does: it is
built, pushed to a git-backed CCE Sync cache, and pulled. That raises a trust
question the code path does not fully share.

**Code artifacts are rebuild-verifiable.** The index is a pure function of
`(repo content at commit, cce version, pack set, embedder)`; with the hash
embedder it is reproducible bit-for-bit across people and across the Ruby/Rust
engines (`docs/sync.md` §2). Anyone with the source can check
`artifact == build(sha)`. Trust in a pushed *code* index is therefore optional —
it is verifiable.

**Knowledge corpora have no such analogue.** The puller does not hold the source
feed (the raw tickets/docs the corpus was distilled from — deliberately ephemeral
builder input, never committed), so a knowledge corpus is **not
rebuild-verifiable by consumers** (`docs/knowledge.md` §Trust). Content-address
integrity is still checked on every pull, and `verify --checksum-only` detects
post-install corruption offline — but neither answers *"do I trust whoever
produced these bytes."* That question has to be answered by posture, not proof.

C22 already makes this tractable: redaction runs at index time, **before anything
serves or syncs** (R22), and C22 assumes **exactly one publisher per corpus**.
G18 is the open decision of who that publisher is, where it pushes, and what
gates access. Open Q5 held it open pending this write-down.

---

## Decision (OD6) — **trust-the-pusher + private-repo ACL, one publisher per corpus**

Accept the solo-operator posture and write down its boundary:

1. **One publisher per corpus.** The canonical pusher is a scheduled **CI adapter
   job** — it fetches from the source tool, emits `cce.knowledge/v1` NDJSON, runs
   `cce knowledge index` (which redacts), and runs `cce knowledge push`. Nothing
   serves knowledge at runtime; consumers pull from git like every other artifact
   (`docs/knowledge.md` §The ingestion reference). C22 already assumes exactly one
   publisher; this ADR names it.
2. **The git host's ACL is the access control.** Push to a **private** cache repo;
   whoever can pull the CCE Sync repo is the intended audience, and write access is
   scoped to the cache repo, not the source (`docs/sync.md` §8). Compartmentalized
   corpora get **one cache repo per access boundary** via the per-corpus
   `sync.remote` override — git does the isolation, not cce.
3. **Trust the pusher.** With no consumer-side rebuild check available, the
   consumer's trust rests on *who holds write access to the cache repo* plus the
   content-address integrity check on pull. That is the whole trust model, stated
   honestly rather than dressed up as verification.

**Why accept, not harden now.** Detached signatures and rebuild-verification only
pay off when a **second publisher can push** — they are insurance against a risk
that does not yet exist. For a single operator pushing to a private repo they
alone control, the git ACL *is* the authentication; adding signing machinery would
buy no security a one-publisher, private-ACL corpus does not already have, and
would add key-management surface for no threat. So the honest posture is: accept
trust-the-pusher, keep signatures a **deferred, additive** upgrade
(`docs/knowledge.md` §Trust already frames them this way).

**Revisit trigger (written down so the decision is falsifiable).** The moment a
**second publisher — a person or a machine — can push to the same corpus**, this
posture must be revisited and detached signing + rebuild-verify added. The
mechanism that makes rebuild-verify possible is **OD5's preserved byte-determinism**
(the hash embedder keeps `artifact == build(sha)` reproducible; `cce sync` refuses
non-deterministic embeddings for exactly this reason). Pusher-side determinism is
today an **audit path for feed-holders** (re-export, compare checksums), not a
consumer verification; a second publisher is what turns that audit path into a
required consumer check. Until a second publisher exists, one-publisher +
private-ACL is correct.

---

## Consequences

- **The current tooling already implements this posture** — this ADR records a
  decision, it does not request a change. `docs/knowledge.md` §Trust states
  trust-the-pusher / git-ACL-is-the-gate / content-address-integrity-on-pull /
  signatures-deferred verbatim; `docs/sync.md` §8 delegates access control to git;
  the shipped CI adapter (`docs/ci/cce-knowledge-sync.yml`) is the one-publisher
  job with two disjoint-scope secrets (source READ vs cache WRITE). No behaviour
  changes; the three gates (`cargo test`, `cargo clippy`, `cargo fmt`) are
  unperturbed because no code is touched.
- **Redaction, not signing, is the invariant that keeps secrets out of the
  cache** (C22/R22): redaction runs before any push, so the private-repo ACL is
  protecting *proprietary but already-redacted* content — the git gate matters
  because the corpus is proprietary, not because it carries secrets.
- **Compartmentalization stays git's job.** A corpus with a narrower audience than
  the code it annotates points at its own cache remote (`sync.remote`); the trust
  boundary is a repo boundary, which operators already understand.
- **Open Q5 is closed** and G18 is decided: the residual "corpus trust posture" the
  proposal carries openly is now accepted-by-ADR with a named revisit trigger,
  rather than left as an unresolved question.

## Alternatives rejected

- **Detached signatures / rebuild-verify now** — premature. They defend against a
  malicious or mistaken *second* publisher; with one publisher pushing to a private
  repo they control, the git ACL is already the authentication. Deferred as an
  additive upgrade, gated on the second-publisher revisit trigger. OD5's
  byte-determinism is preserved precisely so this upgrade stays possible.
- **Public cache repo + signatures as the gate** — inverts the model: it would make
  signing load-bearing for access control instead of git ACL, for a corpus that is
  proprietary regardless. A private repo is simpler and strictly stronger here.
- **A second, direct emit path into the corpus** (bypassing the single CI adapter)
  — breaks C22's one-publisher assumption and the R28/C19 single deterministic emit
  path; a second route is exactly the condition the revisit trigger names, not a
  design to adopt now.
- **Claim knowledge corpora are rebuild-verifiable like code** — false, and the
  tooling must never imply it: the consumer lacks the source feed. Stating the
  weaker, true posture (trust-the-pusher) is the honest record.
