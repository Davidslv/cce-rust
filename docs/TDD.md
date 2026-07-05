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

## Final numbers

- **Tests:** 84 total — 69 library unit tests + 14 CLI integration tests + 1
  `#[ignore]` Ollama test. 83 run in the default suite; **all pass**, 0 failures.
- **Coverage (`cargo llvm-cov`):** 95.33% lines / 95.69% regions overall — up
  from 86.95% lines after the coverage-hardening pass, and well above the ≥85%
  target (SPEC §12). Per-file lines: tokenizer 100%, vector_store 100%,
  config 100%, conformance 100%, store 99%, keyword_store 99%, graph_store 98%,
  bench 97%, chunker 95%, retriever 95%, main 94%, walker 92%, embedder 90%
  (the remaining embedder gap is the Ollama HTTP **success** path, which by
  design is never exercised without a live server).

## Full suite output (`cargo test`)

```
running 69 tests
test bench::tests::bench_runs_on_fixture ... ok
test bench::tests::percentile_nearest_rank ... ok
test chunker::tests::chunk_id_changes_with_path_or_lines ... ok
test chunker::tests::chunk_id_is_deterministic_and_16_hex ... ok
test chunker::tests::js_class_and_method_and_arrow ... ok
test chunker::tests::js_imports_segments ... ok
test chunker::tests::parse_failure_or_other_ext_is_fallback ... ok
test chunker::tests::payments_fixture_chunks_and_import ... ok
test chunker::tests::python_fixture_chunks ... ok
test chunker::tests::python_import_first_component ... ok
test chunker::tests::python_no_symbols_is_fallback_with_python_language ... ok
test chunker::tests::readme_fallback_module_chunk ... ok
test chunker::tests::token_count_rule ... ok
test config::tests::config_default_uses_hash_embedder ... ok
test config::tests::parse_defaults_to_hash_for_unknown ... ok
test config::tests::parse_selects_ollama_case_insensitively ... ok
test conformance::tests::chunks_sorted_and_scores_are_6dp_strings ... ok
test conformance::tests::deterministic_output ... ok
test conformance::tests::has_seven_chunks_and_three_queries ... ok
test embedder::tests::cosine_anchor ... ok
test embedder::tests::embed_deterministic ... ok
test embedder::tests::embed_empty_is_all_zeros ... ok
test embedder::tests::embed_is_l2_normalized ... ok
test embedder::tests::fnv_anchor_a ... ok
test embedder::tests::fnv_anchor_empty ... ok
test embedder::tests::fnv_anchor_foobar ... ok
test embedder::tests::format6_rounds_and_signs ... ok
test embedder::tests::hash_embed_batch_uses_default_trait_impl ... ok
test embedder::tests::ollama_default_has_expected_url_and_model ... ok
test embedder::tests::ollama_embed_batch_falls_back_to_empty_vecs_on_failure ... ok
test embedder::tests::ollama_embed_empty_text_is_empty_without_request ... ok
test embedder::tests::ollama_embed_falls_back_to_empty_on_failure ... ok
test embedder::tests::ollama_healthy_is_false_when_unreachable ... ok
test embedder::tests::ollama_try_embed_batch_errors_when_unreachable ... ok
test embedder::tests::round6_rounds_half_away_from_zero ... ok
test embedder::tests::round_half_away_from_zero ... ok
test embedder::tests::score_key_matches_format6 ... ok
test graph_store::tests::edge_from_import ... ok
test graph_store::tests::neighbors_both_directions ... ok
test graph_store::tests::resolve_by_path_suffix ... ok
test graph_store::tests::unresolved_module_no_edge ... ok
test keyword_store::tests::worked_anchor_example ... ok
test keyword_store::tests::zero_score_docs_excluded ... ok
test retriever::tests::conformance_q1_top1_is_hash_password ... ok
test retriever::tests::conformance_q2_top1_is_process_payment ... ok
test retriever::tests::conformance_q3_top1_from_auth ... ok
test retriever::tests::diversity_cap_respected ... ok
test retriever::tests::empty_query_returns_empty ... ok
test retriever::tests::graph_expansion_adds_related_file_chunks ... ok
test retriever::tests::intent_classification ... ok
test retriever::tests::rrf_anchor ... ok
test retriever::tests::scores_are_deterministic_across_runs ... ok
test store::tests::builds_seven_chunks_from_fixture ... ok
test store::tests::default_store_path_appends_cce_index_json ... ok
test store::tests::load_invalid_json_is_an_error ... ok
test store::tests::load_legacy_json_without_embedder_defaults_to_hash ... ok
test store::tests::reindex_is_idempotent ... ok
test store::tests::save_creates_missing_parent_directories ... ok
test store::tests::save_load_roundtrip ... ok
test tokenizer::tests::anchor_camelcase_not_split ... ok
test tokenizer::tests::anchor_empty ... ok
test tokenizer::tests::anchor_hash_password ... ok
test tokenizer::tests::anchor_select ... ok
test tokenizer::tests::no_dedup_and_order ... ok
test tokenizer::tests::non_ascii_is_separator ... ok
test tokenizer::tests::underscore_and_digits_kept ... ok
test vector_store::tests::ranks_closest_first ... ok
test vector_store::tests::ties_break_by_chunk_id ... ok
test walker::tests::ignore_rules ... ok

test result: ok. 69 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

     Running tests/cli.rs
running 14 tests
test bench_invalid_dir_exits_nonzero ... ok
test bench_runs_on_tiny_local_repo ... ok
test bench_with_explicit_commit_and_name ... ok
test conformance_invalid_dir_exits_nonzero ... ok
test conformance_is_deterministic ... ok
test index_then_search_in_fresh_process ... ok
test index_with_ollama_embedder_falls_back_gracefully ... ok
test index_without_store_uses_default_path_and_search_resolves_it ... ok
test invalid_index_dir_exits_nonzero ... ok
test search_missing_store_exits_nonzero ... ok
test search_with_no_matches_prints_no_results ... ok
test stats_missing_store_exits_nonzero ... ok
test stats_on_empty_index_reports_zero_averages ... ok
test stats_reports_counts ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

     Running tests/ollama.rs
running 1 test
test ollama_embeds_when_available ... ignored, requires a local Ollama server; run with --ignored
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
```
