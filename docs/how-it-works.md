# How CCE works (and why it helps)

CCE (Code Context Engine) indexes a repository so a program — usually an LLM or
coding agent — can **search for the handful of relevant code snippets** instead
of reading whole files. This document explains the benefit and the mechanism.
All diagrams are ASCII on purpose (portable, diff-able, render anywhere).

---

## 1. Why it exists — the benefit

An LLM answering a question about your codebase normally **reads whole files**.
Input tokens are ~85–95% of a coding-assistant bill, so that is expensive and
slow — and it buries the relevant 40 lines inside thousands of irrelevant ones.

```
            WITHOUT CCE                              WITH CCE
     "where are users authenticated?"        "where are users authenticated?"
                 |                                        |
                 v                                        v
      Read whole files:                        cce search -> 4 chunks:
        app/models/user.rb   (900 loc)           auth   . hash_password()
        app/auth/session.rb  (600 loc)           auth   . verify_password()
        app/controllers/...  (1200 loc)          session. create_session()
        config/...    ...                        user   . authenticate()
                 |                                        |
                 v                                        v
         ~45,000 input tokens                      ~800 input tokens
                 |                                        |
                 +-------------> ~94% fewer tokens <------+
          $$$  . slow . noisy context        cheap . fast . focused
                                             -> and often a BETTER answer,
                                                because the model sees signal,
                                                not noise
```

The trade is one tiny search query instead of tens of thousands of tokens of
file dumps. Cheaper, faster, and usually a sharper answer, because the model is
not distracted by code it did not need.

---

## 2. How it works — two phases

### Index time — `cce index <dir>`

```
  repo files          walk + ignore          LanguagePack registry
 +-----------+        (.git, node_modules,   (resolve by extension)
 | *.rb *.ts |  --->  build output, dotdirs  .rb -> ruby   .ts -> typescript
 | *.c  *.py |        skipped; >2MB/binary   .c  -> c      ...no pack -> whole-file
 | .env  (!) |        skipped)                        |
 | Gemfile   |                                        v
 +-----------+                                 tree-sitter AST parse
       |                                        -> one chunk per
       v                                           function / class
 +-----------------+                                    |
 | SECRET SCRUB    |  .env, id_rsa -> skipped           |
 | (secure default)|  AKIA... / sk_live... -> [REDACTED]|
 +-----------------+                                    |
                          +--------------+--------------+---------------+
                          v              v              v               |
                    hash embedder   BM25 index     import graph         |
                    256-d vector    (keywords)     (who imports who)    |
                          +--------------+--------------+---------------+
                                         v
                             on-disk store   .cce/index.json
                             (nothing leaves your machine)
```

### Query time — `cce search "..."`

```
   query --+--> embed  ----------->  vector search  --+   (semantic: cosine)
           +--> tokenize ---------->  BM25 search   --+   (exact keywords)
                                                      v
                              Reciprocal Rank Fusion (RRF)
                              -> confidence score (vector + keyword)
                              -> penalise test/doc paths
                              -> cap 3 chunks per file (diversity)
                              -> +1-hop import-graph expansion
                                                      v
                                     top-K precise chunks
                                                      v
                              +---------------------------+
                              |  LLM / agent gets the 4    |
                              |  functions -- not 4 files  |
                              +---------------------------+
```

### The three key moves

1. **AST chunking, not line-slicing.** tree-sitter cuts each file at real
   boundaries — a function, a class — so a chunk is a *complete idea*, not an
   arbitrary window.
2. **Hybrid retrieval.** Vector search (meaning) *and* BM25 (exact identifiers
   like `getUserById`) fused by Reciprocal Rank Fusion, so both "what does this
   do" and "where is this exact symbol" work.
3. **It serves the snippet, not the file.** That single substitution is the
   entire ~94% saving.

---

## 3. What this build adds around that core

```
  Pluggable packs --- add a language = 1 pack file + `cce packs --validate`
                      (the core names NO language; py/js/ruby/rust/ts/c ship)

  Secret-safe     --- .env & keys never read; tokens redacted before storage
                      (secure by default; --allow-secrets to override)

  Observability   --- every search logs savings + quality -> `cce dashboard`
                      shows "is this actually helping?" trended over time
                        Savings 82.9% ^improving   Quality 0.797 ^improving

  Two engines     --- Ruby & Rust, byte-identical retrieval; Rust ~40x faster/query
```

Why it is genuinely beneficial, concretely:

- **Cost** — you pay for ~800 tokens instead of ~45,000 per question. Over a
  busy day that is the difference between cents and dollars per session.
- **Speed & quality** — less context means faster replies and sharper answers.
- **Private & offline** — everything is local (`.cce/...`), no network by
  default, and secret-safe, so you can point it at a real work repo without
  leaking `.env`.
- **Measurable** — the dashboard tells you whether it is *actually* improving
  your experience, rather than asking you to take "94%" on faith.
- **Yours** — the pluggable packs speak the languages you actually use, and
  adding one is a single validated file.

In one line: **it turns "read the whole file" into "fetch the right function,"
and everything else — packs, secret-scrubbing, the dashboard, two languages —
exists to make that one substitution safe, broad, and provably worth it.**
