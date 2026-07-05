# Code Context Engine — Clean-Room Specification (v1.0)

**Status:** Normative. This document is the *single source of truth* for your
implementation. Build exactly what it describes.

**Date context:** It is July 2026. You may install any packages, toolchains, or
grammars you need from the public internet. Pin versions where you can.

---

## 0. Clean-room rules (READ FIRST)

1. You are implementing from **this specification only**. There exists a
   reference implementation in another language. **You must NOT read it, search
   for it, clone it, or look at it in any form.** If you find a directory named
   `code-context-engine` or any Python source implementing these ideas, do not
   open it. Your work must derive solely from this document.
2. Work entirely inside your assigned working directory. Do not read files
   outside it except package registries/docs you install from.
3. This `SPEC.md` file stays inside your working directory and is part of your
   deliverable. Do not delete or move it.
4. If the spec is ambiguous, pick the simplest reasonable interpretation,
   **write down the decision** in `docs/DECISIONS.md`, and continue. Do not
   invent features not described here.

---

## 1. What you are building

A **Code Context Engine (CCE)**: a local tool that indexes a source-code
repository so that a program can *search* for the most relevant code snippets
instead of reading whole files. The core loop:

```
index a directory
  → walk files → AST-chunk each file into functions/classes
  → embed each chunk into a vector
  → store vectors + a keyword index + a small import graph on disk
search a query
  → hybrid retrieve: vector similarity + BM25 keyword + Reciprocal Rank Fusion
  → confidence-score, penalize test/doc paths, enforce file diversity
  → optionally expand via the import graph
  → return the top-K ranked chunks
```

You will deliver a **complete, working command-line program**, a **test suite
built test-first (TDD)**, **benchmarks**, and **documentation**.

### 1.1 In scope (you MUST build all of this)

- File walking with ignore rules.
- AST-aware chunking via **tree-sitter** for **Python and JavaScript** (minimum).
  A whole-file fallback chunk for every other/unparseable file.
- Import extraction (for the graph).
- A **deterministic hashing embedder** (exact algorithm in §5) — the default.
- An **optional Ollama HTTP embedder** (§11) selectable by config/flag.
- On-disk persistence; `search` and `stats` must work in a *fresh process*
  after `index` has run.
- Hybrid retrieval: cosine vector search + BM25 + RRF + confidence blend +
  path penalty + per-file diversity cap + import-graph expansion (§6).
- A CLI with commands: `index`, `search`, `stats`, `bench`, `conformance` (§9).
- Benchmarks against a pinned real repo (§10).
- The conformance harness producing `conformance.json` (§8).

### 1.2 Out of scope (do NOT build)

Cross-session memory / session capture, MCP server, editor auto-config,
dashboards, savings-dollar pricing, git-hook installation, file watching,
approximate-nearest-neighbor indexes. **Vector search is exact brute-force
cosine** (corpora are small). Keep it simple and correct.

---

## 2. Deliverables & required repository layout

Deliver a runnable project. Suggested layout (adapt naming to your language's
conventions, but keep the separation of concerns):

```
<workdir>/
  SPEC.md                     # this file — keep it here
  README.md                   # what it is, how to build/run, examples
  docs/
    ARCHITECTURE.md           # module map + data flow
    DECISIONS.md              # every ambiguity you resolved + why
    TDD.md                    # your red-green-refactor log + final coverage
    BENCHMARKS.md             # generated benchmark report (§10)
  src/ (or lib/)              # implementation, one concern per file
    tokenizer.*               # shared tokenizer (§4)
    chunker.*                 # tree-sitter chunking + import extraction (§4.2)
    embedder.*                # hash embedder (default) + ollama (optional) (§5)
    vector_store.*            # store vectors, brute-force cosine search (§6.2)
    keyword_store.*           # BM25 index + search (§6.3)
    graph_store.*             # import graph, neighbor lookup (§6.7)
    retriever.*               # the hybrid pipeline (§6)
    store.*                   # persistence (open/save/load) (§7)
    config.*                  # constants + config loading (§3)
    cli.*                     # command-line entry points (§9)
    bench.*                   # benchmark runner (§10)
  test/ (or tests/)           # tests, written FIRST (§ TDD)
```

### 2.1 Per-file documentation requirement (MANDATORY)

**Every source file** must begin with a documentation header (a top-of-file
comment or docstring) answering three things explicitly:

- **Why this file exists** (the problem it solves).
- **What it is / does** (its role in the system).
- **Responsibilities** (a short bullet list of what it owns — and, where useful,
  what it deliberately does *not* own).

A file with no such header is an incomplete deliverable. Keep comment density
consistent with the surrounding code; document the *why*, not the obvious *how*.

---

## 3. Configuration & constants

These constants are **normative**. Use exactly these values; both language
implementations must agree.

| Name | Value | Meaning |
|---|---|---|
| `EMBED_DIM` | `256` | hashing-embedder vector dimension |
| `CHARS_PER_TOKEN` | `4` | token-count estimate = floor(bytes/4), min 1 |
| `RRF_K` | `60` | RRF constant |
| `CONFIDENCE_WEIGHT` | `0.5` | weight of confidence vs normalized RRF in final blend |
| `FTS_BOOST_CODE_LOOKUP` | `1.5` | BM25 weight multiplier when intent is CODE_LOOKUP |
| `MAX_CHUNKS_PER_FILE` | `3` | per-file diversity cap in results |
| `BM25_K1` | `1.2` | BM25 term-frequency saturation |
| `BM25_B` | `0.75` | BM25 length normalization |
| `CANDIDATE_MULTIPLIER` | `3` | fetch top_k × 3 candidates from each retriever |
| `W_VECTOR` | `0.5` | confidence: vector weight |
| `W_KEYWORD` | `0.4` | confidence: keyword weight |
| `W_RECENCY` | `0.1` | confidence: recency weight (recency=0 in deterministic mode) |
| `PATH_PENALTY` | `0.8` | multiplier applied to test/doc-path chunks |
| `PATH_PENALTY_MARKERS` | `["tests/", "test_", "docs/", "spec", "plan"]` | substrings that trigger the penalty |
| `GRAPH_MAX_BONUS_FILES` | `2` | related files pulled in graph expansion |
| `GRAPH_BONUS_CHUNK_SCALE` | `0.85` | score scale for graph-expansion chunks |
| `DEFAULT_TOP_K` | `10` | default results returned |

Config may be loaded from a file (your choice of format) but must default to the
values above. The embedder backend is selected by config/flag:
`embedder = "hash"` (default) or `embedder = "ollama"`.

---

## 4. Text processing

### 4.1 Tokenizer (shared, exact)

One tokenizer is used by the embedder, BM25, and keyword matching. It operates
on the **raw UTF-8 bytes** of the text:

1. A **token** is a maximal run of bytes each in the ASCII set
   `[A-Za-z0-9_]` (letters, digits, underscore). Every other byte (including all
   non-ASCII bytes, whitespace, punctuation) is a separator.
2. Lowercase each token by mapping ASCII bytes `0x41..0x5A` (`A`–`Z`) to
   `0x61..0x7A` (`a`–`z`). Leave all other bytes unchanged.
3. Emit tokens in left-to-right order. Do not deduplicate here.

**Anchor tests (must pass exactly):**
- `tokenize("hashPassword(user_id)")` → `["hashpassword", "user_id"]`
- `tokenize("SELECT * FROM users;")` → `["select", "from", "users"]`
- `tokenize("")` → `[]`
- camelCase is **not** split (`getUserById` → `getuserbyid`).

### 4.2 Chunking (tree-sitter)

For each file, decide its language by extension:
`.py`→python, `.js`/`.jsx`/`.mjs`/`.cjs`→javascript. (You may add more; Python
and JavaScript are the minimum.) Any other extension, or a parse failure, uses
the **fallback**.

**Parsed files:** Parse with tree-sitter. Walk the entire tree (depth-first,
visiting children in order). For **every** node whose type is a *function* or a
*class* node (including nested ones — a method inside a class produces BOTH a
class chunk and a method chunk), emit a chunk:

- Python function node types: `function_definition`.
- Python class node types: `class_definition`.
- JavaScript function node types: `function_declaration`, `method_definition`,
  `arrow_function`, `function_expression`.
- JavaScript class node types: `class_declaration`.

Chunk fields:
- `content` = the exact source bytes from `node.start_byte` to `node.end_byte`.
- `start_line` = node start row + 1 (1-based). `end_line` = node end row + 1.
- `chunk_type` = `"function"` or `"class"`.
- `language` = the resolved language.
- `file_path` = path **relative to the indexed root**, using `/` separators.

If a parsed file yields **zero** function/class chunks, emit one fallback chunk
(below) instead.

**Fallback chunk (unparsed/other/empty-of-symbols):** a single chunk with
`chunk_type = "module"`, `content` = whole file, `start_line = 1`,
`end_line = number of lines`, `language` = resolved language or `"plaintext"`.

**Import extraction (parsed files only):** collect imported module names for the
graph. Python: from `import_statement` and `import_from_statement`, take the
first dotted component of the module (e.g. `import os.path` → `os`,
`from pkg.sub import x` → `pkg`). JavaScript: from `import_statement`, take the
string module specifier's first path segment (`react` from `"react"`,
`"./auth"` → `auth`). Deduplicate, preserve first-seen order. Import extraction
failing must never crash indexing — return `[]`.

### 4.3 Chunk ID (exact, cross-language identical)

```
prefix_bytes = first 100 bytes of UTF8(content)      # a raw byte slice; may cut a codepoint
id_input     = UTF8( f"{file_path}:{start_line}:{end_line}:" ) ++ prefix_bytes
chunk_id     = lowercase_hex( SHA256( id_input ) )[0..16]     # first 16 hex chars
```

`++` is byte concatenation. `file_path` uses `/` separators. Two implementations
given identical inputs MUST produce identical `chunk_id`s.

### 4.4 Token count

`token_count(content) = max(1, floor( byte_length(UTF8(content)) / CHARS_PER_TOKEN ))`.

---

## 5. Embedding

### 5.1 Hashing embedder (DEFAULT — exact algorithm)

Deterministic, no model, identical across languages. Produces an `EMBED_DIM`
(256) vector of `f64`/double.

**FNV-1a 64-bit hash** (operate on token bytes):
```
offset_basis = 0xcbf29ce484222325   (14695981039346656037)
prime        = 0x00000100000001b3   (1099511628211)
hash = offset_basis
for each byte b in token:
    hash = hash XOR b               # XOR low 8 bits
    hash = (hash * prime) mod 2^64  # wrapping 64-bit multiply
```
**Anchor tests (published FNV-1a-64 vectors — must pass):**
- `fnv1a64("")`     = `0xcbf29ce484222325`
- `fnv1a64("a")`    = `0xaf63dc4c8601ec8c`
- `fnv1a64("foobar")` = `0x85944171f73967e8`

**Embed(text) → vector[256]:**
```
v = [0.0; 256]
for each token t in tokenize(text):        # in order
    h = fnv1a64(t)                          # unsigned 64-bit
    bucket = h mod 256
    sign   = -1.0 if ((h >> 63) & 1) == 1 else +1.0
    v[bucket] = v[bucket] + sign            # accumulate term frequency (signed)
# L2 normalize
norm = sqrt( sum(v[i]^2) )
if norm > 0: for i: v[i] = v[i] / norm
return v                                    # all-zeros if text has no tokens
```
Accumulate strictly in token order. Use IEEE-754 doubles throughout.

### 5.2 Cosine similarity

Both vectors are L2-normalized, so **cosine = dot product**, summed over indices
`0..256` in order:
```
cosine(a, b) = sum_{i=0}^{255} a[i]*b[i]
distance     = 1 - cosine                    # in [0, 2] for normalized vectors
```
**Anchor test:** `cosine([0.6,0.8,...0], [1,0,...0]) = 0.6` (pad to 256 with 0).

### 5.3 Determinism & rounding

Cross-language floating-point may differ in the last ULP. Wherever scores are
compared, sorted, or emitted, **round to 6 decimal places, round-half-away-from-
zero**, and break ties by `chunk_id` ascending (lexicographic on the hex string).
This makes rankings reproducible across languages.

---

## 6. Retrieval pipeline

Input: `query` string, `top_k` (default 10), `graph_enabled` (default true;
conformance runs with it **false**). Output: an ordered list of result chunks,
each carrying a final `score`.

### 6.1 Query intent classification

Classify the query to set the BM25 boost. `intent = CODE_LOOKUP` if the
lowercased query matches ANY of:
- contains a whole word in `{function, class, method, def}`, OR
- contains a file-extension token matching `\.(py|js|jsx|ts|go|rb|rs|java)\b`, OR
- matches `where is `, `find .* function`, or `.* defined`.

Otherwise `intent = GENERAL`. (These four intents exist conceptually; you only
need CODE_LOOKUP vs GENERAL for scoring.)
`fts_weight = FTS_BOOST_CODE_LOOKUP (1.5)` if CODE_LOOKUP else `1.0`.

### 6.2 Vector candidates

Embed the query. Compute cosine to every stored chunk. Sort by cosine descending
(tie-break `chunk_id` asc). Take the top `max(top_k * CANDIDATE_MULTIPLIER, 1)`.
Record each candidate's 0-based rank as `vrank[id]` and keep its `distance`.

### 6.3 BM25 candidates

Tokenize the query; use the **set of unique** query tokens `Q`.
Corpus = all stored chunks (documents). For document `D`:
```
|D|    = number of tokens in D (from tokenize(D.content))
avgdl  = mean |D| over all documents
N      = number of documents
n_q    = number of documents containing token q
idf(q) = ln( 1 + (N - n_q + 0.5) / (n_q + 0.5) )        # non-negative (Lucene form)
f(q,D) = frequency of q in D
score(D,Q) = sum_{q in Q, n_q>0} idf(q) * ( f(q,D)*(BM25_K1+1) )
                                   / ( f(q,D) + BM25_K1*(1 - BM25_B + BM25_B*|D|/avgdl) )
```
Rank documents by `score` descending (tie-break `chunk_id` asc); take top
`top_k * CANDIDATE_MULTIPLIER`. Record 0-based `frank[id]`. Documents scoring 0
(no query term present) are not BM25 candidates.

**Worked anchor (must reproduce to ±1e-4):** Two docs, tokenized
`D1=["user","login","user"]` (|D1|=3), `D2=["payment","process"]` (|D2|=2),
`avgdl=2.5`, `N=2`. Query `["user"]`: `n_q=1`, `idf=ln(2)=0.693147`,
`score(D1)=0.693147 * (2*2.2)/(2 + 1.2*(1-0.75+0.75*3/2.5)) = 0.902273`,
`score(D2)=0`.

### 6.4 RRF fusion

Candidate id set = union of vector and BM25 candidate ids.
```
rrf(id) = ( 1/(RRF_K + vrank[id]) if id has a vector rank else 0 )
        + fts_weight * ( 1/(RRF_K + frank[id]) if id has a BM25 rank else 0 )
max_rrf = max over candidates of rrf(id)   (0 if empty)
norm_rrf(id) = rrf(id)/max_rrf  (0 if max_rrf==0)
```
**Anchor:** id at vrank 0 and frank 2, fts_weight 1.0 →
`rrf = 1/60 + 1/62 = 0.032796`.

### 6.5 Confidence score

For each candidate chunk:
```
vector_distance     = its cosine distance (1 - cosine); if the chunk was BM25-only,
                      compute cosine(query, chunk) now.
normalized_distance = clamp(vector_distance / 2, 0, 1)
vector_score        = 1 - normalized_distance                       # in [0,1]
keyword_distance    = 0 if (any unique query token is a substring of the
                      lowercased chunk content) OR (any query file-hint is a
                      substring of file_path) else 2
keyword_score       = max(0, 1 - keyword_distance/5)                # 0→1.0, 2→0.6
recency_score       = 0.0    # deterministic mode. (Real mode may use file mtime.)
confidence = W_VECTOR*vector_score + W_KEYWORD*keyword_score + W_RECENCY*recency_score
```
"file-hint" = any token in the query that contains a `.` and looks like a
filename, or a path fragment; if you don't extract hints, treat as none (the
substring rule above still applies).

### 6.6 Final blend, penalty, diversity

```
final = CONFIDENCE_WEIGHT*confidence + (1-CONFIDENCE_WEIGHT)*norm_rrf
if file_path (lowercased) contains any PATH_PENALTY_MARKERS: final *= PATH_PENALTY
chunk.score = final
```
Sort all candidates by `score` desc (tie-break `chunk_id` asc). Then apply the
**diversity cap**: iterate in sorted order, keep a chunk only if fewer than
`MAX_CHUNKS_PER_FILE` (3) chunks from the same `file_path` are already kept; stop
once `top_k` are kept.

### 6.7 Import-graph expansion (only when `graph_enabled`)

Build/consult an import graph: node per file; a directed edge `A → B` when file
`A` imports a module that resolves to corpus file `B` (resolve by matching the
module name to a file whose path stem — filename without extension — equals the
module, or whose path ends with `<module>.py`/`<module>.js`). Expansion:

1. Take the file paths of the top 3 ranked results.
2. Find neighbor files (either edge direction) not already in the result set.
3. For up to `GRAPH_MAX_BONUS_FILES` (2) such neighbor files, pull up to 2 chunks
   from that file ranked by cosine to the query.
4. For each, `score = max(0, cosine) * GRAPH_BONUS_CHUNK_SCALE (0.85)`. Skip
   duplicates (same file_path + line span). Append these bonus chunks **after**
   the main results.

Graph expansion is excluded from conformance output (§8 runs with it disabled).
Test it with its own unit tests (edge extraction + neighbor lookup + a small
end-to-end case).

---

## 7. Persistence

`index` must write an on-disk store; `search`/`stats`/`conformance` must reopen
it in a **fresh process** and work correctly. Format is your choice (SQLite,
serialized files — anything). The store must persist enough to reconstruct: all
chunks (id, file_path, start/end line, type, language, content, token_count,
embedding vector) and the import graph. BM25 statistics may be stored or
recomputed on load. Re-indexing the same directory must be **idempotent** (chunk
IDs are deterministic; replace prior data for changed/removed files).

Store location: default to a hidden dir under the indexed root (e.g.
`.cce/`), or a path given by a flag. Do not index your own store directory.

### 7.1 File walking & ignore rules

Recursively walk the indexed root. Skip: `.git/`, `.cce/` (your store),
`node_modules/`, `.venv`/`venv/`, `__pycache__/`, `dist/`, `build/`, and any
dotdir. Skip binary/oversized files (skip files > 2 MB, and files that aren't
valid UTF-8). This ignore behavior needs its own test.

---

## 8. Conformance harness (cross-implementation equivalence)

Both implementations must produce **identical** results on a fixed fixture. You
must include the fixture below verbatim and a `conformance` command that emits
`conformance.json`.

### 8.1 Fixture corpus (create these files exactly, under `test/fixture/`)

**`auth.py`:**
```python
import hashlib

def hash_password(password):
    return hashlib.sha256(password.encode()).hexdigest()

def verify_password(password, digest):
    return hash_password(password) == digest

class SessionManager:
    def create_session(self, user_id):
        return {"user": user_id}
```

**`payments.py`:**
```python
from auth import verify_password

def process_payment(amount, currency):
    return {"amount": amount, "currency": currency, "status": "ok"}

def refund_payment(payment_id):
    return {"payment_id": payment_id, "status": "refunded"}
```

**`README.md`:**
```markdown
# Demo
Payment and authentication utilities.
```

**Structural expectations (hand-derivable — assert these in tests):**
- `auth.py` → **4** chunks: `hash_password` (function), `verify_password`
  (function), `SessionManager` (class, spans the whole class), `create_session`
  (function/method, nested inside the class). The class chunk and method chunk
  overlap — that is correct.
- `payments.py` → **2** chunks: `process_payment`, `refund_payment`.
- `README.md` → **1** chunk: `module` fallback (whole file).
- Total: **7** chunks.
- `payments.py` has an IMPORTS edge to `auth.py` (via `from auth import ...`).

### 8.2 Conformance queries

Run each with `top_k = 5` and **graph disabled**:
`Q1 = "hash password"`, `Q2 = "process payment amount"`,
`Q3 = "create session user"`.

Top-1 structural expectations (assert in tests):
- Q1 top-1 chunk is from `auth.py` and is the `hash_password` function.
- Q2 top-1 chunk is `process_payment` in `payments.py`.
- Q3 top-1 chunk is from `auth.py` (`create_session` or `SessionManager`).

### 8.3 `conformance.json` format (exact)

```json
{
  "spec_version": "1.0",
  "impl_language": "ruby",           // or "rust"
  "chunks": [                        // sorted by (file_path, start_line, chunk_id)
    {"file_path":"auth.py","start_line":3,"end_line":4,
     "chunk_type":"function","chunk_id":"<16 hex>","token_count":<int>}
    // ...all 7
  ],
  "queries": [
    {"query":"hash password","top_k":5,"graph_enabled":false,
     "results":[
       {"rank":1,"chunk_id":"<16 hex>","file_path":"auth.py","score":"0.123456"}
       // ...up to 5, score as fixed 6-decimal string
     ]}
    // Q2, Q3
  ]
}
```

- `chunks` sorted by `(file_path, start_line, chunk_id)`.
- `score` is the final blended score as a **fixed 6-decimal string**
  (round-half-away-from-zero).
- The orchestrator will diff the two implementations' `conformance.json`
  **ignoring only the `impl_language` field**. The `chunks` array and every
  `queries[*].results` array must match byte-for-byte. Treat this as a hard
  acceptance gate: if you cannot make it deterministic, you have a bug.

---

## 9. CLI

Provide a single executable (name it `cce`). Commands:

- `cce index <dir> [--store <path>] [--embedder hash|ollama]`
  Walk, chunk, embed, persist. Print a summary: files indexed, files skipped,
  total chunks, elapsed time.
- `cce search <query> [--dir <dir>|--store <path>] [--top-k N] [--no-graph]
  [--json]`
  Load the store, run retrieval, print results. Human format: rank, score,
  `file_path:start_line-end_line`, chunk_type, and a short snippet. `--json`:
  array of `{rank, chunk_id, file_path, start_line, end_line, chunk_type,
  score}` (score as 6-decimal string).
- `cce stats [--store <path>]`
  Print: chunk count, file count, chunks-per-language breakdown, avg chunk
  token_count, store size on disk.
- `cce bench <repo-dir> [--queries <file>] [--store <path>]`
  Run the benchmark (§10) and write `docs/BENCHMARKS.md`.
- `cce conformance <fixture-dir> [-o conformance.json]`
  Index the fixture, run the three conformance queries with graph disabled,
  emit `conformance.json` (§8.3).

Exit non-zero on error with a clear message. Invalid/empty inputs must not crash
(return empty results / friendly errors) — cover these with tests.

---

## 10. Benchmarks

`cce bench` measures the pipeline on a **pinned real repository** using the
default hashing embedder.

### 10.1 Pinned corpus

Clone shallowly at a pinned tag (record the exact commit you used in the report):
- **Primary:** `https://github.com/pallets/flask` at tag `3.0.3`.
- If unavailable, fall back to `https://github.com/psf/requests` at tag
  `v2.32.3` and note the substitution.

Index the repo's Python sources.

### 10.2 Labeled queries (recall set)

Provide these as the default query set (query → a path substring that a correct
top-K should surface). Recall counts a query as hit if any of the top-K results'
`file_path` contains the expected substring.

```
"where are blueprints registered"                   -> blueprints
"application factory and app configuration"          -> app
"load configuration from environment or file"        -> config
"session cookie serialization"                        -> sessions
"url routing and rule mapping"                        -> "" (any of app/blueprints)  # see note
"render a template with context"                      -> templating
"command line interface entry point"                 -> cli
"json encoder and decoder for responses"              -> json
"request and response context management"             -> ctx
"send a file as a response"                           -> helpers
```
(If a target file doesn't exist in your pinned version, drop that query and note
it.) You may extend the set; keep the originals.

### 10.3 Metrics (report all)

- **Index:** total files, total chunks, wall-clock seconds, chunks/second.
- **Query latency:** over the labeled queries (repeat each ≥5×), report p50 and
  p95 milliseconds.
- **Recall@5** and **Recall@10** over the labeled set (fraction of queries hit).
- **Token savings:** for each query, `baseline = sum of token_count over the
  full files touched by the top-10 results` (i.e., for each distinct result
  file, the whole file's token count), `served = sum of token_count of the top-10
  result chunks`. Report the mean `1 - served/baseline` across queries as a
  percentage.
- **Environment:** language + version, machine, embedder used, corpus commit.

### 10.4 Report

Write `docs/BENCHMARKS.md` with a table of the above and one paragraph
interpreting the results. Also keep any micro-benchmark harness you used
(`criterion`, `benchmark-ips`, etc.) but the headline numbers come from
`cce bench`.

Note: with the hashing embedder, retrieval is essentially lexical, so recall
numbers reflect keyword overlap — that is expected and fine. Both languages
should get **identical** recall and token-savings numbers on the same corpus
(another cross-check). Latency will differ by language.

---

## 11. Optional Ollama embedder

Selectable via `--embedder ollama` / `embedder = "ollama"`. Talks to a local
Ollama server (default `http://localhost:11434`), model `nomic-embed-text`, via
`POST /api/embed` with body `{"model": <m>, "input": [<text>, ...]}`, reading
`.embeddings`. Truncate each input to ~2000 chars; skip empty inputs. If Ollama
is unreachable, print a clear message and fall back to (or instruct the user to
use) the hash embedder. This backend is **not** covered by conformance (its
vectors are model-dependent). Keep the `Embedder` interface clean so the two
backends are interchangeable. A minimal integration test may be skipped
gracefully when no server is present.

---

## 12. TDD process (MANDATORY)

You must build test-first. For each unit of behavior:

1. **Red:** write a failing test that pins the spec'd behavior.
2. **Green:** write the minimum code to pass it.
3. **Refactor:** clean up with tests green.

Requirements:
- Use your language's standard test framework.
- Cover, at minimum: the tokenizer anchors (§4.1), FNV-1a anchors (§5.1), cosine
  anchor (§5.2), chunk-ID determinism (§4.3), the BM25 worked example (§6.3), the
  RRF anchor (§6.4), chunking of the fixture (§8.1 structural counts), ignore
  rules (§7.1), the three conformance queries' top-1 (§8.2), persistence
  round-trip (index then search in a fresh process), graph edge extraction and
  expansion (§6.7), and CLI happy-path + an invalid-input path.
- Record your cycle in `docs/TDD.md`: the order you built things, notable
  red→green moments, and your final test count + coverage percentage (use a
  coverage tool). Run the full suite at the end and paste the passing output.

Aim for meaningful coverage (target ≥85% of non-CLI logic). Tests must be
deterministic and hermetic (no network in the default suite).

---

## 13. Acceptance checklist (self-verify before you finish)

- [ ] Every source file has a why/what/responsibilities header.
- [ ] All anchor tests pass (tokenizer, FNV, cosine, chunk-ID, BM25, RRF).
- [ ] Fixture chunking yields the exact 7 chunks described.
- [ ] `cce index` then `cce search` works across separate process runs.
- [ ] `cce conformance test/fixture -o conformance.json` produces the spec'd
      format, deterministically (run it twice — identical output).
- [ ] `cce bench` produces `docs/BENCHMARKS.md` with all metrics.
- [ ] `README.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `docs/TDD.md`
      exist and are real.
- [ ] Full test suite passes; output pasted into `docs/TDD.md`.
- [ ] You never consulted any reference implementation.

Deliver a working program. Correctness and determinism first, then benchmarks,
then polish.
```
