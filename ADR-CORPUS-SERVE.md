# ADR — Corpus-serve bridge: native subcommand, one box, in-process invocation

**Status:** Accepted (2026-07-13). Records the decision **before code**; the
implementation is a later ticket (signal-engine Epic #8 → U1.3 / issue #11).

**Traceability:** Epic [Davidslv/signal-engine#8](https://github.com/Davidslv/signal-engine/issues/8),
ticket **U1.1** — constraints **C9, C10, R9, R20**; gap **G1**. The authoritative
dossier and requirement text live in
[`notes/context-engine/unified-proposal.md`](https://github.com/Davidslv/notes/blob/main/context-engine/unified-proposal.md)
(Operator decisions **OD1** and **OD2**). This ADR is cce's local record of those
two decisions and the invocation path they imply; it does not restate the
proposal.

---

## Context — the seam the whole programme exists to feed (G1)

Four repos are individually green but have never run as one system. The one seam
that carries the product's entire value is a protocol mismatch:

- **signal-engine** (the consumer) enriches triage by calling a corpus over plain
  HTTP: `GET /docs?service=<name>`.
- **cce** (the producer) serves its index over **MCP (JSON-RPC 2.0) on stdio**
  and exposes **no `/docs` route anywhere** (C10/C11, verified against this repo).

Wired together as shipped, nothing errors: signal-engine's corpus client fails
open, stamps `corpus_degraded: true`, logs one line, and triages with **zero
business context — forever**. The moat is silently absent. No service on either
side bridges this seam today; the only corpus responders ever exercised are
signal-engine's own test mocks. **G1 is therefore a blocker, not a gap.**

Two decisions had to be made before any bridge code: what *shape* the bridge
takes (OD1), and *where* it runs (OD2).

---

## Decision 1 (OD1) — Bridge shape: **native `cce corpus serve` subcommand**

Add `cce corpus serve --dir <store>` as a subcommand of cce — **not** a standalone
process sitting beside it. One binary, no middleman, no side-channel.

**Why native, not a standalone process.** A separate process could only reach
cce's retrieval two ways, both worse than a route on the machinery that already
holds the index:

1. shell out to `cce ...` per request (a fork + full process start per query), or
2. embed the cce crate itself — at which point it *is* cce, just with a second
   copy of the binary to version and deploy.

Putting the route inside cce reuses machinery that already exists and is already
tested:

- **The loopback HTTP server is precedent, not new invention.** `src/dashboard.rs`
  already runs a hand-rolled HTTP/1.1 server bound to `127.0.0.1` (`serve(listener,
  …)`, and `TcpListener::bind(("127.0.0.1", port))`), serving **read-only** `GET`
  routes (`/`, `/api/metrics`, `/api/health`). `cce corpus serve` is the same
  shape — a small, read-only, loopback-bound GET surface — reusing that pattern.
- **Retrieval is an in-process call with no side-channel.** The serve route calls
  the existing knowledge retrieval directly: `src/knowledge/retrieval.rs`
  (`search_knowledge(&KnowledgeStore, query, top_k, min_score) ->
  Vec<KnowledgeHit>`) over the store that `cce knowledge index` writes to
  `<dir>/.cce/knowledge/` from a `cce.knowledge/v1` feed. No CLI shell-out, no
  temp files, no second index — the route is a thin HTTP adapter over a function
  cce already ships. This is the exact "CCE retrieval invocation with no
  side-channel across C9/C10" that U1.1 asks the ADR to fix.

**Offline-first is preserved (the footnote that keeps the invariant honest).**
cce is offline-by-default; the Ollama embedder is the only pre-existing opt-in
network exception. The serve route is the *second* opt-in exception, and it is
constrained symmetrically:

> **`cce update` is the only egress; `cce corpus serve` is the only ingress —
> opt-in, bind-configurable, and off by default.**

The route is not compiled into the default path of any existing command, does not
start unless explicitly invoked, and binds where the operator configures (loopback
by default). With the route absent or unstarted, cce behaves exactly as today.

**Follow-on this decision creates:** once the serve route lands, cce's CI inherits
a small **auth + TLS conformance check** for it (tracked as its own ticket,
signal-engine #12/#14). The route is authenticated and read-only by construction —
it only ever returns what is already redacted in the store at index time (v2.1),
so it introduces no new secret-exposure surface.

---

## Decision 2 (OD2) — Bridge placement: **one box, co-located with signal-engine**

`cce corpus serve` runs on the **single signal-engine host**, alongside the engine
itself and Ollama (dev/fallback). The knowledge store arrives on that host via the
nightly `.cck` pull (`cce update` / sync).

**Why one box.**

- **Corpus retrieval becomes a loopback hop.** signal-engine → `cce corpus serve`
  is `127.0.0.1`, so the corpus call carries **no network** in the availability
  math and sits comfortably inside `LLM_TIMEOUT_MS`. Fewer moving parts in the hot
  path of every triage.
- **One host to operate.** One thing to back up, restart, and reason about — which
  matches the engine's own single-writer, single-instance, "treat restart as the
  deployment model" design.
- **Staleness is cheap here.** ADRs, runbooks, and policy docs change daily at
  most; a few hours of knowledge staleness between nightly pulls costs nothing.

**R20 is satisfied by co-location.** R20 forbids the bridge from living in CI or
serverless, and requires it to sit **outside the watched app's blast radius**.
Placing it on the engine's own always-on host (never in the watched app, never in
CI, never serverless) answers R20's operator/restart/rollback questions directly:

| R20 question | Answer |
|---|---|
| Who operates it? | The signal-engine operator, same host as the engine. |
| Restart policy | `Restart=always` (systemd unit, same as the engine). |
| Rollback | Redeploy the previous cce tag; the store is unaffected. |
| Blast radius | The watcher's host, never the watched app's. |

**Revisit trigger (written down so the decision is falsifiable).** The moment a
**second consumer** appears for the knowledge host — a teammate's agent, a second
signal-engine instance — this co-location should be revisited and the knowledge
host split out. Until then, one box is correct.

---

## Consequences

- **Unblocks the implementation ticket** (U1.3 / signal-engine #11): the bridge is
  now specified as **native + one-box + loopback**, so the first bridge ticket is
  unambiguous — implement `cce corpus serve` as an authenticated, read-only,
  loopback GET route over `search_knowledge`, and replace signal-engine's corpus
  mocks with an integration test against it.
- **cce gains a second opt-in network exception**, bounded by the offline-first
  footnote above; the offline-by-default invariant for every other command holds.
- **cce CI gains an auth/TLS conformance check** once the route exists (#12/#14).
- **The seam mismatch is resolved at the producer**, not by teaching signal-engine
  MCP: MCP stays stdio (C10/C13); the bridge is one narrow, authenticated,
  read-only HTTP facade over the same in-process retrieval — nothing here makes cce
  a multi-tenant network brain.

## Alternatives rejected

- **Standalone bridge process** — worse coupling (shell-out or crate-embed per
  request) for no isolation benefit on a single box. See OD1.
- **Teach signal-engine to speak MCP-over-stdio** — pushes protocol complexity
  into the consumer and couples it to cce's transport; the seam is better closed by
  one small HTTP route at the producer.
- **MCP-over-HTTP / multi-tenant network CCE** — out of scope and explicitly not a
  goal; the bridge is a narrow read-only facade, MCP stays stdio.
- **Separate knowledge host now** — premature; justified only once a second
  consumer exists (the revisit trigger above).
