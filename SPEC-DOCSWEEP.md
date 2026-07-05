# CCE v2.4 Consolidation — dashboard refresh + documentation sweep + offline-first verification (SPEC)

The CLOSING task of the v2.4 milestone. CCE MCP (v2.4.0) and CCE Sync (v2.3.0) are both merged on `main`. This is a version bump to **2.4.1**: additive dashboard/metrics work + a verified, gapless documentation sweep. Keep `SYNC_FORMAT_VERSION = "2.3"` UNCHANGED (decoupled from the app version) so the cross-engine sync golden `581cbd0f…` stays byte-identical; bump only the app/crate version to 2.4.1. Single-repo `conformance.json` stays byte-identical. Offline-first is the most important guarantee.

## Summary

The **closing task of the v2.4 milestone** (after CCE MCP  (v2.4.0, merged) merges): a full sweep of the documentation so that — with everything from language packs → secret-scrubbing → workspaces → CCE Sync → CCE MCP now shipped — the docs are current, coherent, and **gapless**. A stranger must be able to install, set up (including CCE Sync and CCE MCP), and use CCE end-to-end **from the docs alone**, and — the most important thing — **offline-first must work and be verified.**

This is not "tidy the README." It is (1) a **dashboard refresh** so it surfaces the information that is now valuable, and (2) a verified, fresh-eyes audit across the whole documentation surface — both gated by a cold-start run (online **and** offline).

## Part 1 — Dashboard refresh (show the information that's now valuable)

The dashboard was built at v1.1 and still shows only savings + retrieval quality from search metrics. Several capabilities have landed since; the dashboard must be brought up to date with what users actually want to see:

- **Per-package / per-member breakdown (workspace, v2.2):** savings, searches, and quality per member (the `by_package` section) — *where in the ecosystem is CCE helping most.*
- **Agent vs human usage (MCP, v2.4):** split CLI searches vs MCP/agent searches — *how much is my agent actually leaning on CCE.*
- **Index freshness / sync status (Sync, v2.3):** the indexed `sha`, source (local vs pulled), and whether behind the remote — *is my context current.*
- **Secret-safety reassurance (v2.1):** the sensitive-files-skipped count — *the redaction is working.*
- **Review existing panels** for continued value; drop or fix anything stale.

Enabling changes (spec them additively so old logs still parse):
- Extend the metrics event schema with the fields these panels need (e.g. `source: "cli" | "mcp"` on search events; `package` is already present; index events carry `sha`/source). Unknown/absent fields degrade gracefully.
- Keep the dashboard **loopback-only, read-only, self-contained** (unchanged posture).
- **Cross-language parity:** identical `/api/metrics` shape and panels in cce-ruby and cce-rust; refresh the committed dashboard screenshot.

## Part 2 — Documentation sweep

### Docs in scope (audit all — cumulative across every version)

- `README.md` (single-repo · workspace · sync · MCP), install, quickstart, usage examples
- `docs/`: getting-started, architecture, workspace, `sync.md`, MCP/agents, how-to, dashboard, adding-a-language, DECISIONS, BENCHMARKS, VERIFIED
- `SECURITY.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `CITATION.cff`, `llms.txt`, `AGENTS.md`

## Requirements

1. **Everything up to date.** Every command, flag, and output example reflects shipped v2.4 behaviour — no stale references to removed/renamed things. Cross-file consistency: versions, repo URLs, and command names agree everywhere (the "consistency across files is the real work" lesson).
2. **Usage examples set.** Worked, copy-pasteable examples with **real captured output** for each of: single repo · workspace/ecosystem · CCE Sync (`init`/`push`/`pull` + the CI recipe) · CCE MCP (`init` + editor wiring + confirming the agent used it) · the dashboard.
3. **Setup without gaps.** Install + environment setup for **macOS and Ubuntu**, prerequisites explicit (toolchain, C compiler, **git, git-LFS**), each verified from a cold start. No "obvious" step left implicit.
4. **Easy install.** The simplest install path front-and-centre, plus a one-command quickstart.
5. **Best practices for CCE Sync and CCE MCP.** A dedicated "best practices" section: one sync repo per access boundary, CI as the canonical pusher, `.gitignore .cce/`, when to use a workspace vs a single repo, wiring MCP + confirming usage via the dashboard, and the secret-safe-by-default posture.
6. **Offline-first — THE most important.** A dedicated, **VERIFIED** section proving every core workflow runs with **no network and no remote**: `index`, `search`, `stats`, `dashboard`, `workspace`, and **MCP against the local index**. Explicitly document the *only* things that touch the network — the optional Ollama embedder, `cce sync push/pull`, and installing the binary/gem — and state plainly that **everything else works fully offline.**

## Verification — the sweep is NOT done until

- **Fresh-eyes cold start (online):** a reader who has never used CCE installs → sets up (incl. Sync + MCP) → uses it, from the docs alone, with zero friction.
- **Offline cold start (mandatory):** with the network disabled and no sync remote configured, `index` + `search` + `stats` + `dashboard` + `workspace` + `cce mcp` (serving the local index) all work exactly as documented. Recorded in `docs/VERIFIED.md`.
- **Every documented code example runs verbatim** (a doc example that doesn't run is a bug — fix the doc or the code).
- **Cross-file consistency pass** (versions, URLs, command names, feature lists).
- `llms.txt` and `AGENTS.md` reflect the full v2.4 surface.

## Acceptance

- [ ] **Dashboard refreshed:** per-package breakdown, agent-vs-CLI split, index freshness/sync status, and sensitive-skipped count are shown; stale panels fixed/removed; metrics schema extended additively; screenshots + `/api/metrics` docs updated; cross-language parity.
- [ ] All docs current and internally consistent (version/URL/command/feature checks pass).
- [ ] Worked, output-backed examples for single-repo, workspace, Sync, MCP, dashboard.
- [ ] macOS + Ubuntu install/setup verified from cold start; git-LFS covered.
- [ ] Best-practices section for Sync + MCP.
- [ ] **Offline-first section, with a recorded offline cold-start run proving index/search/stats/dashboard/workspace/MCP work with no network.**
- [ ] `docs/VERIFIED.md` updated with both the online and offline cold-start transcripts.

## Notes

- Runs **after** CCE MCP ( (v2.4.0, merged)) and CCE Sync merge; it is the final gate of the v2.4 milestone.
- Sibling repo (same sweep, same bar): davidslv/cce-ruby
