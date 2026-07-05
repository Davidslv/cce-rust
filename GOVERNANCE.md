# Governance

This document describes, honestly, how decisions are made in **cce-rust**.

## Model: single maintainer (BDFL)

cce-rust is a solo project. [David Silva](https://davidslv.uk)
([@davidslv](https://github.com/davidslv)) is the sole maintainer and acts as
Benevolent Dictator For Life (BDFL): final say on scope, design, and what merges
rests with him. There is no committee, no voting, and no service-level agreement.
This is stated plainly so contributors know what to expect — see
[SUPPORT.md](SUPPORT.md).

## The specification is the constitution

The real authority in this project is not a person but a document:
[`SPEC.md`](SPEC.md) (SPEC v1.0). cce-rust is a **clean-room implementation** of
that spec, built test-first, and so is its Ruby sibling
([davidslv/cce-ruby](https://github.com/davidslv/cce-ruby)). Decisions are
resolved by appeal to the spec:

- If the spec is clear, the implementation follows it. Deviations are bugs.
- If the spec is ambiguous, the ambiguity is resolved to the simplest reasonable
  interpretation and **recorded** in [`docs/DECISIONS.md`](docs/DECISIONS.md).
- Behaviour that would change [`conformance.json`](conformance.json) — and thus
  drift from the Ruby sibling — is a spec-level matter, not a casual code change.

## How decisions are made

1. **Discussion.** Non-trivial changes start as a GitHub issue. Anyone may
   propose; the maintainer responds as time allows.
2. **Proposal.** For behaviour changes, the case is argued against `SPEC.md`. If
   it resolves an ambiguity, the resolution is captured in `docs/DECISIONS.md`.
3. **Decision.** The maintainer accepts, requests changes, or declines, with a
   reason. Silence is not consent — ping politely if an issue stalls.
4. **Implementation.** Changes land via PR, keeping the three quality gates green
   (test / clippy / fmt) and coverage intact — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Evolution path

This model is deliberate for a small, spec-anchored project. If cce-rust grew a
sustained community of regular contributors, the intended evolution is:

- Recognise consistent contributors as maintainers in [MAINTAINERS.md](MAINTAINERS.md).
- Move from BDFL toward lazy-consensus among maintainers for day-to-day changes,
  reserving the spec as the tie-breaker.
- Version the spec explicitly (it is already `v1.0`) so behaviour changes are
  proposed as spec revisions with matching conformance updates across both
  implementations.

Until then: one maintainer, the spec as constitution, and honesty about both.
