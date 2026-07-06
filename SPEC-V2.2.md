# CCE v2.2 — Workspace Mode (multi-codebase ecosystems)

**Status:** Normative. Evolution spec — deltas only. Base = `SPEC.md` (v1.0),
`DASHBOARD-SPEC.md` (v1.1), `SPEC-V2.md` (v2.0 language packs), `SPEC-V2.1.md`
(v2.1 secret protection) — all implemented. Everything not mentioned here is
unchanged; all existing tests stay green. Single-repo behaviour and its
`conformance.json` are **untouched**.

**How this is built:** fresh sub-agent per repo (Ruby / Rust), working in your
own existing repo (read + refactor — not a clean room). Branch
**`feat/workspace-mode`**. Commit there; **do not push, do not open a PR** — the
orchestrator pushes, opens the PR, merges when green. Do **not** read the
sibling-language repo. This is a **minor release: v2.2.0** (additive).

**Goal.** Let CCE understand an *ecosystem* of related codebases (e.g. a Rails
app + engines + a frontend under one root) as a single searchable whole, while
**each member stays isolated** (its own store). Three pillars, per the design
decisions:
1. **Members are auto-detected** into a reviewable `.cce/workspace.yml`.
2. **Federated storage** — every member is indexed into its *own* `<member>/.cce/`
   exactly as a standalone repo; a workspace is a manifest that federates them.
3. **Level-1 relationships** — every result is tagged with its member, searches
   can be scoped with `--package`, and cross-member **dependency edges** are read
   from manifests (Gemfile/`*.gemspec`/`package.json`).

---

## 1. Concepts & constants

- **Workspace** — a root directory containing ≥1 member, described by
  `<root>/.cce/workspace.yml`.
- **Member** — a codebase inside the workspace, with its own store at
  `<member>/.cce/index.json` (identical to a standalone index of that dir).
- **Federation** — a workspace search/stat/dashboard operates over the *union* of
  in-scope members' stores; nothing is stored centrally except two metadata
  files at the root: `.cce/workspace.yml` and `.cce/workspace-graph.json`.

Constants (normative): `WORKSPACE_FILE = "workspace.yml"`,
`WORKSPACE_GRAPH_FILE = "workspace-graph.json"` (both under the root `.cce/`),
`GRAPH_MAX_BONUS_MEMBERS = 2`, `GRAPH_BONUS_MEMBER_CHUNKS = 2`. Existing retrieval
constants (SPEC §3) are unchanged.

---

## 2. `.cce/workspace.yml` (the manifest)

YAML, at the workspace root. Exact shape:

```yaml
version: 1
name: <workspace name = root dir basename>
members:
  - name: <unique member id>
    path: <path relative to the workspace root, / separators>
    type: rails-app | ruby-engine | ruby-gem | typescript | javascript
    package: <dependency name others use to require it; see §4>
```

`members` is sorted by `path` ascending (deterministic). `name` is unique
(§3). A hand-written manifest is honoured as-is; `cce workspace init` generates
one and the user may edit it.

---

## 3. Member auto-detection — `cce workspace init [<dir>] [--force]`

Walk `<dir>` with the standard ignore rules (SPEC §7.1: skip `.git`, `.cce`,
`node_modules`, `.venv`/`venv`, `__pycache__`, `dist`, `build`, dotdirs). A
directory `D` is a **member** if it contains a **marker** (below). **Members do
not nest:** once `D` is a member, do not descend into it looking for more.

**Markers & type (first match in this precedence sets the type):**
1. `D` contains a `*.gemspec` → Ruby. `type = ruby-engine` if `D` also has
   `app/` **or** `config/routes.rb` **or** a `lib/**/engine.rb`; else
   `type = ruby-gem`.
2. `D` contains `Gemfile` **and** `config/application.rb` → `type = rails-app`.
3. `D` contains `package.json` → `type = typescript` if `D` has `tsconfig.json`,
   else `javascript`.

If a directory matches none, keep descending. If the **root** matches a marker
and has no sub-members, the root is the sole member (degenerate single-repo).

**`package` name** (what other members `require`/`import` to depend on it):
- ruby-engine / ruby-gem: the gem name — from the gemspec's `name` (`s.name = "x"`
  / `spec.name = "x"`); fall back to the `*.gemspec` filename stem.
- typescript / javascript: the `name` field in `package.json`; fall back to the
  member directory basename.
- rails-app: the member directory basename.

**`name`** (member id): the member directory basename. On collision, append
`-2`, `-3`, … in `path`-sorted order (deterministic).

`cce workspace init` writes `<dir>/.cce/workspace.yml` (refusing to overwrite an
existing one unless `--force`) and prints the members found. `cce workspace list
[<dir>]` prints members + the detected cross-member edges (§5).

---

## 4. Federated indexing — `cce index --workspace [<dir>]`

1. Load `<dir>/.cce/workspace.yml` (error with a clear message if absent — tell
   the user to run `cce workspace init`).
2. For **each member**, run the normal single-repo index pipeline on
   `<dir>/<member.path>`, writing to `<dir>/<member.path>/.cce/index.json`. This
   inherits everything: language packs, secret scrubbing (v2.1), the `.cce/`
   store. **A member's store is byte-for-byte identical to indexing that member
   standalone** (isolation preserved — assert this).
3. Build the cross-member dependency graph (§5) and write
   `<dir>/.cce/workspace-graph.json`.
4. Print a per-member summary (files, chunks) + the workspace totals.

Members may also be indexed independently (`cce index <member>`), and a
workspace search will still federate whatever member stores exist.

---

## 5. Cross-member dependency edges (Level 1) — `workspace-graph.json`

For each member, extract the dependency **names** it declares, from whichever
manifests exist in the member root:
- **`*.gemspec`**: every `add_dependency`, `add_runtime_dependency`,
  `add_development_dependency` — capture the first string argument (`"name"`).
- **`Gemfile`**: every `gem "name"` (first string arg). Ignore `gemspec`/`path:`/
  `git:` options.
- **`package.json`**: the keys of `dependencies`, `devDependencies`,
  `peerDependencies`.

An **edge `A → B`** exists when a dependency name declared by member `A` equals
member `B`'s `package` (or `B`'s `name`). Record `via` = `gemspec` | `gemfile` |
`package.json`. Write:

```json
{ "members": ["app","billing","web"],
  "edges": [ {"from":"app","to":"billing","via":"gemfile"} ] }
```

Deterministic: edges sorted by `(from, to, via)`. Extraction is line-regex for
Ruby manifests and JSON-key reading for `package.json` — specify and test each.

---

## 6. Federated search — `cce search "<q>" --workspace [<dir>] [--package a,b] [--top-k N] [--no-graph] [--json]`

**Scope:** all members, or only those named in `--package` (comma-separated;
error on an unknown name).

**Semantics (normative):** a workspace search is **defined to equal** a single
standard retrieval (SPEC §6) run over the **union of the in-scope members'
stored chunks**. Concretely:
1. Load each in-scope member's store; annotate every chunk with its `member`
   name (its `file_path` stays member-relative).
2. Form the combined corpus = the union of those chunks. Run the standard §6
   pipeline **once** over it: query embed → vector search + BM25 (stats computed
   over the union) → RRF → confidence blend → path penalty → per-file diversity
   cap (the diversity key is `(member, file_path)`) → top-K.
3. **Graph expansion (unless `--no-graph`):** the edge set is the union of each
   member's intra-store import graph **plus** the cross-member edges from
   `workspace-graph.json`. For a top result in member `A`, an `A → B` edge lets
   expansion pull up to `GRAPH_BONUS_MEMBER_CHUNKS` chunks from member `B`
   (bounded by `GRAPH_MAX_BONUS_MEMBERS`), scored as in SPEC §6.7.

Because it is the same §6 over the same chunks, a workspace search over members
`{A,B}` returns the same ranked chunks (same order) as a single index built over
`A`+`B` — that equivalence is the correctness anchor.

**Output:** each result carries its `package` (member) + member-relative
`file_path`. Human form: `<score>  <package> · <file_path>:<start>-<end>
(<chunk_type>/<kind>)`. `--json`: array of
`{rank, package, chunk_id, file_path, start_line, end_line, chunk_type, kind,
score}`, `score` as a 6-decimal string; plus a top-level `query_id`.

---

## 7. Workspace stats & dashboard

- `cce stats --workspace [<dir>]` — a per-member table (files, chunks, by-kind)
  plus workspace totals, and the cross-member edges.
- `cce dashboard --workspace [<dir>]` — federate each member's
  `<member>/.cce/metrics.jsonl` **plus the workspace-root `<dir>/.cce/metrics.jsonl`**
  into one dashboard: the existing north-stars (savings, quality) as a workspace
  roll-up **plus a per-package breakdown** (savings & searches per member). The
  aggregator's input for the roll-up is the concatenation of every member's metrics
  events **and the workspace-root log** — the latter is where `cce mcp --workspace`
  records federated (agent) searches (§ MCP), so agent usage appears in
  `totals`/`recent_searches`/`by_source`. The root log is folded in once, guarded
  against a member whose path is the root. The `by_package` section is built from the
  members only: a federated search spans members, so it has no single-package bucket.
  Loopback-only, read-only, self-contained (unchanged posture).

Single-repo `dashboard`/`stats`/`search` (no `--workspace`) are unchanged.

---

## 8. Fixture & cross-language equivalence

Ship `test/fixture/workspace/` — a minimal ecosystem (identical bytes in both
repos):

```
workspace/
  app/            Gemfile (contains: gem "billing"), config/application.rb,
                  app/models/charge.rb   (a class that references Billing)
  engines/
    billing/      billing.gemspec (name = "billing"), lib/billing.rb (a module + method)
  web/            package.json (name = "web"), src/index.ts (a function)
```

Assert (both implementations must reproduce identically):
- **Detection:** `cce workspace init` finds members
  `app` (rails-app), `billing` (ruby-engine, package `billing`), `web`
  (typescript, package `web`), sorted by path, and writes the spec'd
  `workspace.yml`.
- **Edges:** `workspace-graph.json` contains exactly `app → billing (gemfile)`.
- **Isolation:** each member's `<member>/.cce/index.json` is byte-identical to
  indexing that member standalone.
- **Federation:** a scoped search (`--package app,billing`) returns the expected
  chunks from both members, labelled; and equals the union-index result over the
  same two members' chunks (same chunks, same order).
- **Graph hop:** with graph enabled, a top result in `app` expands into
  `billing` via the dependency edge.

Because members' stores are byte-identical across Ruby & Rust (per-member
conformance already holds) and the federation logic is specified exactly, the
workspace `workspace.yml`, `workspace-graph.json`, and search rankings must match
across the two implementations.

---

## 9. CLI summary (additive)

```
cce workspace init [<dir>] [--force]      # detect members → write .cce/workspace.yml
cce workspace list [<dir>]                # members + cross-member edges
cce index      --workspace [<dir>]        # index each member (own store) + build graph
cce search "q" --workspace [<dir>] [--package a,b] [--top-k N] [--no-graph] [--json]
cce stats      --workspace [<dir>]        # per-member + totals
cce dashboard  --workspace [<dir>]        # roll-up + per-package breakdown
```

Absent `--workspace`, every command behaves exactly as today.

---

## 10. TDD, docs, release

- **Test-first.** Cover: detection over the fixture (types, names, package
  names, ordering, collision suffixing, no-nesting), each manifest dependency
  extractor (gemspec/Gemfile/package.json), edge building, per-member
  store-byte-identical-to-standalone, the federation-equals-union equivalence,
  `--package` scoping (incl. unknown-name error), cross-member graph expansion,
  workspace stats/dashboard roll-up + `by_package`, and a re-assert that
  single-repo `conformance.json` is byte-identical.
- **Gates stay green:** Ruby `bundle exec rake test` (coverage ≥ 93%); Rust
  `cargo test` + `clippy --all-targets --all-features -- -D warnings` +
  `fmt --check` (coverage ≥ 92%).
- **Docs:** `README.md` (a "Workspaces / ecosystems" section with a worked
  generic example — a Rails app + one or more engines + a frontend under one
  root; use neutral names like `app` / `billing` / `web`, no real project
  names), a new `docs/workspace.md` (the model, manifest
  format, detection rules, federation semantics, and a "where this would strain"
  note — e.g. huge ecosystems reloading many stores per query; dependency edges
  limited to declared manifests, not Rails route mounting yet), `SECURITY.md`
  (workspace metadata is non-secret; per-member secret scrubbing still applies),
  `CHANGELOG.md` (`2.2.0`, Keep a Changelog), and `docs/DECISIONS.md`. Bump the
  version to **2.2.0** (Ruby: `lib/cce.rb` + `CITATION.cff`; Rust: `Cargo.toml` +
  `CITATION.cff`).

**When done, report:** the manifest/detection/federation/graph built; new test
count + coverage; confirmation the workspace fixture behaves as specified (member
stores byte-identical to standalone, edges correct, federation == union); that
single-repo `conformance.json` is unchanged; all gates green; and the
`feat/workspace-mode` commit hash.
