# TDD log

Built test-first (SPEC §12): for each unit of behavior a failing test was written
first (red), then the minimum code to pass (green), then refactor with tests
green. Tests are deterministic and hermetic — no network in the default suite
(the one Ollama integration test is `#[ignore]`).

## Build order (red → green)

1. **tokenizer** — wrote the four SPEC §4.1 anchor tests first (`hashPassword`,
   `SELECT`, empty, camelCase-not-split). Red (no `tokenize`), then implemented
   the byte-scan. Added non-ASCII-separator and no-dedup tests.
2. **embedder / FNV** — pinned the three published FNV-1a-64 vectors (empty, `a`,
   `foobar`) as red tests, then implemented `fnv1a64`. Added the cosine anchor
   (`0.6`), L2-normalization, all-zeros-on-empty, and determinism.
3. **rounding** — `format6` / `score_key` tests for round-half-away-from-zero and
   sign handling, pinning the SPEC §5.3 rule before wiring it into sorts.
4. **chunker** — token_count rule and chunk_id determinism/16-hex first. Then the
   fixture structural tests: `auth.py` → 4 chunks (fn, fn, class, fn), `payments.py`
   → 2 + import `auth`, `README.md` → 1 `module` fallback (plaintext, lines 1–2).
   Then import extraction (Python first-component, JS segments) and fallback paths
   (other extension, python-with-no-symbols).
5. **vector_store** — closest-first ranking and chunk_id tie-break.
6. **keyword_store** — the SPEC §6.3 **worked anchor** first (D1/D2, avgdl 2.5,
   idf ln 2 = 0.693147, score(D1) = 0.902273, D2 excluded), then zero-score
   exclusion.
7. **graph_store** — edge from import, both-direction neighbors, path-suffix
   resolution, unresolved-module no-edge.
8. **store** — fixture builds 7 chunks; save/load round-trip preserves embeddings
   across a reload; re-index idempotent (identical chunk IDs).
9. **retriever** — the SPEC §6.4 **RRF anchor** (vrank 0 + frank 2 → 1/60 + 1/62
   = 0.032796); intent classification; the three SPEC §8.2 conformance top-1
   expectations; empty-query-empty; diversity cap ≤ 3/file; graph expansion pulls
   the imported neighbor file; score determinism across runs.
10. **conformance** — deterministic (twice-identical) output; 7 chunks + 3 queries;
    chunks sorted; scores as 6-decimal strings; Q1 top-1 from `auth.py`.
11. **bench** — nearest-rank percentile; end-to-end runner on the fixture.
12. **CLI (integration, `tests/cli.rs`)** — index-then-search in a **fresh
    process**; stats; conformance byte-identical across two subprocess runs;
    invalid index dir and missing store exit non-zero. Coverage-hardening pass
    added: default store-path resolution (`--dir` and bare cwd), human (non-JSON)
    search output, the "(no results)" empty-query path, `--embedder ollama`
    graceful fallback, empty-index stats (`avg token/chunk: 0.0`), missing-store
    `stats`, `bench` against a tiny local temp repo (default `unknown` commit and
    explicit `--commit`/`--name`), and invalid-dir exits for `bench`/`conformance`.
13. **embedder / Ollama graceful failure (`embedder.rs` unit tests)** — the
    `OllamaEmbedder` error path exercised **hermetically** against a closed local
    port (`127.0.0.1:1`, connection refused — no server contacted): `try_embed_batch`
    returns `Err`, `healthy()` is false, `embed`/`embed_batch` fall back to empty
    vectors, empty text short-circuits, plus `round6` and the default
    `embed_batch` trait method.
14. **config / store edges** — `EmbedderKind::parse` (case-insensitive ollama,
    unknown → hash), `Config::default`, `default_store_path`, save into missing
    parent dirs, legacy JSON without an `embedder` field, and invalid-JSON load error.

## Notable red→green moments

- The FNV anchors caught the need for **wrapping** 64-bit multiply; a plain
  multiply overflow-panics in debug. Green came from `wrapping_mul`.
- The first import extraction used a `Vec`-as-stack DFS which **reversed**
  first-seen order; the `python_import_first_component` test (`os, pkg, hashlib`)
  went red, driving a switch to proper recursive pre-order.
- After adding the SPEC §10.1 "Python sources only" filter to `cce bench`, the
  bench fixture test went red (7 → 6 chunks, README excluded) and was corrected —
  a good check that the filter actually took effect.

## v1.1 — Dashboard & observability (red → green)

The dashboard feature (DASHBOARD-SPEC v1.1) was added test-first on top of the
v1.0 engine, which stayed byte-for-byte conformant throughout:

1. **Walker** — a `jsonl_logs_are_skipped` test drove excluding `.jsonl` from the
   corpus, so the metrics sample fixture never adds an 8th conformance chunk.
   Re-ran `cce conformance` before/after: byte-identical.
2. **Store** — `persists_whole_file_token_counts_and_baseline_sums` drove the
   per-file token map (DASH §3) and the `baseline_tokens` distinct-file sum,
   including a save/load round-trip and a legacy-store default.
3. **Metrics** — tests for ISO-8601↔epoch round-trips, the injected clock/id
   append round-trip, `--no-metrics` suppression, corrupt/blank-line skipping,
   the fail-open bad-path case, and a 12-hex id format drove `src/metrics.rs`.
4. **Aggregator** — the DASH §4.1 **anchor** (totals, both north-stars, the daily
   series, recent-searches feedback resolution) was written first from the spec's
   exact expected numbers and reproduced on the first green; plus an empty-log
   "no data" aggregate and the direction rule.
5. **Dashboard** — `route`-level tests for `/`, `/api/metrics`, `/api/health`, a
   404, and the self-contained-HTML check, then a real-socket integration test
   (`tests/dashboard.rs`) binding an ephemeral loopback port.
6. **CLI** — `tests/metrics_cli.rs` drove `cce search` appending an event +
   printing the query-id (and the `--json` object shape), `cce feedback`
   recording and resolving into recent-searches, `--no-metrics`, and the
   exactly-one-verdict / unknown-id paths. The one existing test that read the old
   `--json` array was updated to the new `{query_id, results}` object.

Notable moment: the "self-contained" HTML assertion first tripped on the SVG/XHTML
XML **namespace** URIs (`http://www.w3.org/...`) — which are identifiers, never
fetched — so the test was sharpened to forbid real resource loads (`<link`,
`src=`, `cdn`, `@import`) instead of the substring `http`.

## Final numbers

- **Tests:** 113 total — 92 library unit tests + 14 base CLI + 5 metrics CLI + 1
  dashboard socket + 1 `#[ignore]` Ollama. 112 run in the default suite; **all
  pass**, 0 failures.
- **Coverage (`cargo llvm-cov`):** 95.44% lines overall — above the ≥92% v1.1
  target and in line with the v1.0 baseline (95.33%). New modules: aggregator
  99% lines, metrics 98%, dashboard 91% (the uncovered part is the forever-loop
  `run` accept path, exercised only via the bounded `serve` in tests). The base
  engine and `conformance.json` are unchanged.

## Full suite output (`cargo test`)

Abridged to the `test ... ok` lines and per-binary results (113 tests: 92 lib
+ 14 base CLI + 5 metrics CLI + 1 dashboard socket + 1 ignored Ollama):

```
running 92 tests
test aggregator::tests::direction_rule ... ok
test bench::tests::percentile_nearest_rank ... ok
test aggregator::tests::empty_log_is_a_valid_no_data_aggregate ... ok
test chunker::tests::chunk_id_changes_with_path_or_lines ... ok
test chunker::tests::chunk_id_is_deterministic_and_16_hex ... ok
test chunker::tests::parse_failure_or_other_ext_is_fallback ... ok
test aggregator::tests::anchor_savings_north_star ... ok
test aggregator::tests::anchor_quality_north_star ... ok
test aggregator::tests::recent_searches_newest_first_with_feedback_resolved ... ok
test aggregator::tests::anchor_totals ... ok
test aggregator::tests::anchor_daily_series ... ok
test chunker::tests::js_class_and_method_and_arrow ... ok
test chunker::tests::readme_fallback_module_chunk ... ok
test chunker::tests::python_no_symbols_is_fallback_with_python_language ... ok
test chunker::tests::token_count_rule ... ok
test chunker::tests::js_imports_segments ... ok
test chunker::tests::python_import_first_component ... ok
test config::tests::config_default_uses_hash_embedder ... ok
test config::tests::parse_defaults_to_hash_for_unknown ... ok
test config::tests::parse_selects_ollama_case_insensitively ... ok
test chunker::tests::payments_fixture_chunks_and_import ... ok
test chunker::tests::python_fixture_chunks ... ok
test dashboard::tests::missing_log_yields_empty_but_valid_metrics ... ok
test dashboard::tests::unknown_path_is_404 ... ok
test dashboard::tests::health_reports_event_and_skipped_counts ... ok
test dashboard::tests::query_string_is_ignored ... ok
test dashboard::tests::root_serves_self_contained_html ... ok
test embedder::tests::cosine_anchor ... ok
test embedder::tests::embed_deterministic ... ok
test dashboard::tests::metrics_endpoint_is_aggregate_plus_generated_ts ... ok
test embedder::tests::embed_empty_is_all_zeros ... ok
test embedder::tests::fnv_anchor_a ... ok
test embedder::tests::embed_is_l2_normalized ... ok
test embedder::tests::fnv_anchor_empty ... ok
test embedder::tests::fnv_anchor_foobar ... ok
test embedder::tests::format6_rounds_and_signs ... ok
test embedder::tests::hash_embed_batch_uses_default_trait_impl ... ok
test embedder::tests::ollama_default_has_expected_url_and_model ... ok
test embedder::tests::ollama_embed_empty_text_is_empty_without_request ... ok
test embedder::tests::round6_rounds_half_away_from_zero ... ok
test embedder::tests::round_half_away_from_zero ... ok
test embedder::tests::score_key_matches_format6 ... ok
test graph_store::tests::edge_from_import ... ok
test conformance::tests::has_seven_chunks_and_three_queries ... ok
test graph_store::tests::neighbors_both_directions ... ok
test conformance::tests::chunks_sorted_and_scores_are_6dp_strings ... ok
test graph_store::tests::unresolved_module_no_edge ... ok
test graph_store::tests::resolve_by_path_suffix ... ok
test keyword_store::tests::worked_anchor_example ... ok
test keyword_store::tests::zero_score_docs_excluded ... ok
test embedder::tests::ollama_embed_batch_falls_back_to_empty_vecs_on_failure ... ok
test metrics::tests::epoch_and_known_dates_round_trip ... ok
test embedder::tests::ollama_embed_falls_back_to_empty_on_failure ... ok
test embedder::tests::ollama_try_embed_batch_errors_when_unreachable ... ok
test embedder::tests::ollama_healthy_is_false_when_unreachable ... ok
test metrics::tests::hex_id_source_is_12_lowercase_hex_and_unique ... ok
test metrics::tests::malformed_timestamps_are_none ... ok
test metrics::tests::parse_log_skips_blank_and_corrupt_lines ... ok
test metrics::tests::read_missing_log_is_empty ... ok
test metrics::tests::disabled_writer_writes_nothing ... ok
test metrics::tests::bad_path_is_fail_open_not_a_panic ... ok
test conformance::tests::deterministic_output ... ok
test retriever::tests::conformance_q3_top1_from_auth ... ok
test retriever::tests::empty_query_returns_empty ... ok
test retriever::tests::conformance_q1_top1_is_hash_password ... ok
test retriever::tests::conformance_q2_top1_is_process_payment ... ok
test retriever::tests::intent_classification ... ok
test retriever::tests::diversity_cap_respected ... ok
test retriever::tests::rrf_anchor ... ok
test store::tests::default_store_path_appends_cce_index_json ... ok
test metrics::tests::append_with_injected_clock_and_id_round_trips ... ok
test retriever::tests::graph_expansion_adds_related_file_chunks ... ok
test tokenizer::tests::anchor_camelcase_not_split ... ok
test store::tests::load_invalid_json_is_an_error ... ok
test store::tests::load_legacy_json_without_embedder_defaults_to_hash ... ok
test tokenizer::tests::anchor_empty ... ok
test tokenizer::tests::anchor_hash_password ... ok
test tokenizer::tests::anchor_select ... ok
test tokenizer::tests::no_dedup_and_order ... ok
test tokenizer::tests::non_ascii_is_separator ... ok
test store::tests::builds_seven_chunks_from_fixture ... ok
test tokenizer::tests::underscore_and_digits_kept ... ok
test vector_store::tests::ranks_closest_first ... ok
test vector_store::tests::ties_break_by_chunk_id ... ok
test retriever::tests::scores_are_deterministic_across_runs ... ok
test walker::tests::jsonl_logs_are_skipped ... ok
test store::tests::reindex_is_idempotent ... ok
test store::tests::save_load_roundtrip ... ok
test store::tests::persists_whole_file_token_counts_and_baseline_sums ... ok
test store::tests::save_creates_missing_parent_directories ... ok
test walker::tests::ignore_rules ... ok
test bench::tests::bench_runs_on_fixture ... ok
test result: ok. 92 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
running 14 tests
test conformance_invalid_dir_exits_nonzero ... ok
test bench_invalid_dir_exits_nonzero ... ok
test invalid_index_dir_exits_nonzero ... ok
test search_missing_store_exits_nonzero ... ok
test stats_missing_store_exits_nonzero ... ok
test stats_on_empty_index_reports_zero_averages ... ok
test index_then_search_in_fresh_process ... ok
test search_with_no_matches_prints_no_results ... ok
test index_with_ollama_embedder_falls_back_gracefully ... ok
test conformance_is_deterministic ... ok
test index_without_store_uses_default_path_and_search_resolves_it ... ok
test stats_reports_counts ... ok
test bench_with_explicit_commit_and_name ... ok
test bench_runs_on_tiny_local_repo ... ok
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.51s
running 1 test
test serves_page_api_and_health_on_ephemeral_port ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
running 5 tests
test feedback_for_unknown_id_warns_but_records ... ok
test index_no_metrics_writes_no_log ... ok
test feedback_requires_exactly_one_verdict ... ok
test no_metrics_suppresses_the_search_event ... ok
test search_appends_event_prints_query_id_and_feedback_resolves ... ok
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
running 1 test
test ollama_embeds_when_available ... ignored, requires a local Ollama server; run with --ignored
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
