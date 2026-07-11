# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`cce relevance --compare` — paired significance testing (#84).** The
  comparison mode now reports, per metric, the paired t-statistic over the
  per-query deltas, the two-sided p-value at `n−1` degrees of freedom, the
  95% confidence interval on the mean delta, and `n` — in the human table
  (an `n/a` marks a statistic undefined at the input, e.g. `t` at zero
  variance) and in the `--json` report's new top-level `compare` block
  (`null` there). The math is a dependency-free, closed-form t-distribution
  CDF (log-gamma + regularized incomplete beta, new `src/stats.rs`) — chosen
  over a fixed-seed permutation test because it needs no seed and no
  resampling knob to pin in a golden, and an exact sign-flip permutation at
  n=6 quantizes p to 1/64 steps. Hand-computed unit tests against exact
  small-df closed forms and the classic t-table values. `docs/relevance.md`
  gains a minimum-detectable-effect note: at n=6–7 only deltas ≈1.4× the
  per-query delta SD are detectable, so private fixture sets should be sized
  from a pilot variance estimate (Sakai's topic-set-size methodology;
  Urbano et al. on small-n test behavior).
- **`cce relevance` — line-range anchors + token-level metrics (#85).** The
  anchor grammar gains an additive line-range facet: `path@a-b` and
  `path#kind@a-b` (1-based, inclusive) match only results whose line span
  overlaps the range — text after the last `@` is a range only when it is
  all digits-and-dashes, so every existing `cce.relevance/v1` fixture parses
  unchanged (the fixture schema does NOT bump). Cases with ranged anchors
  are additionally scored at token resolution — token-level recall /
  precision / IoU between the expected spans and the top-k result spans,
  weighted per line with the ONE `cce.tokens/v1` estimator over the indexed
  chunk texts (uncovered lines weigh the estimator floor of 1) — rendered as
  a token-level section in the human summary and `tokens` objects in the
  JSON report; unranged cases and sets score and render exactly as before.
  The code starter set gains one ranged case (`python.py@3-4`) so the pinned
  golden exercises the new path.
- **`cce.relevance.report/v2`** — the `--json` report schema bumps once for
  both features above; every v1 field is carried unchanged, and the harness
  golden (`test/fixture/relevance/code.golden.json`) is re-pinned.
  New-goldens-only: zero ranking bytes touched, `cce conformance` output over
  `test/fixture/samples` verified byte-identical.

### Fixed
- **Blended `context_search` no longer silently swallows a knowledge-store load
  failure (#143) — the knowledge-side mirror of #132.** A corrupt-but-present
  knowledge store (`current` pointer intact, snapshot unparseable) was mapped
  `load_current` `Err → None → empty`, so the default blend routed to code-only
  and served a confident answer with `isError:false` and no hint that knowledge
  context was missing — even explicit `source:"knowledge"` masked the corruption
  as "0 chunk(s)". The fix splits absent from failed exactly as #132 did for the
  code side: a store that was NEVER ingested stays silent (knowledge-only-absent
  is then the correct, complete answer), but a store that EXISTS and fails to load
  (`InvalidData`) now routes to the knowledge-aware default AND prepends the pinned
  `KNOWLEDGE_STORE_LOAD_ERROR_NOTICE` through the same visible-degradation notice
  channel (code hits still served, `isError` still reserved for malformed calls).
  Code-store and knowledge-store corruption are now surfaced symmetrically, closing
  the asymmetry #132 left. Healthy- and absent-path output is unchanged.
- **Sync pull no longer rejects a valid pushed artifact as a "checksum
  mismatch" (#115).** `Artifact::from_bytes` recomputed the checksum by
  re-serializing the artifact, but reconstructing `file_imports` from the
  resolved graph edges is lossy (each target file is reduced to a module name),
  so a module resolvable only via the `.py`/`.js` path-suffix fallback
  re-resolved to a *different* same-stem corpus file on the pull side, produced a
  different graph line, and diverged the recomputed checksum — permanently
  breaking pull for that `repo@sha` with an error falsely implying corruption.
  Integrity is now verified over the canonical *received* bytes (SHA-256 with the
  manifest `checksum` blanked), so a byte-identical push always round-trips while
  genuine corruption is still caught. Canonical pushed bytes and the cross-engine
  golden are unchanged.
- **`is_dirty` no longer misparses the first `git status --porcelain` line,
  keeping the `.cce/` churn exemption correct both ways (#117).** `run()` trims
  stdout, which strips the leading status column off the first porcelain line
  (` M .cce/index.json` → `M .cce/index.json`), so the fixed 3-char path offset
  read the wrong substring: a tracked-modified `.cce/` file was misclassified as
  real dirt (wrongly blocking `cce sync push`), and a change under a `..cce/`
  path was misclassified as ignorable churn (wrongly publishing from a dirty
  tree). `is_dirty` now reads raw, untrimmed porcelain output so every line keeps
  its status columns.
- **Workspace member-name suffixing is now collision-free against real
  sibling directories (#131).** `detect_members` minted `basename-N` suffixes
  by a per-basename counter without checking the result against other members'
  basenames, so a workspace with `a/widget`, `b/widget`, and a directory
  literally named `widget-2` produced TWO members named `widget-2` — violating
  the documented "unique member id" invariant, and making federation namespace
  two members' chunks under one prefix and attribute results to the wrong
  package. Suffixing now skips any candidate that is already assigned OR
  reserved as another member's natural basename, so ids are always unique.
- **Walker no longer silently drops traversal errors (#133).** The walk loop
  used `walker.flatten()`, which discards the `ignore` crate's `Err` entries —
  so a permission-denied or otherwise unreadable directory made every file
  beneath it vanish from the index with nothing recorded: `skipped` stayed 0,
  exit was clean, and the artifact silently diverged across machines with
  different permissions (the same builder-independence violation class as #24).
  The loop now matches the `Err` arm and tallies it in a new
  `WalkResult::walk_errors` counter (separate from `skipped`, since these are
  directory-level failures, not per-file skips). The count is threaded through
  `BuildStats` and surfaced to the operator: `cce index` prints a `walk errors`
  summary line and emits a stderr warning ("N directory(ies) could not be
  read; files under them were NOT indexed…") whenever it is nonzero, and the
  workspace index warns per member — so the loss and the cross-machine
  divergence are visible, not silent. When there are no traversal errors the
  summary is byte-identical to before.
- **Walker path normalisation no longer conflates distinct Unix filenames
  (#105).** The relative `file_path` was built with `replace('\\', "/")`, which
  rewrites the literal backslash — a legal filename byte on macOS/Linux — so a
  root file named `a\b.py` collapsed onto the same path as a nested `a/b.py`,
  giving two files identical, order-nondeterministic provenance. The walker
  (and the workspace member-path builder, the same defect) now normalise ONLY
  the platform separator (`std::path::MAIN_SEPARATOR`): a no-op on Unix that
  preserves backslashes, still `\`→`/` on Windows where `\` is never a filename
  byte.
- **A valid-JSON request with an id but no string `method` now echoes the id as
  a `-32600` Invalid Request instead of a null-id parse error (#125).**
  `parse_request` flattened "invalid JSON" and "valid JSON, wrong shape" into one
  error, so `handle_line` answered a request like `{"jsonrpc":"2.0","id":7,"method":5}`
  with `-32700` and `id:null` — a conforming client correlates responses by id,
  cannot match `id:null` to request 7, and leaves the call pending until timeout.
  `parse_request` now returns a `ParseError` that distinguishes the two: non-JSON
  stays `-32700` with a null id, while valid JSON that is not a well-formed
  request is `-32600` "invalid request" with the recoverable id echoed, so the
  client can correlate and fail the call cleanly.
- **One invalid-UTF-8 byte on stdin no longer kills the whole MCP session
  (#124).** The `cce mcp` read loop read each line into a `String` with
  `read_line`, which propagates an `InvalidData` ("stream did not contain valid
  UTF-8") error the moment a line carries a stray non-UTF-8 byte; that error
  unwound out of `run`, `serve` turned it into process death, and every
  in-flight and subsequent request went unanswered — while every other form of
  garbage input got a graceful `-32700`. The loop now reads raw bytes with
  `read_until(b'\n', …)` and validates them: a non-UTF-8 line is answered with a
  `-32700` parse error (JSON mandates UTF-8) and the session keeps serving the
  next request. stdout stays pure JSON-RPC — the parse error is a protocol
  response, not a stderr diagnostic.
- **`is_code_lookup` now conforms to SPEC §6.1 boundary semantics (#107).**
  The extension check examined only the FIRST `.ext` occurrence, so a later
  genuine extension token was shadowed by an earlier non-boundary match
  ("render in app.tsx or util.ts", "main.python auth.py" → GENERAL instead of
  CODE_LOOKUP); it now scans every occurrence per the regex `\.(...)\b`. The
  `.* defined` phrase used `contains("defined")`, firing on "undefined"/
  "predefined"; it now requires the space the spec mandates. The folded
  `find .* function` sibling likewise now requires a space before "function"
  (no longer matching an embedded "malfunction"). This drives `fts_weight`
  (1.5 vs 1.0), so misclassification skewed every BM25 RRF contribution.
- **`rank_core` with `top_k=0` now returns an empty result set (#109).** The
  diversity-cap loop pushed a candidate before testing `kept.len() >= top_k`,
  and the `(top_k * CANDIDATE_MULTIPLIER).max(1)` candidate floor always
  supplied one candidate, so `cce search --top-k 0` returned a phantom result
  (and logged `result_count=1`) while `bm25_only_search` returned none. The cap
  is now checked before keeping, so both pipelines agree on the degenerate input.
- **An empty query vector now genuinely disables vector recall (#110).** When
  the query embedding was unavailable at query time (e.g. Ollama died inside the
  ping→embed TOCTOU window and `embed` returned an empty vector), `rank_by_cosine`
  scored every chunk 0.0 and tie-broke by `chunk_id`, so `rank_core` handed full
  RRF vector-rank credit to the lexicographically smallest chunk_ids — surfacing
  alphabetical noise with confident-looking scores while the warning claimed
  "vector recall disabled". `rank_core` now gathers no vector candidates when the
  query vector is empty, leaving BM25 as the sole recall source (a zero-overlap
  query returns empty, matching `bm25_only_search`).
- **Per-store fingerprint filename so two named stores in one directory no
  longer clobber each other's fingerprint (#100).** `beside_store` in
  `src/fingerprint.rs` resolved every store in a directory to the same constant
  `fingerprint.json`, so `cce index dirB --store D/b.json` overwrote the
  fingerprint written for `D/a.json` and `cce doctor --store D/a.json` then
  reported a permanent false "store bytes do not match the fingerprint"
  corruption that re-indexing could never clear. The canonical `index.json`
  store keeps the historical `fingerprint.json` (existing stores resolve
  unchanged), but a named store now derives a per-store
  `<file-name>.fingerprint.json`, so colocated stores keep independent
  fingerprints. (The non-atomic write half of the report was already closed by
  #101, which routes the fingerprint write through `crate::atomic::atomic_write`.)
- **`recall()` filters before it truncates, so valid memories are no longer
  starved by coincidental non-matches (#103).** `src/memory.rs` passed `top_k`
  into `search`, which truncated the candidate list to `top_k` BEFORE the
  precision filter (score floor AND shared-token) ran — contradicting the
  documented rank-generously-then-filter-then-truncate contract. A no-token
  vector coincidence inside the `top_k` window could consume a slot and drop a
  qualifying entry ranked just below it. Recall now ranks the whole corpus,
  filters, then truncates to `top_k`.
- **Punt phrase `n/a` no longer substring-matches file paths, so correct
  answers are not silently misgraded as punts (#106).** `is_punt` in
  `src/eval.rs` did a raw case-insensitive substring scan, so an answer citing a
  multi-segment path like `common/auth.py` (`…commo·n/a·uth…`) graded `Punt`,
  zeroing the correctness-gated A/B paired set. Punt phrases now match only on
  non-alphanumeric word boundaries, so `N/A` as a real non-answer still fires
  while a path segment never does.
- **Paired-t constancy guard tolerates rounding residue, so mathematically-equal
  deltas from different bases no longer emit a saturating t (#108).** `paired_t`
  in `src/stats.rs` used an exact bit-identity check, so per-query deltas that
  are the same value computed from different bases (`0.6−0.4` vs `0.2−0.0`)
  slipped through and their ~1e-17 variance produced t ≈ 1e16, rendered as the
  i64-saturated `+9223372036854.775807` in the byte-pinnable
  `cce.relevance.report/v2`. The guard now treats deltas whose spread is within
  a few ULPs of their magnitude as constant (t = `n/a`, p = 0, CI = [mean, mean]).
- **Newline in a knowledge record title can no longer break the single-line
  provenance grammar or inject a fake result line (#112).** `provenance_line` in
  `src/knowledge/retrieval.rs` interpolated the title (and facets) unsanitized,
  so a feed-controlled title containing `\n` produced a multi-line MCP
  `context_search` header, letting attacker-controlled data spoof extra
  ranked-result/heading lines. Free-text provenance fields now neutralize any
  control character — plus the Unicode line/paragraph separators U+2028/U+2029,
  which are line terminators for non-terminal consumers yet not `is_control()` —
  to a space; a clean title stays byte-identical, so every pinned provenance
  golden is unchanged.
- **`SyncConfig::load` surfaces a malformed project config instead of silently
  using the global remote (#119).** `load` in `src/sync/config.rs` mapped a
  failed parse of an existing `.cce/config` to `None` via `.ok()`, treating a
  typo'd config exactly like an absent one and falling back to the global
  remote — so `sync push`/`pull` could target the wrong cache with no warning. A
  project config that exists but does not parse is now surfaced with a clear
  warning and the load stays all-local, so the misconfiguration cannot be masked
  by an unrelated global remote.
- **The dashboard sets a read timeout so an idle or half-open connection can no
  longer hang the server (#128).** `handle_connection_with` blocked forever in
  `read_line` on a connection that sent no bytes (e.g. a browser speculative
- **The dashboard sets a read timeout so an idle connection that sends no bytes
  can no longer hang the server (#128).** `handle_connection_with` blocked forever
  in `read_line` on a connection that sent no bytes (e.g. a browser speculative
  preconnect); because connections are served serially, one such socket stalled
  every later request indefinitely. The request-line read now has a bounded
  `set_read_timeout`, so a fully-idle socket is dropped after the timeout and the
  server recovers. The normal request path is unaffected (covered by a new
  real-socket serve test). Note this is a per-read deadline (it resets on each
  byte received), so it fixes the reported zero-byte / preconnect hang but not a
  slowloris that dribbles one byte per window; a whole-request deadline is tracked
  as follow-up #160.
- **`parse_iso` applies an explicit timezone offset and rejects impossible
  calendar dates (#130).** The ISO parser validated only the first 19 bytes and
  ignored the timezone suffix, so `--since 2026-07-01T09:45:00+09:00` was read as
  `09:45Z` (should be `00:45Z`), silently excluding in-window events, and
  `2026-02-31` was accepted and normalized to March 3. It now parses and applies
  `Z` / `±HH` / `±HHMM` / `±HH:MM` offsets, rejects any other trailing text, and
  rejects a day that fails the civil-date round-trip. Self-written timestamps
  (always `…Z`) are unaffected. Behavior change: a `--since` value that was
  previously accepted loosely — an impossible date like `2026-02-31` (silently
  normalized to March 3) or a fractional-second instant like
  `2026-07-01T09:45:00.500Z` — is now rejected with a clean `invalid --since`
  error instead of being accepted or misread.
- **`cce usage --since` rejects an overflowing relative duration with a clear
  error instead of panicking or wrapping (#129).** `parse_since` in
  `src/usage.rs` multiplied the parsed count by the unit seconds with unchecked
  i64 arithmetic, so a count like `999999999999999d` panicked in debug and
  wrapped to a garbage cutoff in release. It now uses `checked_mul` (surfacing
  the standard `invalid --since` guidance on overflow) and a saturating
  subtraction for the cutoff. Verified in a release build: the overflow is an
  error, not a wrapped cutoff.
- **Aggregate roll-ups sum `tokens_saved` (and `sensitive_skipped`) with
  saturating u64 arithmetic, so a corrupt or forged log value can no longer
  overflow-panic `cce usage` / kill the dashboard process in debug, nor wrap to
  a garbage total in release (#127).** `compute_totals`, `source_usage`,
  `savings_window`, `compute_secret_safety` and the daily accumulator in
  `src/aggregator.rs` now sum through a shared clamping fold (clamping at
  `u64::MAX`), matching the saturating policy already used in `savings.rs`. When
  the clamp actually engages the fold emits a warning to STDERR (never STDOUT — it
  can run under the dashboard), so the resulting nonsense figure that flows into
  `cost_saved_usd` is no longer silent. Verified in a release build: the total
  clamps instead of wrapping.
- **`cce sync push` from a detached HEAD no longer silently rewinds
  `refs/main` (#151).** `push_one` derived the pointer branch as
  `current_branch(root).unwrap_or("main")`, so a detached checkout
  (`git checkout <old-sha>`) published an honest artifact but rewound
  `refs/main` to the old sha — silently rewinding every consumer's `--latest`.
  Push now resolves the branch via an explicit `sync.ref` (the CI contract),
  else the checked-out branch, and when HEAD is detached with no resolvable ref
  it **refuses — advancing no pointer** — rather than rewinding one it cannot
  attribute (fully closing #151, not deferring it). A CI job on a detached-at-SHA
  checkout sets `sync.ref` to publish; a push-event checkout on an attached
  branch is unaffected.
- **`cce sync pull --workspace` now honours the §9.4 `--force` guard per
  member (#118).** The workspace branch returned before the guard and never
  consulted `SyncState`, so a member pinned via `cce sync pull --commit <sha>`
  was silently clobbered by a root `pull --workspace --latest`. Each member is
  now checked against its local marker and refuses without `--force`, exactly
  as the single-repo path does.
- **`resolve_pull_sha` distinguishes a `refs/main` read failure from pointer
  absence (#120).** A transient IO/permission failure reading `refs/main` was
  mapped to "no `--latest` pointer" and pushed into the #72 single-fallback;
  a `has`-first check now surfaces the real read error and only a genuinely
  absent pointer proceeds to the fallback.
- **`cce sync status` no longer panics on a short checksum marker (#134).**
  `&state.checksum[..12]` panicked when `.cce/synced.json` (which can come from
  an older/sibling engine or a hand-edit) carried a checksum under 12 bytes or
  with a multi-byte char at the boundary. All such slices now use a
  byte-boundary-safe `short_checksum` helper.
- **Code-sync `repo_id` gets the same path-segment validation as `corpus_id`
  (#141).** `resolve_repo_id` returned the config/override id verbatim, so
  `.`, `..`, and embedded separators flowed into `content_address` /
  `pointer_address` as path segments and escaped the repo namespace. A single
  chokepoint now rejects any id that is not one `Normal` path component
  (`valid_repo_id`, mirroring #121).
- **`cce knowledge push` verifies content, not just the snapshot id, before
  treating a re-publish as unchanged (#113).** The snapshot id hashes only the
  feed bytes, so two producers with a byte-identical feed but different
  `markdown.max_section_tokens` (or redactor version) share one id over
  different `.cck` bytes; the guard short-circuited on `pointer == snapshot`
  and silently overwrote the content-addressed key. It now fetches and
  checksum-compares the remote current and refuses a divergent overwrite
  without `--force`.
- **Superseded local knowledge snapshots are pruned on save (#114).**
  `KnowledgeStore::save` wrote a new `<snapshot>.json` and repointed `current`
  but never removed the superseded artifact (each carrying per-chunk
  embeddings), leaking unbounded disk on routine re-ingestion. `current` now
  advances first, then superseded `<snapshot>.json` artifacts are pruned
  (best-effort, scoped strictly to 16-hex snapshot ids — the `synced.json`
  marker and `current` pointer are never touched).
- **`cce knowledge pull` advances `current` only after the marker is durable
  (#122).** The pull moved the `current` pointer (activating the new store)
  before writing `synced.json`, so a marker-write failure returned `Err` while
  the active store had already been replaced, misfiring the §5 guard both
  ways. The snapshot artifact and marker are now written first and `current`
  advances last, so a marker failure leaves the prior store active and
  consistent.
- **A corrupt knowledge marker no longer disarms the pull overwrite guard
  (#123).** `KnowledgeSyncState::load` mapped any read/parse error to `None`,
  indistinguishable from "never pulled", so a truncated/empty/corrupt
  `synced.json` waved a different-corpus overwrite through without `--force`.
  A new strict load surfaces a corrupt marker as an error (bypassable only with
  `--force`), and `save` propagates a serialization failure instead of writing
  an empty marker.
- **Both sync marker writes route through `atomic::atomic_write` (#150).**
  `SyncState::save` (`.cce/synced.json`) and the knowledge `synced.json` marker
  still used a bare truncate-then-write; a crash mid-write could leave either
  truncated. They now use the #101 atomic temp-file + rename helper, bytes
  byte-identical.
- **Memory append is a single write with a newline guard, so a torn or
  interleaved append can no longer silently lose entries (#102).** `append`
  in `src/memory.rs` wrote the JSON line and its trailing `\n` as two separate
  `write_all` syscalls on an `O_APPEND` handle and never checked that the
  existing `memory.jsonl` ended with a newline. So a partial append (kill or
  ENOSPC between the two writes) or an `O_APPEND` interleave between two
  concurrent `cce mcp` processes concatenated the next entry onto the previous
  line; `load_entries` then silently skipped the merged malformed line — losing
  BOTH decisions while `record_decision` still reported success. The append is
  now built as ONE buffer ending in `\n` and issued in a SINGLE `write_all`,
  which closes the two-write torn case and, for a normal-size record (the
  common case — a `write_all` that does not split into multiple `write`
  syscalls), the concurrent interleave-of-the-pair case, and a
  leading-newline guard prepends a `\n` when the store does not already end in
  one, so a previously-torn file self-heals its boundary instead of
  concatenating onto the broken line. The normal-case bytes are unchanged (a
  well-formed file round-trips byte-identically). `load_entries` still skips a
  legacy malformed line rather than crashing, but now emits a `warning:` naming
  the file and the count of skipped records, so the loss is no longer silent.
- **`index.json` is now written atomically (temp-file + fsync + rename), so a
  crash mid-save can't destroy the store (#101).** `Index::save` used a bare
  `std::fs::write` (open-with-truncate), so a `SIGKILL`/OOM/disk-full mid-write
  destroyed the previous good `.cce/index.json` and left a truncated/0-byte file,
  and a concurrent reader (a long-lived MCP server, `sync push`, `stats`) could
  observe an empty/partial file and spuriously fail with `InvalidData`. Reachable
  from `cce index`, `cce sync pull` (its `install_artifact` calls the same
  `save`), and mcp init. The write now stages the bytes to a uniquely-named temp
  file IN THE SAME DIRECTORY (rename is only atomic within a filesystem), fsyncs
  it, then `rename`s it over the destination — a reader sees either the old
  complete file or the new one, never a partial one, and a failed write leaves the
  previous store byte-for-byte intact with no stray temp file. The temp name
  carries the pid plus a process-local counter so two concurrent saves never
  collide, with no nondeterministic randomness. The mechanism is a shared
  `atomic::atomic_write` helper (mirroring the existing self-update binary swap)
  now used by the code store, the knowledge store's snapshot artifact and
  `current` pointer (`src/knowledge/store.rs`), and the store fingerprint
  (`src/fingerprint.rs`). The on-disk bytes are byte-identical to before — only
  the write mechanism changed — so conformance and pinned-store checksums are
  untouched. On a re-save the atomic write also carries over the destination
  file's existing unix mode before the rename, so a user-tightened store (e.g.
  `chmod 600 .cce/index.json`) keeps its permissions instead of reverting to the
  umask default — matching main's write-through behaviour; a fresh store still
  takes the umask default.
- **`cce sync push --commit <sha>` can no longer poison the content-addressed
  cache (#116).** `resolve_push_sha` accepted any `--commit` value without checking
  it existed or equalled HEAD, while `ensure_hash_index` always rebuilds from the
  working tree. So `cce sync push --commit <old-sha>` at a clean HEAD published
  `build(HEAD)` under the old sha's content-address key **and** moved `refs/<branch>`
  back to it — violating the `artifact == build(sha)` invariant, so consumers pulling
  that sha (or `--latest`, now rewound) failed `cce sync verify`. `--commit` is a
  sanity assertion, not a backfill selector: push can only publish HEAD (it builds
  from the working tree), so a `--commit` that is not a valid commit, or does not
  resolve to the current HEAD, is now rejected before any build/put/ref move. This
  fully closes the **content** invariant (`artifact == build(sha)`). Note a
  remaining, separately-tracked gap (#151): a **detached-HEAD** push (e.g. after
  `git checkout <old-sha>`) has no branch, so the ref pointer falls back to
  `refs/main` and still advances it to the checked-out commit — publish from a
  branch. Default (no `--commit`) behavior is unchanged. New `git::resolve_commit`
  helper validates/normalizes the commit-ish (rejecting leading-dash values so no
  git flag can be smuggled in); SPEC-SYNC §5 documents the contract.
- **`cce init` can no longer destroy user-owned files on a read/parse failure
  (#99).** One root cause, five sub-bugs: a failed read or parse of an
  existing `.mcp.json`, `CLAUDE.md`, or `.gitignore` was silently treated as
  "file empty/absent" and the file rebuilt from scratch under a success
  message. Concretely: a `.mcp.json` with one trailing comma (or a non-object
  root/`mcpServers`) was rebuilt from `{}`, wiping every other MCP server the
  user had configured; a `CLAUDE.md` with an orphaned/misordered/duplicated
  CCE marker had user sections spliced away or was grown unboundedly on every
  run; a `.gitignore` containing one non-UTF-8 byte was replaced with just
  the 3-line CCE block, making previously-ignored secrets committable; and a
  `CLAUDE.md` that could not be read (non-UTF-8 content, a permission error)
  was overwritten wholesale with just the CCE block. `cce init` now fails
  safe at every seam: a read/parse failure of an existing file aborts that
  file's update with an actionable error naming the problem and leaves the
  file byte-untouched; CLAUDE.md markers are recognised only when ALONE ON
  THEIR OWN LINE and touched only as exactly one BEGIN followed by exactly one
  END (anything else is refused with the marker counts and a repair hint) — so
  the marker strings quoted in a user's prose are no longer mistaken for the
  block delimiters and the content between two prose mentions is never spliced
  out; `.gitignore` is handled as raw bytes, so non-UTF-8 content is preserved
  verbatim with the CCE block appended after it; and only `NotFound` may create
  a fresh file. A fail-safe refusal of a later file notes that earlier files may
  already have been updated and that re-running (idempotent) is safe. The
  successful-path output is byte-identical to before. Scope: this guarantee
  covers read/parse failures; a crash MID-WRITE can still truncate a file (the
  writers use non-atomic `std::fs::write` truncate-then-write) — atomic
  temp-file + rename is a separate change, tracked with the store-atomicity work.
- **Generic-assignment redaction no longer leaks a secret containing a quote
  or apostrophe (#104).** The row-10 value class `[^\s"']+` stopped at the
  first `'`/`"`, so `password = don't-…` matched only `don` — under the
  8-char guard, leaving the whole secret unredacted in the store — and
  `password = "abcdefghij'tail…"` redacted only the prefix, persisting the
  tail. The value is now matched by quoting style: a double-quoted value runs
  to its matching `"` (inner `'` and spaces allowed), a single-quoted value
  to its matching `'` (inner `"` and spaces allowed), an unquoted value to
  whitespace/line end — never across a line, never past the first closing
  quote into a neighbouring assignment, and the closing quote still survives
  outside the redaction. Placeholder-guard and short-value semantics are
  unchanged; output stays deterministic and idempotent.
- **A corpus_id of `..` or `.` can no longer escape the corpus namespace and
  destroy other corpora's snapshots (#121).** `valid_corpus_id` accepted any
  non-empty sanitize-stable id, and `.`/`..` are sanitize-stable in the
  `[A-Za-z0-9._-]` charset — yet the id is used verbatim as a path segment on
  the cache, so `knowledge/v1/..` normalizes to `knowledge/` and a
  keep-last-N retention prune under `--corpus ..` enumerated and deleted
  EVERY corpus's `.cck` snapshots (cross-corpus data loss, dangling `current`
  pointers). The validation rule now additionally requires the id to resolve
  to exactly one `Normal` path component (`Path::new(id).components()`), so
  `.` and `..` are rejected at the single `resolve_corpus_id` chokepoint —
  before any filesystem or remote operation — while legitimate dotted ids
  (`my.corpus.v1`, `.hidden`, `v1.`) remain valid. Push, pull, and retention
  all inherit the guard. As release-present defense in depth, the retention
  delete site now confines every key it is about to prune to
  `knowledge/<ver>/<corpus_id>/` (segment-exact) and errors on any key outside
  it — this holds in shipped binaries and checks the key set itself, so it
  refuses the cross-corpus delete even if a traversal id ever reached it.
- **Blended `context_search` no longer silently swallows a code-index load
  failure (#132).** The blend — the DEFAULT path once a knowledge store
  exists — mapped every `load_index()` error to zero code rows, so a corrupt
  or unreadable `.cce/index.json` produced a confident knowledge-only answer
  with `isError:false` and no hint that code results were missing, while
  `source:"code"` on the same server correctly surfaced guidance. The fix
  splits absent from failed: a store that was NEVER built (a knowledge-only
  project) still blends silently — knowledge-only is then the correct,
  complete answer — but a store that EXISTS and fails to load now prepends
  the pinned `CODE_INDEX_LOAD_ERROR_NOTICE` through the same visible-
  degradation notice channel as issue #30's Ollama-down notice (knowledge
  hits still served, `isError` still reserved for malformed calls). The
  workspace variant gets the same split, through its own
  `WORKSPACE_CODE_INDEX_LOAD_ERROR_NOTICE`: because the federated path can
  only tell that a member store is missing or unloadable (not corrupt vs
  never-indexed), its wording reports code results as INCOMPLETE and covers
  corrupt, unreadable, OR not-yet-indexed members — so a normal
  partially-indexed workspace gets an accurate message, never a false
  corruption claim. A workspace whose members were all never indexed stays
  silent. Healthy-path output is byte-unchanged.
- **A secret in a knowledge record's title (or any other facet) can no longer
  reach the store, the provenance line, or a pushed corpus (#111).** `ingest()`
  redacted the rendered document before chunking but then attached the RAW
  `rec.title` — and raw `url`, `labels`, `group`, `state`, `state_reason`,
  `updated_at`, `source`, and `links` — as per-chunk facets, so a record titled
  e.g. `Rotate api_key: … in prod` persisted the secret verbatim in
  `.cce/knowledge/<snapshot>.json`, served it in every `[knowledge] <title> — …`
  provenance header (which also renders `state` and `updated_at`), and exported
  it in the `.cck` on `knowledge push`, while the body was redacted — violating
  the module's "the store never sees a secret" invariant. The `cce.knowledge/v1`
  schema validates none of these facets (`state`/`updated_at`/`source` are plain
  `Option<String>`/`String`, not an enum or ISO-checked format), so EVERY facet
  except the record id now passes through the SAME v2.1 redactor at the ingest
  seam, once per record, before attachment; the breadcrumb `name`/`kind` were
  already safe (derived from the redacted document). The record `id` is an
  **accepted, documented residual**: it is the addressing key (chunk ids and the
  synthetic document path derive from it), so it stays verbatim, the contract
  now requires ids to be secret-free (`docs/knowledge.md`), and the
  redacted-display mitigation for `expand_chunk`/`related_context` is tracked as
  #144. Redaction is the identity on clean text, so secret-free stores are
  byte-unchanged (the pinned ingest checksum and `conformance.json` goldens
  hold); stores indexed from a secret-bearing feed before this fix should be
  re-indexed to scrub the persisted facets.
- **A sync push that loses a ref race can no longer report success while
  publishing nothing (#92).** The push retry rebased the working clone onto
  the advanced remote with the result discarded, under the assumption that
  racing pushes never conflict in content. That holds for content-addressed
  artifact keys but not for the fixed-path keys rewritten by every push — the
  code cache's `refs/<ref>` pointer, the knowledge `current` pointer,
  `corpus.json`, and the workspace metadata — so two racing pushes to the same
  corpus or repo produced a genuine rebase conflict. The conflict was
  swallowed, the retry push of `HEAD` reported "everything up-to-date", and
  the command exited 0 with a full success report while the remote never
  received the commit; worse, the clone under `~/.cce/sync/<remote>/` was left
  with the rebase in progress, so **every subsequent push also silently
  no-opped** until the directory was deleted by hand. The retry now REBUILDS
  the change instead of rebasing: on a rejected push it fetches, hard-resets
  the clone onto the new `origin/<branch>`, re-writes the entries, re-commits,
  and pushes again (bounded attempts) — whole-file last-writer-wins, exactly
  the read-then-publish semantic SPEC-SYNC-KNOWLEDGE §5 defines for the #90
  guard — and exhausted retries return a real error, never `Ok` without a
  successful publish. Commit failures in `put_many` propagate instead of
  falling through (the "nothing to commit" idempotent re-push tolerance
  stays, now detected via `diff --cached`, not by parsing git output). Every
  successful `put_many` is verified by reading the just-written keys back
  from `origin/<branch>` (byte-compared for pointer/metadata keys; existence
  for possibly-LFS `.cce`/`.cck` artifacts), converting any residual silent
  failure into a loud one. The verification is supersede-aware: a competitor
  who legitimately advances the branch past our commit between our accepted
  push and the check is NOT a lost publish — when the tip no longer carries
  an entry, the publish still verifies iff the pushed commit itself carries
  the content AND is an ancestor of `origin/<branch>`; only a push whose
  commit never landed errors, and the error names the next step (fetch and
  inspect the branch; local stores are unaffected; re-run the push). Clones
  already poisoned by the old path self-heal: opening a sync clone with a
  rebase in progress aborts it and restores a clean tree before any
  operation runs (when `--abort` cannot run and the heal falls back to
  `rebase --quit`, the detached HEAD is re-attached to the cache's real
  branch — resolved from `origin/HEAD` — never blindly to the default
  branch name, which would fork a cache living on another branch).
  Deterministic race tests (a pre-receive hook lands a conflicting commit
  mid-flight, then rejects the first push; post-receive hooks simulate the
  supersede and a push whose ref never keeps our commit) cover the
  conflicted and non-conflicted races, retry exhaustion, the
  supersede/genuine-loss split, commit-failure propagation, and both heal
  paths, over both the code and knowledge keyspaces, process-level CLI
  included. `SPEC-SYNC.md` §3/§4, `SPEC-SYNC-KNOWLEDGE.md` §3/§5,
  `docs/sync.md` (a troubleshooting row for the `push verification failed`
  error family included), `docs/knowledge.md`, and `docs/DECISIONS.md` no
  longer claim pushes never conflict in content, document the re-apply
  retry, and state plainly that a push that loses the ref race republishes
  WITHOUT re-running the §5 shrink guard (the behavioral fix is a separate
  follow-up).
- **`cce knowledge push` can no longer silently shrink a published corpus (#90).**
  Push replaces the corpus's `current` snapshot wholesale, so a store rebuilt
  locally from only a subset of a corpus's feed sources (feedA of feedA+feedB)
  would silently drop every record only the other sources carried — whoever
  pushed last won. Push now diffs the outgoing record-id set against the
  remote's current snapshot (fetched and checksum-verified with the exact pull
  machinery) and **refuses a push whose `removed` set is non-empty**, printing
  a deterministic diff report — record counts plus lexicographically sorted
  `added` / `removed` / `changed` id lists (`changed` = a record whose rendered
  content — title + body — differs byte-for-byte, via a full-content per-record
  digest; facet-only edits do not register; lists elide past 20 ids) — and
  naming `--force`, the only override (it skips the diff entirely). The new
  `--dry-run` prints the same report and exits 0 without touching the remote —
  no artifact, no pointer move, no retention — and reports "first publish"
  when no remote pointer exists. Guard rules: a first publish and an
  idempotent re-publish of the already-current snapshot have nothing to diff
  and proceed byte-identically to before; adds-only and changed-only pushes
  never block and stay exactly as quiet; a remote current that exists but
  cannot be fetched or verified is a refusal, never a silent proceed; the
  guard runs before any remote mutation (the LFS attribute write included).
  Proposals 3 (`--merge`) and 4 (`index --into`) from #90 are deferred — a
  merged store is not derived from any single feed, so it needs a new snapshot
  derivation. `SPEC-SYNC-KNOWLEDGE.md` §5 is amended to define the guard
  normatively (client-side, enforced by this engine's push path; §13 parity
  requires other engines to implement the same rule; a read-then-publish guard,
  not a transaction). Docs: `docs/knowledge.md`, `docs/sync.md`, and a
  subset-builder note in `docs/ci/cce-knowledge-sync.yml`.

## [2.8.0] - 2026-07-08

### Added
- **Usage visibility (v2.8): `cce usage` + the opt-in MCP result footer (#35).**
  `cce usage [--workspace] [--since 24h|7d|<ISO>] [--source mcp|cli|all] [--json]`
  is the one-shot, CI-friendly terminal counterpart to the dashboard's
  agent-vs-human panel: the `mcp` (agent) vs `cli` (human) split — searches,
  tokens saved, savings ratio, quality, latency — the recent queries, and a
  `by_package` mini-table in workspace mode. **Pure projection, zero new
  accounting**: it reuses the exact `aggregate()` / federated aggregation the
  dashboard serves (including the #28 workspace-root-log rule), so its numbers
  are always identical to `cce dashboard`'s for the same log and window (proven
  by tests that run both paths over one fixture). `--json` emits the versioned,
  byte-pinned `cce.usage/v1` projection (stable field names, the same shapes as
  `/api/metrics` where they overlap); the human block is byte-pinned too.
  Deterministic, offline, read-only; `now` is injected below the CLI edge.
  Second surface: a per-project `.cce/config` key `mcp.result_footer:
  off (default) | on | session` appends ONE byte-pinned line to `context_search`
  results — `cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880
  baseline · saved ~8,676 (88%)` (`session` adds a running per-session clause).
  Rendered after all measurement from values already on the recorded `search`
  event: toggling it never changes a recorded metric (same query, footer off vs
  on ⇒ an identical recorded event, test-proven), and with the footer `off` the
  MCP tool-result bytes, `conformance.json`, and every MCP golden are untouched.
  Config-only by design — no runtime tool, so the agent cannot toggle its own
  observability. Additive `/api/metrics` fields feed the new surfaces:
  `by_source.*.mean_latency_ms` and `recent_searches[].source`. Spec committed
  as `SPEC-USAGE-VISIBILITY.md`; docs in `docs/mcp.md`, `docs/dashboard.md`,
  and `docs/how-to.md`.
- **`cce relevance` — the retrieval-relevance evaluation harness (#63).** The
  missing third leg of the measurement story: `cce conformance` proves output
  stability and `cce bench`/`cce eval` measure latency and token savings — this
  measures **ranking quality**. Labeled query→expected-result fixture sets
  (`cce.relevance/v1` NDJSON: `{query, expected: [file or file#kind anchors], k}`
  per line — a documented contract like the knowledge feed) run through the REAL
  retrieval pipeline at a named backend (`bm25` = the issue-#30 keyword-only
  mode, `vector` = pure cosine order, `hybrid` = the full SPEC §6 pipeline
  `cce search` serves) and are scored with standard IR metrics — precision@k,
  recall, MRR, F1 — per query and macro-averaged per backend.
  `--compare A,B` prints per-query deltas so a proposed ranking change shows
  exactly which queries it helps or hurts before it merges. Two starter fixture
  sets ship in `eval/relevance/` (code over the conformance sample corpus;
  knowledge-style queries over a small markdown corpus); the hash-path `--json`
  report (`cce.relevance.report/v1`) is byte-pinned in CI against
  `test/fixture/relevance/code.golden.json`, conformance-style. Measurement
  only: zero ranking-behavior changes. See `docs/relevance.md`.
- **Build fingerprint + `cce doctor` — detect config drift before it degrades
  retrieval (#62).** Every store write (`cce index`, workspace indexing, the
  `cce init` local index, and every `cce sync pull` install) now stamps a small
  `cce.fingerprint/v1` block into `fingerprint.json` **beside** the store: engine
  version, embedder id + dimensions, the chunker identity (language-pack set,
  markdown split budget, nesting limit), the tokenizer rule id, and the redaction
  flag — plus a SHA-256 self-checksum over the canonical serialization and a
  SHA-256 binding to the exact store bytes it describes (a store rebuilt by an
  older binary is detected as stale, never trusted). Additive by construction:
  the fingerprint is a separate file old readers never open — the store bytes,
  the sync artifact, `conformance.json`, and every byte-pinned golden are
  untouched, and all recorded values derive from pinned constants so the
  fingerprint itself is deterministic. `cce doctor [--dir|--store]` is the
  read-only report over it: fingerprint fields vs the running binary's pinned
  equivalents, with every mismatch explained ("chunker changed: chunk_ids may
  not be reproducible; re-index to realign"; embedder/dimension drift = the #30
  meaningless-cosine failure mode); store parse health with the #30
  empty-embedding tripwire; the #55 installed-bytes corruption re-hash for
  pulled stores (reusing the `verify --checksum-only` machinery verbatim); the
  knowledge store's contract version, snapshot id, and data as-of; and a
  workspace mode that checks every member (StoreOnly consumers included) and
  summarizes. Doctor never mutates; it exits non-zero ONLY on definite
  corruption/mismatch — soft findings are distinct `advisory` lines, and a
  pre-fingerprint store is a graceful re-index notice with exit 0. Hermetic
  tests only (no network, no Ollama).

## [2.7.1] - 2026-07-08

### Added
- **`cce update` / `cce upgrade` — checksum-verified self-update from GitHub Releases
  (#75).** The missing client for the tag-driven release pipeline: resolve the latest
  (or a `--version vX.Y.Z`-pinned) release via one `SHA256SUMS` fetch, download the
  platform tarball by shelling out to `curl` (the house pattern — sync shells out to
  git; no HTTP-client dependency), verify it against `SHA256SUMS`, and atomically
  rename the new binary over `current_exe()` (symlinks resolved; the running process
  keeps its inode). Everything stages in a temp dir, so a corrupt download, checksum
  mismatch, or unwritable install location leaves the current install untouched — a
  mismatch is a loud refusal, an unwritable location suggests `sudo`/manual install
  (never privilege escalation), an unsupported platform names the four published
  targets, and a missing curl points at the manual install. `--check` is scriptable
  (one line; exit 0 = up to date, exit **10** (pinned) = update available); `--version`
  is the rollback path (downgrades warn but proceed); after updating, the CHANGELOG
  sections between old and new print newest-first, capped at 5 with a releases-page
  link. Per the settled offline-first posture, `update` is explicit-invocation network
  ONLY and the sole code path that invokes curl (grep-provable: confined to
  `src/update.rs`); no other command gained any network behavior. `SHA256SUMS`
  verification protects integrity, not authenticity beyond GitHub's TLS — stated
  plainly in the docs; detached signatures remain a documented future hardening. The
  release asset naming is now a compatibility contract (noted in RELEASING.md). Tests
  are fully hermetic: a local HTTP fixture server via the test-only
  `CCE_UPDATE_BASE_URL`/`CCE_UPDATE_TARGET` overrides, mutating tests run a staged
  copy of the binary, and the delta rendering is byte-pinned. No retrieval surface:
  `conformance.json`, all goldens, and `SYNC_FORMAT_VERSION` are untouched.

### Fixed
- **Repos pushed from a non-`main` default branch are no longer invisible to consumer
  mode (#72).** `cce sync list`, `pull --latest`, and `pull --all` resolved the latest
  pointer at `refs/main` only, so a repo whose CI pushes from e.g. `master` showed
  `latest = -` and was warned-and-skipped despite a valid artifact + pointer. Now, when
  `refs/main` is absent and **exactly one** other `refs/<name>` pointer exists, it
  resolves the latest sha — annotated on every surface (`<sha> (master)` in the human
  listing, an optional `ref` field on the `cce.synclist/v1` JSON row, `ref : master` /
  `(ref master)` in pull reports); with **several** non-main refs the skip/`-` behaviour
  stays but the warning names the available refs. Explicit control: `cce sync pull
  --latest --ref <name>` (rejected with `--all`, where repos have different default
  branches) and a per-member `sync.ref` config key that `pull --all` refreshes honor and
  preserve across the config rewrite. All of a repo's refs enumerate in ONE listing call,
  never N pointer reads. `refs/main`-resolved outputs stay byte-identical everywhere
  (the existing pinned goldens pass unchanged); `SYNC_FORMAT_VERSION`,
  `conformance.json`, and the knowledge `current` pointer family (a different key space)
  are untouched — asserted, not assumed.

## [2.7.0] - 2026-07-08

### Added
- **Knowledge-corpus sync M5.1+M5.2 — the `.cck` artifact and `cce knowledge push` / `pull`
  (#56, per SPEC-SYNC-KNOWLEDGE).** A built knowledge store now travels through the same
  content-addressed cache as code indexes. `sync::knowledge_artifact` owns the canonical,
  byte-exact `.cck` container (manifest + one line per chunk in store order, sorted-keys
  compact JSON, the `.cce` base64 f64-LE embedding codec, zero provenance fields, checksum
  computed with `checksum:""`) — a pure function of `(feed, corpus_id)`, with a committed
  golden checksum for the shared fixture feed and a refusal of embedding-less Phase-A
  stores. `cce knowledge push [--corpus <id>] [--remote <url>]` exports the current local
  store and lands artifact + `current` pointer + published `corpus.json`
  (`cce.knowledgemeta/v1`, carrying `pushed_at` — deliberately outside the reproducible
  artifact; the deterministic `data_as_of` lives inside it) in one commit, then applies
  per-corpus `knowledge.sync.retention` (`keep-last-<n>` prunes oldest by the cache repo's
  commit order; the `current` snapshot is never pruned; prune failures warn, never fail the
  push). `cce knowledge pull [--corpus <id>] [--latest | --snapshot <id>] [--force]
  [--remote <url>]` verifies the manifest checksum (a mismatch is a hard failure naming the
  key) and installs into `.cce/knowledge/` **byte-identical to a local ingest**, recording
  the knowledge sync marker (`synced.json` with `installed_sha256`, the #55 mechanism —
  the `verify --checksum-only` surface wires up in M5.3). Guards per the spec: corpus_id is
  never derived (explicit `--corpus` or `knowledge.sync.corpus_id`, validated
  sanitize-stable); pulling a different corpus refuses without `--force`; the raw feed
  never travels and a planted secret arrives redacted in the artifact (`knowledge index`
  has no bypass flag — asserted). Config: `knowledge.sync.corpus_id` / `remote` (per-corpus
  §4.3 override; default `sync.remote`) / `retention`. `serde_json` gains the
  `float_roundtrip` feature so a loaded store's embeddings parse back to the exact doubles
  that were written (push exports the loaded store; without it the `.cck` drifted a ULP
  from a local ingest). Additive throughout: `SYNC_FORMAT_VERSION`, `conformance.json`,
  code artifacts, and every existing golden are untouched (asserted, not assumed — a
  knowledge corpus beside code artifacts leaves `sync list --json` byte-identical).
- **Knowledge-corpus sync M5.3+M5.4 — the consumer surface and the ingestion reference
  (#56, completing SPEC-SYNC-KNOWLEDGE).** Corpora are now first-class on every consumer
  surface. `cce sync list` grows the §6 knowledge section: a human block after the repos
  table (corpus / current / snapshots / LFS-aware bytes / data as-of) and an OPTIONAL
  `knowledge` array on the unchanged `cce.synclist/v1` JSON — emitted only when the cache
  carries a corpus, so knowledge-free listings stay byte-identical (nullable fields stay
  present as `null`). `cce sync pull --all [--corpus <id>]` installs the cache's corpus
  into the consumer workspace root `.cce/knowledge/` via the `knowledge pull` machinery
  verbatim (store, `current`, and marker byte-identical to a direct pull): an explicit
  `--corpus` wins, a single-corpus cache auto-installs, several corpora warn-and-skip
  naming the ids (one active corpus per root; member pulls never fail because of
  knowledge), and refresh is marker-idempotent — an unmoved remote `current` reports
  `up-to-date` with no fetch, a moved one refreshes exactly the corpus. `cce sync verify
  --checksum-only` gains the knowledge row: re-hash of the installed snapshot against the
  marker's `installed_sha256`, with member semantics (pass row; a mismatch fails loudly
  naming the corpus — plus the honest sharpening that knowledge has NO rebuild-verify
  escalation path at all; a marker without the hash is an explicit notice at exit 0), and
  a knowledge-only root verifies too. MCP `index_status` gains the §4.4 knowledge block
  (corpus or `(local ingest)`, snapshot, records/chunks, data as-of, best-effort
  offline-safe `remote current` / `behind remote` mirroring the code freshness rules);
  reports without a knowledge store are byte-identical. M5.4 ships the reference
  scheduled-adapter workflow `docs/ci/cce-knowledge-sync.yml` (fetch → emit
  `cce.knowledge/v1` → `cce knowledge index` (redacts) → `cce knowledge push`; a builder
  job, never a serving process; the feed is ephemeral and never committed; disjoint
  source-READ vs cache-WRITE secrets) and the documentation pass: docs/knowledge.md M5
  un-deferred with the full sync/consumer/freshness/trust story, docs/sync.md consumer
  mode covers corpora in `list`/`pull --all`/`verify`, docs/mcp.md documents the
  `index_status` knowledge block, README and llms.txt updated. One pinned surface moved
  by design: the #69 additivity test now asserts the M5.3 shape (every pre-existing
  listing field byte-stable beside a corpus; the corpus visible only as the new optional
  key).

### Documentation
- **SPEC-SYNC-KNOWLEDGE.md — the normative build spec for M5, knowledge-corpus sync (#56).**
  The SPEC-SYNC pattern reapplied to the v2.6 knowledge system: a canonical, provenance-free
  `.cck` corpus artifact (the built, redacted store — never the raw feed) under an additive
  `knowledge/<contract_version>/<corpus_id>/` key space in the same git+LFS cache, with a
  `current` pointer and a published `corpus.json` per corpus. Settles the six M5 decisions
  normatively (corpus identity, the honest trust-the-pusher posture with a code-vs-knowledge
  comparison table, access boundary, freshness signals, per-corpus retention, index-time
  redaction), specs `cce knowledge push/pull`, the `cce sync list` knowledge section (still
  `cce.synclist/v1` — additive optional key, knowledge-free listings byte-identical),
  `pull --all` corpus install at the workspace root, and `verify --checksum-only` coverage,
  plus the CI-cron builder reference workflow and milestones M5.1–M5.4. Spec-first: no
  implementation in this change; `SYNC_FORMAT_VERSION`, code artifacts, and all goldens
  untouched. `docs/knowledge.md`'s M5 deferral note now points at the spec.

## [2.6.9] - 2026-07-08

### Added
- **`cce sync list [--remote <url>] [--json]` — enumerate what a sync cache holds (#53).**
  The discovery half of consumer mode: one row per `repo_id` with its **latest sha** (the
  `refs/<branch>` pointer `pull --latest` reads — `-`/`null` when a repo has no pointer yet),
  **artifact count**, and **total artifact bytes** (LFS-aware: an LFS pointer reports its
  recorded artifact size, not the ~130-byte pointer file). Wires up the previously
  CLI-unreachable `SyncRemote::list` (#37/#50), keeping its pinned graceful-skip of
  non-artifact cache entries. Read-only — it never mutates the cache or the local `.cce/` —
  and repo-less: a bare directory plus `--remote <url>` is sufficient. Rows sort by `repo_id`;
  an empty cache is a friendly message (exit 0); an unreachable remote is a clear non-zero
  error. `--json` emits the stable, byte-pinned `cce.synclist/v1` shape.
  `SYNC_FORMAT_VERSION`, `conformance.json`, and every existing golden are untouched.
- **`cce sync pull --all --into <dir> [--remote <url>]` — the one-command repo-less consumer
  workspace (#54).** From a bare directory: enumerates the cache (the #53 `sync list`
  machinery), pulls every `repo_id`'s latest artifact into `<dir>/<member>/.cce/`, and
  synthesizes `<dir>/.cce/workspace.yml` plus the root and per-member `.cce/config`, so
  `cce search --workspace <dir>` and `cce mcp --workspace --dir <dir>` work immediately —
  zero source checkouts, each member federated at its own independent sha. Members are
  short-named from the repo_id's last `__` segment (`-2`/`-3` on collision); the full
  repo_id lives in the member's config so per-member pulls keep working. Repos without a
  latest pointer are warned and skipped, never fatal. Re-runs are idempotent refreshes:
  only members whose latest pointer moved are re-pulled, new repo_ids join the workspace,
  and vanished ones are warned about but never deleted. Synthesized manifests use the new
  neutral `type: store-only` member type (a member with no source to classify); detection
  never emits it and hand-written manifests stay byte-identical. Consumer mode (including
  the repo-less single-member `--latest`/`--commit` pull) is now documented in
  `docs/sync.md`. `SYNC_FORMAT_VERSION`, `conformance.json`, and every existing golden are
  untouched.
- **The self-describing cache — published workspace metadata + `cce sync verify
  --checksum-only` (#55).** Consumer mode 3/3. `cce sync push --workspace` now also publishes
  the canonical `workspace.yml` and the derived cross-member `workspace-graph.json` at
  well-known keys under the workspace's **base** repo_id
  (`hash/<ver>/<base>/workspace.yml` / `…/workspace-graph.json`) — additive by construction
  (neither an artifact nor a `refs/` pointer; SPEC-SYNC §3 now states the additive-keys rule
  normatively). The pull paths consume it: `pull --workspace` installs the published graph,
  merges the real member types/packages into the local manifest (matched by name; the local
  path wins), and can bootstrap a repo-less consumer with no manifest at all; `pull --all`
  discovers every published manifest via the extended `sync list` machinery, enriches exactly
  the members each manifest covers, and installs the merged graphs rewritten to the consumer
  member names (member-name collisions across workspaces: first in repo_id order keeps the
  bare name, later ones stay at their `-2`/`-3` names, warned). Result: a repo-less federated
  search regains **cross-member graph expansion**, byte-identical to the source-side
  workspace. `cce sync verify --checksum-only` gives consumers a real integrity check with
  zero source checkout: `pull` records the SHA-256 of the exact `index.json` bytes it
  installs (an additive `installed_sha256` field in `.cce/synced.json`), and verify re-hashes
  the on-disk file against it — **version-independent** ("has this file changed since
  pull"), so artifacts pushed by older cce versions verify exactly like current ones
  (live-verified against a mixed-version cache; an export-based comparison would false-fail
  them). Failures are loud and name the member; a marker written by an older cce (no
  recorded hash) is an explicit exit-0 re-pull notice, never a false failure. Documented
  caveat: detects corruption, not a malicious build (true `artifact == build(sha)`
  verification stays with source-holders/CI). Also from live review: a `pull --all` refresh
  now **re-adopts** a member directory whose `.cce/config` went missing (matched by name,
  noted in the report) instead of duplicating it as `<name>-2`. Caches without
  published metadata, plain single-member pulls, `SYNC_FORMAT_VERSION`, `conformance.json`,
  and every existing golden are untouched.

### Documentation
- **Consumer-mode documentation sweep (pre-v2.6.9).** The whole doc surface now tells the
  #53/#54/#55 story coherently: a "consume a team cache" recipe in `docs/how-to.md` (the
  flagship repo-less flow), consumer-mode/`store-only` coverage in `docs/workspace.md`, a
  repo-less agent-context note in `docs/mcp.md`, the `list`/`pull --all` CLI surface in
  SPEC-SYNC §5, a "consumer mode over a server" decision entry in `docs/DECISIONS.md`,
  refreshed module-map/`llms.txt`/README index rows, the `Cargo.toml` description, and
  current test counts (605) in README/AGENTS/CONTRIBUTING/getting-started/llms.txt.

## [2.6.8] - 2026-07-08

### Changed
- **Index-time embedding now batches chunks through `try_embed_batch` (#38).** The store build
  path used to embed one chunk per call — one HTTP request per chunk on the Ollama backend, so a
  repo with tens of thousands of chunks cost tens of thousands of sequential round-trips. Chunks
  are now embedded in bounded batches of `EMBED_BATCH_SIZE` (64, pinned in `src/config.rs`), so
  indexing issues `ceil(chunks / 64)` requests instead of one per chunk (measured on a 300-file /
  600-chunk synthetic repo against a 10 ms-latency stub: 601 → 11 requests, ~10.2 s → ~0.2 s).
  The fail-loud policy (#30) holds at batch granularity: a failed or count-mismatched batch aborts
  the index naming the batch's file span, and nothing is persisted — never empty or misaligned
  vectors. The hash embedder is untouched (its default batch impl maps the same pure per-text
  embed over each batch), so all goldens and `conformance.json` are byte-identical.

### Fixed
- **The chunkers survive pathologically nested input — iterative tree walks, no SIGSEGV (#49).**
  A property-suite CI run died with SIGSEGV before proptest could persist the failing seed. Two
  crash classes were reproduced deterministically and fixed. (1) The code and markdown chunkers'
  per-node **recursive** AST walks (`collect_chunks`, `visit_pre`, the heading/inline walks)
  overflowed the thread stack on deeply nested input — measured crash at depth ~219 on a 256 KiB
  stack and ~875–1748 (grammar-dependent) at the 2 MiB Rust test-thread default, while tree-sitter
  itself parses the same input fine at depth 500k. All walks are now **iterative `TreeCursor`
  loops** with identical pre-order emission, so chunk output is byte-identical for every input.
  (2) tree-sitter-md's external scanner serializes its open-block stack into tree-sitter's fixed
  1024-byte buffer **without a bounds check**: ~255 simultaneously open blocks (e.g. one line of
  255 `>` characters) is an assert-abort in debug and a buffer overrun (SIGSEGV) in release,
  independent of stack size and uncatchable from Rust. `chunk_markdown` now computes a conservative
  per-line upper bound on open-block depth **before parsing** and degrades estimated-deeper-than-192
  input to the existing deterministic whole-doc fallback chunk — fail-safe, never crash. A
  deterministic regression suite (`tests/deep_nesting.rs`) chunks nesting just under and far past
  the old thresholds on a 256 KiB thread, and each chunker property case now runs on a 16 MiB
  thread so any future crash becomes a persistable proptest counterexample instead of a process
  kill. All goldens and `conformance.json` are byte-identical.
- **`cce search --workspace --package ""` now errors loudly instead of silently returning no
  results (#45).** An empty-but-present `--package` value (`""`, `","`, whitespace — e.g. an unset
  shell variable in `--package "$PKG"`) used to parse to an empty scope, federate over zero members,
  and print nothing, bypassing the #26 unknown-token error. `parse_scope` now lives in
  `cce::federation` and rejects a scope with no usable token with an actionable message
  (`--package requires at least one member or package name (e.g. --package app,billing)`); the MCP
  `context_search` `package` argument goes through the same parser, so `{"package": ""}` gets the
  same friendly guidance instead of silent no-results. Valid scopes are byte-identical.

### Added
- **Binary-level error-path tests: corrupt store, malformed manifest, garbage remote listing,
  dashboard CLI (#37).** Four real-world corruption scenarios are now pinned by driving the real
  `cce` binary: a truncated-JSON or binary-junk store makes `search`/`stats` exit non-zero with the
  friendly `could not load store …` message (never a panic); a syntactically broken
  `.cce/workspace.yml` surfaces `invalid workspace.yml: …` from `search --workspace` and
  `stats --workspace`; non-artifact entries in a sync remote's ref listing are skipped gracefully
  by `SyncRemote::list` (unit-level — no CLI command reaches the listing parser today); and
  `cce dashboard --port 0 --no-open` (plus the `--workspace` variant) binds an ephemeral loopback
  port, prints the URL, and answers `/api/health` with 200 + valid JSON. Test-only — no behavior
  change.
- **Tests for `src/main.rs` and a byte-pinned `search --json` golden (#32).** The CLI entry point
  (~1,300 lines) previously had zero tests. It now has a unit suite pinning current behavior of the
  pure helpers — `parse_scope` comma/whitespace/empty-segment edges, `resolve_read_store` /
  `resolve_metrics_path` / `metrics_beside_store` precedence (explicit `--metrics` wins, else beside
  the resolved `--store`, else `<root>/.cce/metrics.jsonl`) — plus byte-pinned goldens for the
  script-facing `results_json` / `fed_results_json` shapes (field order, 6-decimal string scores
  incl. round-half-away-from-zero, `query_id: null` when metrics are off, trailing newline), and a
  binary-level `tests/cli.rs` test pinning the parsed `--json` field set. Test-only — no behavior
  change; all existing goldens and `conformance.json` unchanged.
- **Automated, tag-driven releases.** Pushing a `vX.Y.Z` tag now re-runs every CI gate on the tagged
  commit, verifies the tag matches `Cargo.toml` and that this file has a matching section, builds
  release binaries for macOS (arm64/x86_64) and Linux (x86_64/arm64), and publishes a GitHub Release
  with this file's section as the notes plus a `SHA256SUMS`. Process documented in `RELEASING.md`;
  README gains a prebuilt-binary install path. (Repo infrastructure — the `cce` binary is unchanged.)
- **Property-based tests for the chunkers and the pinned token rule (#33).** A new `proptest` suite
  (`tests/property_chunkers.rs`) generates adversarial-but-legal source for all six language packs
  (unicode identifiers, CRLF line endings, trailing whitespace, missing final newline, empty and
  comment-only files, deeply nested definitions, raw printable-unicode garbage) and markdown
  (ATX/setext headings, preambles, fenced code blocks containing `#` lines, varied split budgets),
  and asserts the chunkers' documented invariants on every input: in-bounds ordered line ranges,
  content as an exact byte slice of the input, pre-order nested-or-disjoint emission, determinism,
  `chunk_id` recomputable from the persisted fields, the pinned `max(1, floor(bytes/4))` token rule,
  and markdown section ordering/coverage. Test-only: goldens, `conformance.json`, and the `cce`
  binary are unchanged.

### Documentation
- **v2.6 documentation sweep (#34).** Re-ran the gapless-docs discipline (#11, last executed at
  v2.5.5) over the v2.6.0–v2.6.7 surface. The knowledge track (`cce knowledge index`, the
  `cce.knowledge/v1` contract, the `context_search` `source: code|knowledge|both` blend, provenance +
  staleness weighting, the `knowledge.*` config keys) now appears in `docs/knowledge.md` (M4 section),
  `docs/mcp.md` (the `source` schema property), the README, and the getting-started/how-to/
  how-it-works/architecture cross-references; the v2.6.3 gitignore-aware walker (committed
  `.gitignore` only — builder independence) is documented in the README, guides, architecture, and
  sync's rationale; `docs/sync.md` states that push always rebuilds from source (v2.6.2);
  `docs/workspace.md` + `docs/architecture.md` carry the v2.6.4 `--package` semantics (name or
  `package:` field, loud error with the available list) and the v2.6.7 MCP caching instead of the
  stale "reloaded per query" claim. Stale pins fixed: `cce 2.5.5` / `--tag v2.3.0/v2.4.0` examples,
  the retired `built_at` CI comment, the `--top-k` default (10, not 5), and the 416/500 test counts
  (now 540); the Cargo.toml `description` extends through v2.6 (metadata only). Docs-only — no engine
  change; `conformance.json` and all goldens are byte-identical.

## [2.6.7] - 2026-07-06

### Changed
- **The MCP server caches the single-repo index and the knowledge store across calls (#31).** The
  long-lived `cce mcp` server did O(corpus) work on EVERY tool call: the single-repo path re-read +
  JSON-parsed the whole store and rebuilt the entire BM25 index and import graph per request
  (`Index::load`), and the knowledge path additionally re-ran the embedder over legacy chunks and
  rebuilt a BM25 index per query. Extending the #26 workspace pattern, `McpServer` now caches the
  loaded `Index` and the loaded+embedded knowledge store, keyed by a cheap freshness fingerprint —
  store-file `mtime`+length from one `fs::metadata` call per tool call (for knowledge: the `current`
  pointer plus the snapshot artifact it names). A re-index, a knowledge re-ingest, or a
  `cce sync pull` (startup auto-pull or mid-session) invalidates on the next call; a **deleted store
  drops the cache and serves the friendly missing-index message** — never a stale answer. The #26
  workspace union cache (previously cached forever) now carries the combined fingerprint of its
  in-scope member store files, so a member re-index mid-session is picked up without restarting
  `cce mcp`. Warm calls sit under the #41 per-query embedder choice (BM25-only degradation
  unchanged). Perf only — **ranked results and MCP result text are byte-identical warm vs cold**
  (regression-tested), CLI one-shot paths are untouched, and `conformance.json` + all goldens are
  unchanged. On a synthetic 3.2k-chunk store driven over stdio, a warm MCP `context_search` drops
  from ~23ms to ~2ms per call (~10×); a warm knowledge query (300 records) from ~6ms to ~1ms (~5×) —
  and the win scales with corpus size, since the removed work was O(corpus) per call.

## [2.6.6] - 2026-07-06

### Fixed
- **The Ollama embedder fails loud instead of degrading silently (#30).** Three compounding silent
  failures in the opt-in `--embedder ollama` path are gone. (1) *Index time:* an embedding failure —
  Ollama unreachable at start, or dying mid-index — now **aborts `cce index` with a clear error and
  writes no store** (previously `embed_batch` swallowed errors into empty vectors, which were persisted
  and scored cosine 0 forever, invisible to vector recall). There is deliberately **no fallback to the
  hash embedder at index time** either: that would poison the store's declared embedder space just as
  badly. (2) *Query time, CLI:* `cce search` (and `--workspace`) on an ollama-built store with Ollama
  down now **errors with guidance** (start Ollama, or re-index with the default hash embedder) instead
  of silently embedding the query with the hash backend — cosine across two unrelated vector spaces is
  meaningless. (3) *Query time, MCP:* `context_search` follows the friendly-error pattern — it does not
  crash the session, and now **degrades to keyword-only (BM25) results under a pinned `NOTICE:` line**,
  so the agent keeps getting results while the degradation stays visible. The `Embedder` trait lost its
  silent-empty-vector batch path (`embed_batch` → fallible `try_embed`/`try_embed_batch`), the endpoint
  and model are overridable via `CCE_OLLAMA_URL`/`CCE_OLLAMA_MODEL` (which also keeps the new
  failure-policy tests hermetic — a loopback HTTP stub, never a real server), and the docs that
  described the silent fallback as a feature are rewritten. The default hash-embedder path, the
  knowledge store (hash-only), `conformance.json`, and all goldens are **byte-identical**.

## [2.6.5] - 2026-07-06

### Fixed
- **The workspace dashboard now shows `cce mcp --workspace` (agent) searches (#28).** In workspace mode
  the MCP server writes `search` events to the workspace-root `.cce/metrics.jsonl`, but
  `cce dashboard --workspace` aggregated only the member logs — so agent/MCP searches never appeared in
  `totals`, `recent_searches`, or `by_source`, contradicting `docs/mcp.md`. The workspace dashboard now
  folds the root log into its roll-up (guarded against double-counting a member that points at the root).
  These federated searches span members and stay **out of `by_package`** by design — that panel remains
  per-member. Docs aligned; per-package attribution of agent searches is left as a follow-up option.

### Changed
- **Faster, correcter workspace federation (#26).** Member stores load **without** building per-member
  BM25 (federation scores only the union's BM25), removing redundant work — full-workspace search is
  ~1.3–2× faster (a real 38.6k-chunk workspace: 3.2s→2.4s CLI). **`--package` short-circuits** to load
  only the scoped member(s) (2.08s→1.58s) and now resolves by member name **or** the `package:` field,
  **erroring with the available list** on no match (previously matched member name only and returned
  empty silently). The **MCP server caches the assembled union** per scope, so repeated
  `context_search` no longer re-federates (warm call ≈ CLI). Perf/correctness only — **ranked results
  are byte-identical** (regression-tested); keeps exact brute-force cosine (ANN deferred).

## [2.6.3] - 2026-07-06

### Fixed
- **The indexer now honors committed `.gitignore`** (#24). The walker uses ripgrep's `ignore` crate and
  skips files ignored by the repo's committed `.gitignore`, restoring the sync invariant `artifact ==
  build(sha)`. Machine-local ignore sources (`.git/info/exclude`, global `core.excludesfile`) and
  `.gitignore` above the walk root are deliberately NOT honored, so artifacts stay builder-independent;
  `.git/` and `.cce/` are always skipped. Previously a gitignored-but-present file (e.g. Next's
  `next-env.d.ts`) polluted local indexes, false-failing `cce sync verify`.

### Added
- **`cce init` gitignores the cache** — appends `.cce/*` + `!.cce/workspace.yml` to the repo `.gitignore`
  (git repos only; idempotent): the local index/cache is never committed, the shared `.cce/workspace.yml`
  stays committable.

## [2.6.2] - 2026-07-06

### Fixed
- **`cce sync push` now always rebuilds the index from the working tree** before exporting, instead of
  re-exporting an existing `.cce/index.json`. A just-pulled or otherwise stale/foreign index could be
  republished verbatim under the content-address sha key, violating `artifact == build(sha)` and making
  `cce sync verify` fail. `pull`/`verify` unchanged; the Sync artifact format is byte-identical.

## [2.6.1] - 2026-07-06

### Added
- **Knowledge Sources (v2.6 Phase B)** — knowledge chunks are searchable through the same hybrid
  retrieval as code (hash embedder + BM25 + RRF). `context_search` gains an optional
  `source: code|knowledge|both` (**still 9 tools**; code-only behaviour byte-identical when no
  knowledge store). Knowledge hits carry provenance (`[knowledge] <title> — <state> · <updated_at> ·
  <url>`) with deterministic staleness weighting (recency; drop `not_planned`/`wontfix`; merged-PR
  boost) and a precision-filtered recall floor; `expand_chunk`/`related_context` work on knowledge
  chunks. Fully additive: `conformance.json` + the Sync artifact are byte-identical.

## [2.6.0] - 2026-07-05

### Added
- **Knowledge Sources (v2.6 Phase A)** — a markdown-heading chunker (tree-sitter-markdown; each `##`
  section becomes a content-addressed chunk), the neutral **`cce.knowledge/v1`** ingest contract, and
  **`cce knowledge index <file.jsonl>`** which renders + heading-chunks records into a *separate,
  snapshot-keyed knowledge store* (redacted before write; issue/doc metadata as facets). **Fully
  additive** — the code index, `conformance.json`, and the Sync artifact are byte-identical. Config:
  `markdown.max_section_tokens` (400), `knowledge.enabled`.

## [2.5.5] - 2026-07-05

### Documentation
- **v2.5 documentation sweep** — brought every doc current to the complete Savings
  Layers track (v2.5.0–v2.5.4), verified from a cold start. No engine behaviour
  change; `conformance.json` and the Sync artifact are byte-identical.
  - `README.md`: a "Token savings — honestly" section covering the seven Savings
    Layers, compact-by-default retrieval with expand-on-demand, `cce savings`, and
    the honest "vs full-file baseline — not your real end-to-end agent cost"
    framing; the MCP section now lists all **nine** tools.
  - `docs/savings.md` (new): the seven layers, the ledger, `cce savings`, the
    `cce.tokens/v1` estimator caveat, and the `cce eval` A/B harness.
  - `docs/mcp.md`: documents all **nine** MCP tools with input schemas and the
    find → expand → widen relationships, memory, summarization, and output
    compression.
  - `docs/architecture.md`, `docs/how-it-works.md`, `docs/getting-started.md`,
    `docs/how-to.md`, `docs/dashboard.md`: compact-by-default retrieval,
    `expand_chunk`, memory, and the `savings_by_layer` panel where relevant.
  - `docs/DECISIONS.md`: the key v2.5 decisions (compact-by-default and the
    structural-compact fix, memory anti-pollution, deterministic structured
    digests, grammar self-measurement, `SYNC_FORMAT_VERSION` decoupling, Rust-first
    sequencing).
  - `docs/VERIFIED.md`: a fresh cold-start transcript exercising `cce index` → the
    nine-tool `cce mcp` session (compact `context_search`, `expand_chunk`,
    `record_decision`/`session_recall`, `summarize_context`) → `cce savings`.
  - `llms.txt`, `AGENTS.md`, `CITATION.cff`: the full v2.5 surface.

## [2.5.4] - 2026-07-05

### Added
- **Grammar compression (L3)** — the MCP read-tool result grammars are byte-pinned to a compact,
  filler-free format (`context_search`, `expand_chunk`, `related_context`, `session_recall`,
  `summarize_context`), and the `grammar` savings bucket is self-measured (compact vs a pinned
  verbose baseline, via `cce.tokens/v1`). Completes the **seven-bucket savings ledger**. Additive;
  `conformance.json` and the Sync artifact unchanged. **This completes the v2.5 Savings Layers track.**

## [2.5.3] - 2026-07-05

### Added
- **Turn summarization (L6)** — a `summarize_context(scope?)` MCP tool returning a **deterministic,
  structured** digest of the session so far (files · chunks · queries · decisions touched — deduped,
  sorted, capped with `… (+N more)`) — a structured ledger digest, NOT an LLM summary, so it stays
  byte-deterministic and offline. Backed by an in-memory, wall-clock-free per-session ledger.
  `summarization.auto_tokens` config (default null = manual-only). `tools/list` is now nine tools.
  Additive; `conformance.json` and the Sync artifact unchanged.

## [2.5.2] - 2026-07-05

### Added
- **Memory recall (L5)** — a local-only, secret-scrubbed `.cce/memory.jsonl` of *validated* decisions,
  with two MCP tools: `record_decision(text, tags?, area?)` (deduped by a content-hash id; redacted
  before write) and `session_recall(query, top_k?)` (hybrid search over the memory corpus,
  precision-filtered — score ≥ 0.30 and a shared query token — to avoid context pollution). Reuses the
  retrieval engine; workspace-aware (root + members); **never pushed by Sync** (non-reproducible /
  local). `tools/list` is now eight tools. Additive; `conformance.json` and the Sync artifact unchanged.

## [2.5.1] - 2026-07-05

### Added
- **Output compression (L4)** — `cce init` writes a leveled output-rules block into `CLAUDE.md`
  (`output.level`: `off | lite | standard | max`, default **standard**): terser answers and
  changed-lines-only code edits. New MCP tool **`set_output_compression`** dials the level for the
  running session (in-memory; does not rewrite `CLAUDE.md`). `tools/list` is now six tools.
  Additive; `conformance.json` and the Sync artifact are byte-for-byte unchanged.

## [2.5.0] - 2026-07-05

The first **Savings Layers** — retrieval returns compact chunks by default, with progressive
disclosure to recover detail on demand, and a seven-bucket savings ledger + eval harness that
make token savings measurable and honest.

### Added
- **Chunk compression (L2)** — `context_search` gains `detail: signature | compact | full`
  (default `compact`). AST-driven, per language pack: a container chunk renders as its header +
  doc + the signature lines of its direct members (methods, and Ruby model DSL such as
  `has_many`/`belongs_to`/`validates`); a leaf chunk as its signature + doc. **Retrieval-time
  only** — the index, `conformance.json`, and the Sync artifact are byte-for-byte unchanged.
- **Progressive disclosure (L7)** — new MCP tools `expand_chunk` (recover the full body / file
  slice / graph-neighbours of a chunk by `chunk_id`) and `related_context` (import-graph
  neighbours — imports **and** consumers). `tools/list` is now five tools.
- **Savings ledger** — a seven-bucket `savings` object on search events, a `savings_by_layer`
  panel on `/api/metrics`, and a `cce savings` command with an embedded offline price table.
  Every surface is labelled *"vs full-file baseline — not your real end-to-end agent cost."*
- **Deterministic token counter** `cce.tokens/v1` (`max(1, floor(bytes/4))`) and an in-repo A/B
  **eval harness** (`cce eval`) — correctness-gated, cost-primary.

### Notes
- Tool descriptions carry explicit trigger conditions and steer the agent to `expand_chunk`
  instead of re-searching (measured on a real ecosystem to matter).
- Rust-first: all new formats are byte-pinned so cce-ruby can reconcile to them in a later track.
- `SYNC_FORMAT_VERSION` stays `2.3` (decoupled from the app version); the Sync golden is unchanged.

## [2.4.1] - 2026-07-05

The **closing consolidation of the v2.4 milestone**: a refreshed dashboard that
surfaces the capabilities landed since v1.1, plus a verified, gapless, offline-first
documentation sweep. **Additive patch release** — the metrics schema grows only by
adding fields (older logs still parse), the base engine and single-repo
`conformance.json` are byte-for-byte unchanged, and `SYNC_FORMAT_VERSION` stays
`"2.3"` so the shared golden checksum
`581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` is untouched.

### Added

- **Dashboard refresh (`src/dashboard.rs`, `src/aggregator.rs`)** — four new panels:
  **agent-vs-human usage** (CLI vs MCP searches), **per-package breakdown**
  (savings/searches/quality per workspace member — now with `mean_top_score`),
  **index freshness** (indexed `sha`, local-vs-`sync-pull` source), and
  **secret-safety** (sensitive-files-skipped count). Every panel is **purely
  log-derived, so the dashboard makes zero network calls** and stays loopback-only,
  read-only, and self-contained (inline CSS/JS, hand-drawn SVG). Behind-remote lives
  in `cce sync status` / MCP `index_status`, not on the dashboard.
- **Metrics schema — additive fields.** `search` events carry
  `source: "cli" | "mcp"` (the CLI `search` path tags `"cli"`; the MCP
  `context_search` path tags `"mcp"`). `index` events carry `sha`, `source`
  (`"local"` for `cce index`, `"sync-pull"` for a `cce sync pull` install), and
  `sensitive_skipped`. Absent/unknown fields degrade gracefully (a pre-v2.4.1 search
  reads back as `"cli"`; an index event as `"local"`).
- **Aggregator sections.** `/api/metrics` gains `by_source`, `secret_safety`, and
  `index_freshness` (`{indexes, source, sha, indexed_ts}`) — all pure, log-derived,
  cross-language-identical — plus `totals.mean_top_score`. `by_package` (workspace)
  gains `mean_top_score` and is sorted by package. `cce sync pull` records a
  `sync-pull` index event so the pulled provenance is observable with no network call.
- **Documentation sweep** — a dedicated, **verified offline-first** section proving
  `index` / `search` / `stats` / `dashboard` / `workspace` / `cce mcp` all run with
  no network and no remote; macOS **and** Ubuntu setup with explicit prerequisites
  (toolchain, C compiler, git, git-LFS); a Sync + MCP best-practices section; and
  both an online and an offline cold-start transcript in
  [`docs/VERIFIED.md`](docs/VERIFIED.md).

### Changed

- `retriever::build_search_record` takes a `source` argument so the CLI and MCP
  search paths tag their metrics events.
- `cce sync pull` now appends a `sync-pull` `index` event to the metrics log so the
  dashboard's freshness panel is fully log-derived (no request-path network call).
- Version bumped to **2.4.1** (`Cargo.toml`, `CITATION.cff`). `SYNC_FORMAT_VERSION`
  deliberately **unchanged** at `"2.3"`.

## [2.4.0] - 2026-07-05

**CCE MCP** — a [Model Context Protocol](https://modelcontextprotocol.io) server
(`cce mcp`) so an agent (Claude Code) uses CCE as a **first-class tool it
auto-invokes** — running `context_search` instead of reading and grepping whole
files — plus `cce init` to wire an editor up plug-and-play. This closes the last
gap between the clean-room CCE and the original Python implementation: the agent
integration. Built test-first from [`SPEC-MCP.md`](SPEC-MCP.md). **Additive minor
release**: the CLI and single-repo `conformance.json` are untouched, and MCP is
read-only, offline, and does not require CCE Sync.

### Added

- **`cce mcp`** (`src/mcp/`) — an MCP server over stdio (JSON-RPC 2.0), pinning
  protocol version `2025-06-18`. Handles `initialize` (advertising
  `serverInfo { name: "cce", version }` and `capabilities { tools: {} }`),
  `notifications/initialized`, `tools/list`, `tools/call`, and `ping`. Resolves the
  store exactly like the CLI (`--dir` / `--store` / cwd, `--workspace`), is
  read-only, and answers a missing/empty index with a friendly "run `cce index`"
  message rather than crashing. The dispatch loop is transport-generic, so it is
  driven hermetically in tests by piping JSON-RPC to stdin.
- **Three tools** with schemas identical to the Ruby engine (the cross-language
  contract): `context_search` (ranked chunks for a query — the "PREFERRED over
  Read/Grep" tool — logging a `search` metrics event and returning a `query_id`),
  `index_status` (counts + sync freshness), and `record_feedback` (a `feedback`
  event closing the dashboard's quality loop).
- **`cce init [<dir>] [--agent claude] [--remote <sync-url>] [--force]`** — ensures
  an index (`cce sync pull --latest` when a remote is configured/passed, else a
  local `cce index` / workspace index), then merges an idempotent `cce` entry into
  `.mcp.json` and a marker-bounded block into `CLAUDE.md`, and prints next steps.
- **CCE MCP × CCE Sync (soft dependency)** — on startup, if a sync remote is
  configured and `sync.auto_pull` is on, `cce mcp` best-effort pulls the latest
  CI-built index (offline-safe; never blocks or errors). `index_status` reports the
  index source (local vs pulled), its sha, and whether it is behind the remote. MCP
  works fully with no Sync configured. New public `sync::commands::freshness`.
- **Docs** — a README "Use it with Claude Code (MCP)" section, [`docs/mcp.md`](docs/mcp.md),
  and a cold-start MCP transcript added to [`docs/VERIFIED.md`](docs/VERIFIED.md).

### Changed

- **Sync artifact format version decoupled from the app version** — introduced
  `sync::SYNC_FORMAT_VERSION` (`"2.3"`), which names the *artifact format* rather than
  the release, replacing the old `cce_version_minor()` that derived it from the crate
  version. CCE MCP is additive and does not change the artifact format, so the format
  version stays `2.3`: the content address stays `hash/2.3/…`, the manifest
  `cce_version` stays `"2.3"`, and the shared golden checksum on `test/fixture/samples`
  stays `581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` — so a v2.4
  release does **not** invalidate existing caches or diverge from the Ruby engine's
  artifacts. `SYNC_FORMAT_VERSION` moves only when the artifact bytes actually change.
- `retriever::build_search_record` was lifted out of `main.rs` into the library so
  the CLI `search` and the MCP `context_search` log a byte-identical metrics event.

## [2.3.0] - 2026-07-05

**CCE Sync** — a distributed, offline-first cache for the index: *git remotes for
the index*. Your local `.cce/` stays authoritative; an optional git-backed remote
is a **content-addressed cache** you push to and pull from. Because the index is
deterministic (hash embedder), a cache for `repo@sha` is byte-identical no matter
who — or which language engine — built it. Built test-first from
[`SPEC-SYNC.md`](SPEC-SYNC.md). **Additive minor release**: absent a configured
remote, every command behaves exactly as before and single-repo `conformance.json`
remains byte-identical.

### Added

- **Portable interchange artifact** (`src/sync/artifact.rs`) — a canonical,
  byte-exact, cross-language format (reconciled to the single spec in
  [`SPEC-SYNC-RECONCILE.md`](SPEC-SYNC-RECONCILE.md)): a UTF-8 stream with an LF
  after every line — the manifest line, one sorted-key compact-JSON object per chunk
  (sorted by `(file_path, start_line, id)`), then the graph line
  `{"edges":[…],"nodes":[…]}`. Embeddings are **standard base64 (with padding) of
  256 little-endian IEEE-754 `f64` bytes** (not decimals), so the bytes match across
  Ruby and Rust. **No provenance** (`built_at`/`built_by` removed) so the artifact is
  reproducible; `file_tokens` lives in the manifest; `pack_set_id` is the literal
  `c,javascript,python,ruby,rust,typescript`. `checksum` = lowercase-hex SHA-256
  over the whole stream serialized with `checksum` set to `""`. A committed **shared
  golden checksum** on `test/fixture/samples` anchors the format cross-language.
- **Content address** (`src/sync/mod.rs`) —
  `<embedder>/<cce_ver>/<repo_id>/<sha>.cce`; `repo_id` = normalized git origin
  (`host__org__repo`) or a `sync.repo_id` override. Only the `hash` embedder is
  shareable.
- **Git remote backend** (`src/sync/remote.rs`) — a `SyncRemote` trait with a
  `GitRemote` impl: a local working clone under `~/.cce/sync/<remote-id>/`,
  `put` = write at the content path + commit + push (fetch-rebase-retry on a ref
  race), `get` = fetch + read. `*.cce` blobs use **git-LFS** by default; the core
  path works over plain git (no `git-lfs` binary required for the tests).
- **CLI** (`src/sync/commands.rs`, `src/main.rs`) — `cce sync init`, `push`,
  `pull`, `status`, `verify`. `push` refuses a dirty tree or a non-hash index;
  `pull` installs the artifact into `.cce/` and never overwrites a different sha
  without `--force`; `pull --latest` follows a per-branch ref pointer; `verify`
  re-indexes locally and confirms the pulled checksum. All are **workspace-aware**
  (`--workspace`), each member keyed by its own `repo_id@sha`.
- **Config** (`src/sync/config.rs`) — `sync.remote`, `sync.lfs` (default true),
  `sync.repo_id`, `sync.auto_pull`, `sync.retention` under `<root>/.cce/config`
  (global `~/.cce/config.yml` fallback). Absent ⇒ pure local CCE.
- **Docs** — a README "CCE Sync" section with a verified end-to-end walkthrough,
  macOS/Ubuntu install incl. `git lfs install`, a ready-to-copy CI workflow
  ([`docs/ci/cce-sync.yml`](docs/ci/cce-sync.yml)), [`docs/sync.md`](docs/sync.md)
  (model, artifact format, content address, permissions, troubleshooting), and
  [`docs/VERIFIED.md`](docs/VERIFIED.md) (the cold-start transcript).

### Guarantees

- **Offline-first (normative).** No remote ⇒ every command behaves as today. A
  configured-but-unreachable remote ⇒ `sync` fails gracefully; all non-sync
  commands are unaffected. A failed push/pull never breaks local indexing or search.

## [2.2.0] - 2026-07-05

**Workspace mode** — CCE now understands an *ecosystem* of related codebases (e.g.
a Rails app + engines + a frontend under one root) as a single searchable whole,
while **each member stays isolated in its own store**. Built test-first from
[`SPEC-V2.2.md`](SPEC-V2.2.md). This is an **additive minor release**: absent
`--workspace`, every command behaves exactly as before and single-repo
`conformance.json` remains byte-identical.

### Added

- **Auto-detection + manifest** (`src/workspace.rs`). `cce workspace init [<dir>]
  [--force]` walks the root under the standard ignore rules and detects members by
  §3 markers — `*.gemspec` ⇒ Ruby (`ruby-engine` when an `app/`, `config/routes.rb`
  or `lib/**/engine.rb` marker is present, else `ruby-gem`); `Gemfile` +
  `config/application.rb` ⇒ `rails-app`; `package.json` ⇒ `typescript` (with
  `tsconfig.json`) or `javascript`. Members do **not** nest. Writes a deterministic
  `<dir>/.cce/workspace.yml` (members sorted by path, names collision-suffixed).
  Hand-written manifests are honoured. `cce workspace list` prints members + edges.
- **Federated indexing** — `cce index --workspace [<dir>]` indexes each member into
  its **own** `<member>/.cce/index.json` via the normal pipeline (language packs +
  secret scrubbing inherited). A member's store is **byte-identical to indexing that
  member standalone** (asserted). Then builds `<dir>/.cce/workspace-graph.json`.
- **Cross-member dependency edges (Level 1)** (SPEC-V2.2 §5). Declared deps are
  extracted from `*.gemspec` (`add_dependency`/`add_runtime_dependency`/
  `add_development_dependency`), `Gemfile` (`gem "name"`), and `package.json`
  (`dependencies`/`devDependencies`/`peerDependencies`); an edge `A → B` is recorded
  (with its `via`) when a dep `A` declares matches member `B`'s `package` or `name`.
  Deterministic: edges sorted by `(from, to, via)`.
- **Federated search** — `cce search "q" --workspace [<dir>] [--package a,b]
  [--top-k N] [--no-graph] [--json]`. Defined to equal the standard §6 retrieval run
  over the **union** of in-scope members' chunks (BM25 stats over the union;
  diversity key `(member, file_path)`). Each result is tagged with its `package` and
  member-relative `file_path`. Graph expansion adds the union of members' intra-store
  import graphs **plus** cross-member edges (a top result in `A` expands into a
  dependency target `B`). `--package` scopes to named members (errors on an unknown
  name).
- **Workspace stats & dashboard** — `cce stats --workspace` (per-member + totals +
  edges) and `cce dashboard --workspace` (a roll-up over every member's
  `metrics.jsonl` plus a `by_package` breakdown; loopback-only, read-only,
  self-contained, unchanged posture).
- Fixture ecosystem `test/fixture/workspace/` (`app` / `billing` / `web`) plus 10
  end-to-end CLI tests and unit tests covering detection, each dependency extractor,
  per-member byte-identical isolation, federation-equals-union, `--package` scoping
  (+ unknown-name error), the cross-member graph hop, stats and dashboard roll-up,
  and a re-assert that single-repo `conformance.json` is byte-identical.

### Changed

- `retriever` is refactored to expose `rank_core` (the §6 ranking without graph
  expansion) so federated search runs the **identical** pipeline over the union
  corpus. `store::Index::from_parts` and `graph_store::Graph::{out_pairs,from_pairs}`
  support building the combined corpus. Single-repo behaviour is unchanged.
- New pinned dependency `serde_yaml = "=0.9.34"` (parsing hand-written manifests;
  the manifest is emitted by a byte-deterministic hand-rolled writer).
- Version bumped to **2.2.0** (`Cargo.toml`, `CITATION.cff`).

## [2.1.0] - 2026-07-05

**Secret & sensitive-file protection**, built test-first from
[`SPEC-V2.1.md`](SPEC-V2.1.md). Indexing becomes **secret-safe by default** in two
layers, with an explicit opt-out. This is an **additive minor release**: the base
engine is untouched and `conformance.json` remains byte-identical.

### Added

- **Layer 1 — sensitive files are never read** (`src/sensitive.rs`). Before the
  walker reads a file, its basename is tested against a fixed policy: sensitive
  extensions (`pem`, `key`, `p12`, `pfx`, `keystore`, `jks`, `ppk`, `der`, `asc`),
  exact basenames (`credentials.*`, `secrets.*`, `.netrc`, `.pgpass`, `.htpasswd`,
  `.dockercfg`, `kubeconfig`, `id_rsa`/`id_dsa`/`id_ecdsa`/`id_ed25519`), and the
  **dotenv rule** (`.env` / `.env.*` are sensitive **except** safe templates ending
  `.example`/`.sample`/`.template`/`.dist`). Skipped files are counted separately
  as **`sensitive skipped`** in the `index` summary and never read into memory.
- **Layer 2 — secrets are redacted before chunking** (`src/redactor.rs`). Each
  indexed file's content is scrubbed for high-confidence secrets — private-key
  blocks, AWS/GitHub/Slack/Stripe/OpenAI/Anthropic/Google keys, JWTs, and a
  guarded generic `key = value` assignment — replaced with `[REDACTED:<LABEL>]`
  **before** it is chunked, embedded, or stored, so the store never contains the
  raw value and `chunk_id`/`token_count` derive from the redacted text. A
  placeholder guard leaves documentation examples (`API_KEY="your-api-key-here"`),
  interpolations, and literals untouched. Redaction is deterministic, so the
  cross-language equivalence guarantee still holds.
- **`--allow-secrets`** flag on `cce index` (default off ⇒ protection **on**)
  disables both layers for a run and prints a warning; content is then indexed
  verbatim.
- Fixture corpus `test/fixture/secrets/` (`.env`, `.env.example`, `id_rsa`,
  `config.rb`) plus an end-to-end acceptance test of the skip/redact/opt-out
  behaviour.
- Test suite grows to 154 hermetic tests (+1 `#[ignore]` Ollama) at 95.08% line
  coverage (`cargo llvm-cov`).

### Changed

- `cce index` summary adds a `sensitive skipped : N` line (and widens the label
  column). No change to the store schema or to `conformance.json`.
- New pinned dependency: `regex = "=1.12.4"` (redaction patterns).

## [2.0.0] - 2026-07-05

Pluggable **language packs**, built test-first from [`SPEC-V2.md`](SPEC-V2.md).
Language support is factored out of the core into self-contained packs, four new
languages ship, and every chunk gains a `kind` field. **This is a breaking
release**: the conformance output shape changes and the supported-language set
changes.

### Added

- **Language-pack architecture** — a `LanguagePack` trait (`src/packs/`) plus a
  registry resolve files to packs by extension. The core chunker/importer
  (`src/chunker.rs`) references **no language by name**; a guard test enforces it.
  Adding a language is one pack file + registration + validation — no core edits.
- **Four new languages**: **Ruby**, **Rust**, **TypeScript**, and **C** packs,
  joining the converted **Python** and **JavaScript** packs (six total). New
  grammar crates pinned in `Cargo.toml` (`tree-sitter-ruby`, `-rust`,
  `-typescript`, `-c`), ABI-compatible with the pinned `tree-sitter` core.
- **`kind` field on every chunk** — the exact tree-sitter node type (e.g.
  `struct_specifier`, `trait_item`, `interface_declaration`, `method`), carried
  through persistence, `search` (human + `--json`), `stats` (a by-kind
  breakdown), and conformance. `kind` is not part of `chunk_id`.
- **Three-layer pack validators** (`src/packs/validators.rs`): structural lint,
  grammar-binding lint with "did you mean" node-kind suggestions, and a
  behavioural self-test (min function/class counts, kinds present, and
  `extract_imports == expected` exactly). Surfaced by **`cce packs`** /
  **`cce packs --validate`**, a CI test gate over every pack, and cheap fail-fast
  startup checks.
- **Sample corpus** at `test/fixture/samples/` (seven files) — both the pack
  self-tests and the cross-language conformance corpus.
- **Per-language benchmarks** — `cce bench --lang ruby|rust|typescript|c` with the
  labeled query sets from SPEC-V2 §8; measured numbers for Ruby (sinatra), Rust
  (hyperfine), TypeScript (zustand), and C (jq) in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).
- New guide [`docs/adding-a-language.md`](docs/adding-a-language.md); README,
  architecture, how-to, getting-started, `llms.txt`, and `AGENTS.md` swept of the
  Python/JavaScript-only framing.
- Test suite grows to 129 tests at 94.76% line coverage (`cargo llvm-cov`).

### Changed (breaking)

- **Conformance output shape** — `cce conformance` now targets
  `test/fixture/samples`, tags `spec_version` `"2.0"`, adds `kind` to every chunk
  object, and drops the query section (the chunk array is the equivalence gate).
- **Supported-language set** — six AST-aware packs instead of two.
- **Module-fallback line count** — the fallback chunk's `end_line` is now
  `(number of "\n" bytes) + 1` (a trailing newline counts its line), closing the
  one v1 cross-language divergence. This changes fallback `chunk_id`s.
- The base v1 fixture moved to `test/fixture/base/` so the samples corpus is
  independent.

## [1.1.0] - 2026-07-05

Dashboard & observability, built test-first from
[`DASHBOARD-SPEC.md`](DASHBOARD-SPEC.md) (SPEC v1.1). The base engine (chunking,
embedding, retrieval) is unchanged and stays byte-for-byte conformant —
`conformance.json` is identical to the 1.0.0 release.

### Added

- Persisted metrics event log (`.cce/metrics.jsonl`): `cce search`, `cce index`,
  and the new `cce feedback` each append one best-effort/fail-open JSON line. The
  metrics subsystem is the one place real wall-clock time and unique IDs are used;
  the clock and id source are injected so tests stay deterministic.
- `cce feedback <query-id> --helpful|--not-helpful [--note ...]` — rate a past
  search result. `cce search` now prints a `query-id` (and adds `query_id` to
  `--json`, which is now an object wrapping the `results` array).
- Whole-file token counts persisted per indexed file so a search's
  `baseline_tokens` (the "read the whole file" counterfactual) is accurate.
- Pure aggregator (`aggregator.rs`): totals, two north-stars (token/cost SAVINGS
  and retrieval QUALITY) with current-vs-prior windowed deltas and an
  improving/degrading/flat direction, a daily series, and a recent-searches view.
  Reproduces the DASHBOARD-SPEC §4.1 anchor exactly.
- `cce dashboard [--dir DIR|--store PATH] [--port N] [--metrics PATH] [--no-open]`
  — a loopback-only (`127.0.0.1`), read-only, fully self-contained web server
  (inline CSS/JS, hand-drawn SVG charts, no external network/CDN) serving
  `GET /`, `GET /api/metrics`, and `GET /api/health`. Hand-rolled on
  `std::net::TcpListener` — no new dependency.
- `--no-metrics` flag on `index`/`search`; the metrics log format (`.jsonl`) is
  excluded from indexing so it never pollutes the corpus.
- Docs: new [`docs/dashboard.md`](docs/dashboard.md) (pipeline, schema, formulas,
  "where this would strain"); README, `docs/how-to.md`, `SECURITY.md`,
  `llms.txt`, and `AGENTS.md` updated.
- Test suite grows to 113 tests (112 hermetic + 1 `#[ignore]` Ollama) at 95.44%
  line coverage (`cargo llvm-cov`).

## [1.0.0] - 2026-07-05

Initial public release: a clean-room, test-first Rust implementation of the Code
Context Engine, built solely from [`SPEC.md`](SPEC.md) (SPEC v1.0).

### Added

- `cce index` — walk a directory, AST-chunk files with tree-sitter (Python and
  JavaScript, with a whole-file `module` fallback for other languages), embed
  each chunk, and persist a JSON store (vectors + BM25 + import graph).
- `cce search` — hybrid retrieval (exact cosine + Lucene-form BM25) fused with
  Reciprocal Rank Fusion, a confidence blend, a test/doc path penalty, a per-file
  diversity cap, and optional import-graph expansion; human and `--json` output.
- `cce stats` — summary of a persisted store (chunks, files, tokens, languages).
- `cce bench` — benchmark the pipeline on a real repository and write
  `docs/BENCHMARKS.md`.
- `cce conformance` — emit a byte-stable `conformance.json` for cross-language
  verification against the Ruby sibling.
- Deterministic FNV-1a hashing embedder (default, offline) and an optional,
  opt-in local Ollama embedder (`--embedder ollama`) with graceful fallback.
- Determinism guarantees: 6-decimal round-half-away-from-zero and `chunk_id`
  tie-breaking throughout (SPEC §5.3).
- Test suite of 84 tests (83 hermetic + 1 `#[ignore]` Ollama) at 95.33% line
  coverage (`cargo llvm-cov`).
- Project documentation: `SPEC.md`, `docs/architecture.md`, `docs/getting-started.md`,
  `docs/how-to.md`, `docs/DECISIONS.md`, `docs/TDD.md`, `docs/BENCHMARKS.md`.

[Unreleased]: https://github.com/davidslv/cce-rust/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/davidslv/cce-rust/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/davidslv/cce-rust/compare/v1.1.0...v2.0.0
[1.1.0]: https://github.com/davidslv/cce-rust/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/davidslv/cce-rust/releases/tag/v1.0.0
