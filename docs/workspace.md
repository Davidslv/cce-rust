# Workspace mode (multi-codebase ecosystems)

Workspace mode (SPEC-V2.2) lets CCE reason about an **ecosystem** of related
codebases — for example a Rails app, its engines, and a frontend under one root —
as a single searchable whole, while **each member stays isolated** in its own
store. This document describes the model, the manifest format, the detection
rules, the federation semantics, and where the approach strains.

Absent `--workspace`, every command behaves exactly as in single-repo mode, and
single-repo `conformance.json` is byte-identical.

## The model

Three ideas:

1. **Members are auto-detected** into a reviewable, hand-editable
   `<root>/.cce/workspace.yml`.
2. **Federated storage** — every member is indexed into its *own*
   `<member>/.cce/index.json`, exactly as if it were a standalone repo. A
   workspace is just a manifest that federates them. A member's store is
   **byte-for-byte identical** to indexing that member on its own.
3. **Level-1 relationships** — every result is tagged with its member, searches
   can be scoped with `--package`, and **cross-member dependency edges** are read
   from manifests (`Gemfile` / `*.gemspec` / `package.json`).

Nothing is stored centrally except two metadata files at the root: `.cce/workspace.yml`
and `.cce/workspace-graph.json`.

## The manifest — `.cce/workspace.yml`

```yaml
version: 1
name: <workspace name = root dir basename>
members:
  - name: <unique member id>
    path: <path relative to the root, / separators>
    type: rails-app | ruby-engine | ruby-gem | typescript | javascript
    package: <the dependency name others use to require it>
```

`members` is sorted by `path` ascending (deterministic). `name` is the member
directory basename; on collision, `-2`, `-3`, … are appended in path-sorted order.
`cce workspace init` generates the manifest; a hand-written one is honoured as-is.
CCE emits the file with a byte-deterministic writer, and parses it back with a
YAML reader so edits round-trip.

## Detection rules

`cce workspace init [<dir>] [--force]` walks `<dir>` under the standard ignore
rules (skip `.git`, `.cce`, `node_modules`, `.venv`/`venv`, `__pycache__`,
`dist`, `build`, any dotdir). A directory `D` is a **member** if it contains a
**marker**. **Members do not nest:** once `D` is a member, CCE does not descend
into it. Markers, in precedence order:

1. `D` contains a `*.gemspec` → **Ruby**. `type = ruby-engine` when `D` also has
   `app/` **or** `config/routes.rb` **or** a `lib/**/engine.rb`; otherwise
   `type = ruby-gem`. `package` = the gem name from the gemspec (`s.name` /
   `spec.name`), falling back to the gemspec filename stem.
2. `D` contains `Gemfile` **and** `config/application.rb` → `type = rails-app`.
   `package` = the member directory basename.
3. `D` contains `package.json` → `type = typescript` when `D` has `tsconfig.json`,
   otherwise `javascript`. `package` = the `name` field in `package.json`,
   falling back to the member directory basename.

Children are searched first; if the whole tree yields no member and the **root**
itself matches a marker, the root becomes the sole member (the degenerate
single-repo case).

## Cross-member dependency edges — `.cce/workspace-graph.json`

For each member, CCE extracts the dependency **names** it declares from whichever
manifests exist in the member root:

- **`*.gemspec`** — every `add_dependency`, `add_runtime_dependency`,
  `add_development_dependency` (first string argument).
- **`Gemfile`** — every `gem "name"` (first string argument; `path:`/`git:`
  options are ignored by construction).
- **`package.json`** — the keys of `dependencies`, `devDependencies`,
  `peerDependencies`.

An **edge `A → B`** exists when a dependency name declared by `A` equals member
`B`'s `package` (or `B`'s `name`); `via` records the source manifest
(`gemspec` | `gemfile` | `package.json`). Edges are deduplicated and sorted by
`(from, to, via)`:

```json
{ "members": ["app","billing","web"],
  "edges": [ {"from":"app","to":"billing","via":"gemfile"} ] }
```

## Federation semantics

A **workspace search** is *defined to equal* a single standard retrieval (the
SPEC §6 hybrid pipeline) run over the **union** of the in-scope members' stored
chunks. Concretely:

1. Load each in-scope member's store; annotate every chunk with its `member` name
   (its `file_path` stays member-relative). `--package a,b` restricts the scope to
   the named members — a name resolves against the member **name** or the
   manifest's **`package:` field** (v2.6.4) — and an unknown value errors loudly,
   listing the available members (never a silent empty result). Scoping is also
   the main performance lever: only the named members' stores are loaded, so
   latency tracks their size, not the whole ecosystem's.
2. Form the combined corpus = the union of those chunks and run the §6 pipeline
   **once** over it: query embed → vector search + BM25 (statistics computed over
   the union) → RRF → confidence blend → path penalty → per-file diversity cap
   (diversity key `(member, file_path)`) → top-K. Because it is the same §6 over
   the same chunks, a workspace search over members `{A,B}` returns the same
   ranked chunks, in the same order, as a single index built over `A`+`B` — that
   equivalence is the correctness anchor.
3. **Graph expansion** (unless `--no-graph`): the edge set is the union of each
   member's intra-store import graph **plus** the cross-member edges from
   `workspace-graph.json`. For a top result in member `A`, an `A → B` edge lets
   expansion pull a bounded number of chunks from member `B`.

Each result carries its `package` (member) and a member-relative `file_path`. The
`--json` output is an array of `{rank, package, chunk_id, file_path, start_line,
end_line, chunk_type, kind, score}` (6-decimal score string) plus a top-level
`query_id`.

Implementation note: the union corpus is built with **member-namespaced paths**
(`<member>/<rel>`), so the diversity key is naturally `(member, file_path)` and
BM25 statistics span the union; the namespace is stripped for output. The combined
import graph is the union of each member's *own* intra-store graph (namespaced),
so module-name resolution never introduces spurious cross-member file edges — the
only cross-member links are the declared dependency edges.

## Stats & dashboard

- `cce stats --workspace [<dir>]` — a per-member breakdown (files, chunks,
  by-kind) plus workspace totals and the cross-member edges.
- `cce dashboard --workspace [<dir>]` — federates every member's
  `<member>/.cce/metrics.jsonl` **and the workspace-root `<dir>/.cce/metrics.jsonl`**
  (where `cce mcp --workspace` records agent searches) into one dashboard: the
  existing north-stars as a workspace roll-up **plus a `by_package` section**
  (searches & tokens saved per member). The root log feeds the roll-up
  (`totals`/`recent_searches`/`by_source`) so agent usage shows up; those federated
  searches span members, so they stay **out of `by_package`**, which remains
  per-member. Loopback-only, read-only, and self-contained, exactly as the
  single-repo dashboard.

## Where this would strain

- **Many members, reloaded per query (CLI).** A CLI federated search loads every
  in-scope member's store and unions their chunks on each invocation. For a huge
  ecosystem (dozens of large members) that is a lot of JSON to read and hold in
  memory per query. Two mitigations since v2.6: scope with `--package` (only the
  named members load), and use the long-lived MCP server — it **caches the
  assembled union per scope** across calls, invalidated by an `mtime`+length
  fingerprint of the member stores, so a warm agent search is as fast as a
  single-repo one (see [`mcp.md`](mcp.md)). A shared on-disk vector store would
  still scale better than reload-and-union for the cold CLI path.
- **Edges are declared, not behavioural.** Cross-member edges come only from
  declared manifest dependencies. A Rails app that *mounts* an engine's routes,
  or reaches into it through runtime constant lookup rather than a `Gemfile`
  line, produces no edge yet. Level-1 relationships are deliberately manifest-only.
- **Detection is marker-based.** The precedence rules cover the common Ruby/JS
  layouts; polyglot members, unconventional gemspec/`package.json` placement, or
  a member that is itself a nested workspace are out of scope. The manifest is
  hand-editable precisely so these cases can be corrected by review.
- **Secret protection is per member.** Each member is scrubbed exactly as a
  standalone index (SPEC-V2.1). The workspace metadata files are non-secret; see
  [`SECURITY.md`](../SECURITY.md).
