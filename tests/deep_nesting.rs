//! # tests/deep_nesting — deterministic regression tests for issue #49
//!
//! **Why this file exists:** The #33 property suite caught a real crash on CI:
//! a generated input SIGSEGV'd the `property_chunkers` binary, and because a
//! SIGSEGV kills the process before proptest persists its seed, no minimal case
//! survived. The crash class was reproduced deterministically: (1) the chunkers'
//! per-node recursive tree walks overflowed the thread stack on deeply nested
//! input (at a 256 KiB stack around depth 219; at the 2 MiB test-thread default
//! around depth 875–1748 depending on the grammar), and (2) tree-sitter-md's
//! external scanner overruns tree-sitter's fixed 1024-byte serialization buffer
//! at ~255 simultaneously open blocks (one line of 255 `>` characters), which is
//! an abort in debug and memory corruption in release, independent of stack size.
//!
//! **What it is / does:** Chunks pathological inputs — nesting just under the old
//! crash threshold and far past it — on a deliberately tiny (256 KiB) thread
//! stack, asserting the walk is iterative (survives any depth) and the markdown
//! scanner guard degrades to the whole-doc fallback instead of crashing. Inputs
//! are synthesized, capped at a few hundred KB, and run in milliseconds.
//!
//! **Responsibilities:**
//! - Pin the "never SIGSEGV on nested input" contract for both chunkers.
//! - It does NOT assert chunk bytes (the goldens do) beyond the fallback shape.

use cce::chunker::Chunker;
use cce::markdown::{chunk_markdown, PREAMBLE_KIND};

/// The old recursive walk overflowed a 256 KiB stack around depth 219; running
/// on this stack makes any reintroduced recursion fail loudly on every platform
/// instead of only on one CI runner's stack layout.
const SMALL_STACK: usize = 256 * 1024;

/// Run `f` on a small-stack thread; a panic (or an overflow abort) fails the test.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(f)
        .expect("spawn small-stack thread")
        .join()
        .expect("chunking must complete on a small stack");
}

/// `depth` nested parenthesized expressions inside a JS function (~2·depth bytes).
fn deep_js(depth: usize) -> String {
    format!("function f() {{\n{}1{}\n}}\n", "(".repeat(depth), ")".repeat(depth))
}

/// `depth` nested blocks inside a C function (~4·depth bytes).
fn deep_c(depth: usize) -> String {
    format!("void f(void) {{\n{}int x = 0;\n{}}}\n", "{\n".repeat(depth), "}\n".repeat(depth))
}

/// `depth` nested `if … end` in Ruby (flat indentation, ~8·depth bytes).
fn deep_ruby(depth: usize) -> String {
    format!("{}y = 1\n{}", "if x\n".repeat(depth), "end\n".repeat(depth))
}

#[test]
fn code_chunker_survives_nesting_just_under_the_old_crash_depth_on_a_small_stack() {
    // 200 < 219, the old recursive walk's crash depth at a 256 KiB stack: the
    // pre-fix code dies here; the iterative walk must not.
    on_small_stack(|| {
        let mut ck = Chunker::new();
        assert!(!ck.chunk_file("gen/deep.js", &deep_js(200)).chunks.is_empty());
        assert!(!ck.chunk_file("gen/deep.c", &deep_c(200)).chunks.is_empty());
        assert!(!ck.chunk_file("gen/deep.rb", &deep_ruby(200)).chunks.is_empty());
    });
}

#[test]
fn code_chunker_survives_pathological_nesting_far_past_the_old_crash_depth() {
    // 100k deep — two orders of magnitude past every measured crash threshold
    // (input stays ~200–800 KB per language). The parse succeeds and the single
    // enclosing function is still emitted as a chunk.
    on_small_stack(|| {
        let mut ck = Chunker::new();
        for (path, input) in [
            ("gen/deep.js", deep_js(100_000)),
            ("gen/deep.c", deep_c(100_000)),
            ("gen/deep.rb", deep_ruby(50_000)),
        ] {
            let fc = ck.chunk_file(path, &input);
            assert!(!fc.chunks.is_empty(), "[{path}] deep nesting must still chunk");
        }
    });
}

#[test]
fn markdown_chunker_survives_deep_blockquote_and_list_nesting_on_a_small_stack() {
    // Deep but under the scanner guard: parsed normally (the heading is found),
    // proving the walk itself no longer recurses. 100 quote levels was already
    // past the old recursive walk's comfort zone at this stack size.
    on_small_stack(|| {
        let mut quotes = String::from("# Title\n\n");
        for i in 0..100 {
            quotes.push_str(&"> ".repeat(i + 1));
            quotes.push_str("q\n");
        }
        let chunks = chunk_markdown("gen/deep.md", &quotes, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "Title", "under-guard input must be parsed, not fallback");
    });
}

#[test]
fn markdown_chunker_degrades_to_whole_doc_fallback_past_the_scanner_limit() {
    // One line of 255 `>` characters opens ~255 blocks — exactly the input that
    // overruns tree-sitter-md's 1024-byte scanner serialization buffer (abort in
    // debug, memory corruption in release). The guard must divert it to the
    // whole-doc fallback BEFORE the parser sees it; same for far deeper input.
    on_small_stack(|| {
        for depth in [255usize, 100_000] {
            let doc = format!("# Title\n\n{} deep\n", ">".repeat(depth));
            let chunks = chunk_markdown("gen/deep.md", &doc, 400);
            assert_eq!(chunks.len(), 1, "depth {depth}: one fallback chunk");
            assert_eq!(chunks[0].kind, PREAMBLE_KIND, "depth {depth}: whole-doc fallback");
            assert_eq!(chunks[0].content, doc.trim_end(), "depth {depth}: whole doc");
            assert_eq!(chunks[0].start_line, 1);
        }
    });
}

#[test]
fn markdown_chunker_survives_deeply_indented_lists() {
    // 300 list levels (~90 KB): estimated past the guard, so it must take the
    // deterministic fallback path — and must never reach the crashing scanner.
    on_small_stack(|| {
        let mut doc = String::new();
        for i in 0..300 {
            doc.push_str(&"  ".repeat(i));
            doc.push_str("- item\n");
        }
        let chunks = chunk_markdown("gen/deep.md", &doc, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, PREAMBLE_KIND);
    });
}
