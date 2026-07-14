# ADR — Search-quality ceiling: BM25 + facets is the shared-path ceiling, semantic ranking deferred

**Status:** Accepted (2026-07-14). Records a decision about **what not to build
yet**; it is a ceiling, not a feature — no behaviour changes with this ADR. The
current retrieval already *is* this ceiling (see *Consequences*).

**Traceability:** Epic [Davidslv/signal-engine#8](https://github.com/Davidslv/signal-engine/issues/8),
ticket **U9.5** — gap **G17**; resolves **Open Q4**. The authoritative dossier and
requirement text live in
[`notes/context-engine/unified-proposal.md`](https://github.com/Davidslv/notes/blob/main/context-engine/unified-proposal.md)
(Operator decision **OD5**). This ADR is cce's local record of that decision and the
determinism constraint that makes it load-bearing; it does not restate the proposal.

---

## Context — should the shared retrieval path buy semantic ranking now (G17)

Both consumers of cce's index — `cce search` over code, and the M4 knowledge blend
that feeds signal-engine's triage over `cce corpus serve` — retrieve through **one
shared path**: the deterministic hash embedder + BM25 + Reciprocal Rank Fusion of
`SPEC.md` §6. Knowledge search added no bespoke scorer; since v2.6.1 it runs the
**exact same hybrid retrieval as code** (`docs/knowledge.md` §"Searching knowledge
(M4)"). So "how good is retrieval" is a single question asked once, not per surface.

That raises the standing temptation to reach for **semantic embeddings** — a learned
vector model (e.g. the optional Ollama embedder, `SPEC.md` §5/§11) in place of the
hash embedder — on the theory that dense vectors rank a small doc corpus better than
lexical BM25. G17 is the open decision of whether to spend that now. Open Q4 held it
open pending this write-down.

Two facts frame the call:

- **The corpus is small, curated, and consistent in vocabulary.** Knowledge chunks
  are distilled ADRs/runbooks/tickets with facets attached at index time — `source`,
  `url`, `state`, `state_reason`, `updated_at`, `group`, `labels`, `title`, `links`,
  and the record `id` (`docs/knowledge.md` §M3) — and served scoped: the corpus-serve
  surface answers `GET /docs?service=<name>`, passing the service name as the query
  (`src/corpus.rs`). "Service-registry facets" is OD5's phrase for exactly this
  lexical-plus-scoped shape. Lexical match over a controlled vocabulary, gated by the
  L5 precision floor, is a strong baseline; the failure mode dense vectors fix
  (synonym/paraphrase gap over a large, uncontrolled corpus) is not the regime here.
- **Semantic embeddings break the property the distribution model depends on.** The
  index is a pure function of `(repo content at commit, cce version, pack set,
  embedder)` and is therefore content-addressable and reproducible bit-for-bit
  across people and across the Ruby/Rust engines — but **only with the hash
  embedder** (`docs/sync.md` §2). Ollama/semantic indexes are non-reproducible, and
  `cce sync push` **refuses** them: they are local-only (`docs/sync.md` §1). A `.cck`
  corpus is *served* to signal-engine and *pulled* by consumers; making semantic
  ranking the shared-path ranker would break `.cck` hash-determinism — the very
  thing that lets a corpus be shared, integrity-checked, and (per OD6) one day
  rebuild-verified.

---

## Decision (OD5) — **accept the BM25 + facets ceiling; defer semantic ranking**

Document **lexical BM25 + service-registry facets** as the shared-path retrieval
ceiling and do **not** pursue semantic embeddings on that path now:

1. **BM25 + RRF + facets is the ceiling for both surfaces.** The shared hash
   embedder + BM25 + RRF path, scoped by service on the corpus-serve surface and
   gated by the L5 precision floor (`knowledge.min_score` default **0.30** plus a
   shared-query-token requirement, `docs/knowledge.md` §"Staleness weighting"), is
   accepted as good-enough for the small curated corpus. No second, knowledge-only
   ranker is introduced; the one shared ranking stays the one shared ranking.
2. **Semantic embeddings stay a local-only opt-in, never on the shared path.** The
   Ollama embedder remains available for a local index but is never the ranker
   behind a `.cck` that is pushed, served, or pulled — `cce sync push` already
   enforces this by refusing non-reproducible indexes. Determinism on the shared
   path is preserved deliberately.
3. **The trigger to reopen is a measured retrieval failure, not a hunch.** Buying
   semantic search before the benchmark that would justify it exists is premature;
   the M3 scorecard is that benchmark.

**Why accept, not build now.** For a small, curated, consistent-vocabulary corpus,
lexical + facets serves it well, and the cost of going semantic is not just model
plumbing — it is forfeiting `.cck` reproducibility, the property that makes the
corpus shareable at all and that OD6's future rebuild-verify hardening is built on.
Spending that before any evidence shows retrieval (not model reasoning) is the
bottleneck would be buying capability against a problem not yet demonstrated.

**Revisit trigger (written down so the decision is falsifiable).** The **M3
with/without-corpus scorecard's retrieval-usefulness signal** (signal-engine Epic #8
· U3.2, issue #18). If shadow mode shows triage failing because the **right document
was not retrieved** — not because the model reasoned badly over a document that
*was* retrieved — that is the evidence that funds semantic ranking. At that point the
determinism trade-off is reconsidered with a number in hand: either a semantic ranker
scoped to the local/non-shared path, or an explicit decision to break `.cck`
determinism for the corpus with eyes open. Until that signal appears, BM25 + facets
is correct.

---

## Consequences

- **The current retrieval already implements this ceiling** — this ADR records a
  decision, it does not request a change. Knowledge and code share the hash embedder
  + BM25 + RRF path (`docs/knowledge.md` §M4); facets and the L5 precision floor
  already do the scoping; `cce sync push` already refuses non-deterministic
  embeddings (`docs/sync.md` §1). No code is touched, so the three gates (`cargo
  test`, `cargo clippy`, `cargo fmt`) are unperturbed.
- **`.cck` byte-determinism is preserved on purpose.** Keeping the shared path on the
  hash embedder is what keeps a corpus content-addressable and shareable, and keeps
  OD6's deferred rebuild-verify (ADR-CORPUS-TRUST) *possible* — semantic ranking on
  the shared path would foreclose it. The two ADRs are linked by this one property.
- **The revisit trigger is a real, funded signal, not "someday."** M3 (U3.2) is a
  built scorecard; its retrieval-usefulness output is the specific input that would
  reopen this decision, so the ceiling is falsifiable rather than permanent.
- **Open Q4 is closed** and G17 is decided: the residual "semantic-index decision"
  the proposal carried openly is now accepted-by-ADR with a named, measurable revisit
  trigger.

## Alternatives rejected

- **Adopt semantic embeddings on the shared path now** — premature and
  determinism-breaking. It defends against a synonym/paraphrase gap not shown to
  exist over this small curated corpus, and it forfeits `.cck` reproducibility (the
  cache stops being content-addressable; `cce sync push` refuses it), which the
  distribution model and OD6's rebuild-verify future both depend on. Deferred behind
  the M3 retrieval-usefulness trigger.
- **A bespoke knowledge-only scorer, separate from code retrieval** — rejected by the
  v2.6.1 design that made knowledge reuse the one shared ranking. A second ranker
  would fork the retrieval path, break Ruby/Rust conformance parity, and mean an
  operator's local `cce search` and a consumer's served corpus rank by different
  rules — two things to reason about where there is now one.
- **Semantic locally, hash on the sync path** — two rankers again, and a gap between
  what an operator searches locally and what consumers actually receive; the honest
  single-ranker posture is stronger until a measured reason to split appears.
- **Leave the question open (do nothing)** — the one disallowed answer. An
  unresolved "maybe semantic later" keeps the temptation live and unbudgeted; naming
  the ceiling and its trigger converts an open question into a decision with a test.
