//! # workspace — member auto-detection, the manifest, and cross-member edges
//!
//! **Why this file exists:** SPEC-V2.2 turns a directory of related codebases
//! into an *ecosystem* CCE can reason about as a whole. That requires three
//! deterministic, cross-language-identical artifacts: a reviewable member list
//! (`.cce/workspace.yml`), the dependency edges between members
//! (`.cce/workspace-graph.json`), and the detection rules that produce them.
//!
//! **What it is / does:** Owns the `Member`/`Manifest` types, the §3 marker-based
//! auto-detection walk (members never nest), a canonical byte-deterministic YAML
//! writer + a tolerant reader (hand-written manifests are honoured), the §5
//! per-manifest dependency extractors (gemspec / Gemfile / package.json), and the
//! edge builder that links `A -> B` when `A` declares a dependency on `B`'s
//! package (or name).
//!
//! **Responsibilities:**
//! - Own detection, the manifest shape, dependency extraction, and edge building.
//! - Guarantee determinism: members sorted by path, edges sorted by (from,to,via).
//! - It does NOT index, search, or federate — `federation` consumes these.

use crate::config::{WORKSPACE_FILE, WORKSPACE_GRAPH_FILE};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A member's detected kind (SPEC-V2.2 §2/§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberType {
    RailsApp,
    RubyEngine,
    RubyGem,
    Typescript,
    Javascript,
    /// A member with no source checkout — only a pulled `.cce/` store (consumer
    /// mode, issue #54). Never produced by detection (there is no source to
    /// classify); synthesized manifests (`cce sync pull --all`) write it, and
    /// hand-written manifests may use it.
    StoreOnly,
}

impl MemberType {
    /// The manifest string form.
    pub fn as_str(self) -> &'static str {
        match self {
            MemberType::RailsApp => "rails-app",
            MemberType::RubyEngine => "ruby-engine",
            MemberType::RubyGem => "ruby-gem",
            MemberType::Typescript => "typescript",
            MemberType::Javascript => "javascript",
            MemberType::StoreOnly => "store-only",
        }
    }

    /// Parse the manifest string form; unknown strings are `None`.
    pub fn parse(s: &str) -> Option<MemberType> {
        match s {
            "rails-app" => Some(MemberType::RailsApp),
            "ruby-engine" => Some(MemberType::RubyEngine),
            "ruby-gem" => Some(MemberType::RubyGem),
            "typescript" => Some(MemberType::Typescript),
            "javascript" => Some(MemberType::Javascript),
            "store-only" => Some(MemberType::StoreOnly),
            _ => None,
        }
    }
}

/// One workspace member (SPEC-V2.2 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Unique member id (directory basename, collision-suffixed).
    pub name: String,
    /// Path relative to the workspace root, `/`-separated.
    pub path: String,
    /// Detected member type.
    pub member_type: MemberType,
    /// The dependency name other members use to require it (SPEC-V2.2 §3).
    pub package: String,
}

/// The full workspace manifest (SPEC-V2.2 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Manifest schema version (always 1 in v2.2).
    pub version: u32,
    /// Workspace name = root directory basename.
    pub name: String,
    /// Members, sorted by `path` ascending.
    pub members: Vec<Member>,
}

// --- Detection (SPEC-V2.2 §3) ---

/// Directory names pruned during the detection walk (mirrors walker §7.1).
const IGNORE_DIRS: [&str; 8] =
    [".git", ".cce", "node_modules", ".venv", "venv", "__pycache__", "dist", "build"];

/// Should this directory be skipped by the detection walk? Ignore-listed names
/// and any dotdir are pruned (the root, passed explicitly, is never pruned here).
fn is_ignored_dirname(name: &str) -> bool {
    IGNORE_DIRS.contains(&name) || name.starts_with('.')
}

/// Immediate child directories of `dir`, filtered by the ignore rules, sorted by
/// name for deterministic traversal.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ignored_dirname(&name) {
                continue;
            }
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs
}

/// A detected member before name-collision resolution.
struct Detected {
    path: String,
    member_type: MemberType,
    package: String,
    basename: String,
}

/// True if any `*.gemspec` file sits directly in `dir`.
fn find_gemspec(dir: &Path) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("gemspec") {
                matches.push(path);
            }
        }
    }
    matches.sort();
    matches.into_iter().next()
}

/// True if `dir` has a `lib/**/engine.rb` anywhere under `lib/`.
fn has_engine_rb(dir: &Path) -> bool {
    let lib = dir.join("lib");
    if !lib.is_dir() {
        return false;
    }
    for entry in walkdir::WalkDir::new(&lib).into_iter().flatten() {
        if entry.file_type().is_file() && entry.file_name().to_string_lossy() == "engine.rb" {
            return true;
        }
    }
    false
}

/// Classify a directory by the §3 markers, returning its type + package if it is
/// a member, else `None`. Precedence: gemspec, then Gemfile+application.rb, then
/// package.json.
fn classify(dir: &Path) -> Option<(MemberType, String)> {
    let basename = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();

    // Rule 1: a gemspec makes it Ruby (engine vs gem).
    if let Some(gemspec) = find_gemspec(dir) {
        let is_engine = dir.join("app").is_dir()
            || dir.join("config").join("routes.rb").is_file()
            || has_engine_rb(dir);
        let member_type = if is_engine {
            MemberType::RubyEngine
        } else {
            MemberType::RubyGem
        };
        let package = gem_name_from_gemspec(&gemspec).unwrap_or_else(|| gemspec_stem(&gemspec));
        return Some((member_type, package));
    }

    // Rule 2: Gemfile + config/application.rb => rails-app.
    if dir.join("Gemfile").is_file() && dir.join("config").join("application.rb").is_file() {
        return Some((MemberType::RailsApp, basename));
    }

    // Rule 3: package.json => typescript (if tsconfig.json) else javascript.
    let package_json = dir.join("package.json");
    if package_json.is_file() {
        let member_type = if dir.join("tsconfig.json").is_file() {
            MemberType::Typescript
        } else {
            MemberType::Javascript
        };
        let package = package_json_name(&package_json).unwrap_or(basename);
        return Some((member_type, package));
    }

    None
}

/// The `name = "..."` value from a gemspec (`s.name`/`spec.name`), if present.
fn gem_name_from_gemspec(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let t = line.trim();
        // Match `<ident>.name = "value"` (single or double quotes).
        if let Some(rest) = t.split_once(".name") {
            let rhs = rest.1.trim_start();
            if let Some(after_eq) = rhs.strip_prefix('=') {
                if let Some(name) = first_string_literal(after_eq) {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// The gemspec filename stem (fallback package name).
fn gemspec_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// The `name` field of a `package.json`, if present and a string.
fn package_json_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
}

/// Extract the first `"..."`/`'...'` string literal from a fragment.
fn first_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j <= bytes.len() {
                return Some(s[start..j].to_string());
            }
        }
        i += 1;
    }
    None
}

/// Recurse into `dir`, collecting members (which never nest). Paths are recorded
/// relative to `root` with `/` separators.
fn detect_subtree(root: &Path, dir: &Path, out: &mut Vec<Detected>) {
    if let Some((member_type, package)) = classify(dir) {
        let rel = rel_path(root, dir);
        let basename = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        out.push(Detected { path: rel, member_type, package, basename });
        return; // members do not nest
    }
    for child in child_dirs(dir) {
        detect_subtree(root, &child, out);
    }
}

/// The `/`-separated path of `dir` relative to `root` (root itself => ".").
///
/// Normalises ONLY the platform path separator (`MAIN_SEPARATOR`): '/' on Unix (a
/// no-op) and '\' on Windows. A blanket `replace('\\', "/")` would rewrite the
/// legal backslash byte in a Unix directory name and conflate distinct members —
/// the same defect fixed in the walker for issue #105.
fn rel_path(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(p) if p.as_os_str().is_empty() => ".".to_string(),
        Ok(p) => p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
        Err(_) => dir.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
    }
}

/// Detect the members of a workspace rooted at `root` (SPEC-V2.2 §3).
///
/// Children are searched first; if any member is found in a subtree the root is a
/// container. Only when the whole tree yields no member is the root itself
/// classified (the degenerate single-repo case).
pub fn detect_members(root: &Path) -> Vec<Member> {
    let mut detected: Vec<Detected> = Vec::new();
    for child in child_dirs(root) {
        detect_subtree(root, &child, &mut detected);
    }
    if detected.is_empty() {
        // Degenerate: the root itself may be the sole member.
        if let Some((member_type, package)) = classify(root) {
            let basename =
                root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            detected.push(Detected { path: ".".to_string(), member_type, package, basename });
        }
    }

    // Deterministic: sort by path ascending, then assign collision-suffixed names.
    detected.sort_by(|a, b| a.path.cmp(&b.path));
    let mut used: BTreeMap<String, usize> = BTreeMap::new();
    let mut members: Vec<Member> = Vec::with_capacity(detected.len());
    for d in detected {
        let count = used.entry(d.basename.clone()).or_insert(0);
        *count += 1;
        let name = if *count == 1 {
            d.basename.clone()
        } else {
            format!("{}-{}", d.basename, count)
        };
        members.push(Member { name, path: d.path, member_type: d.member_type, package: d.package });
    }
    members
}

/// Build the manifest for a workspace rooted at `root`.
pub fn build_manifest(root: &Path) -> Manifest {
    let name = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .or_else(|| root.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| ".".to_string());
    Manifest { version: 1, name, members: detect_members(root) }
}

// --- Manifest YAML (SPEC-V2.2 §2) ---

/// The store path of the workspace manifest: `<root>/.cce/workspace.yml`.
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(".cce").join(WORKSPACE_FILE)
}

/// The store path of the cross-member graph: `<root>/.cce/workspace-graph.json`.
pub fn graph_path(root: &Path) -> PathBuf {
    root.join(".cce").join(WORKSPACE_GRAPH_FILE)
}

impl Manifest {
    /// Serialize to the canonical, byte-deterministic YAML of §2. Hand-rolled so
    /// the exact bytes are under our control (and match across languages).
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str("version: 1\n");
        s.push_str(&format!("name: {}\n", yaml_scalar(&self.name)));
        s.push_str("members:\n");
        for m in &self.members {
            s.push_str(&format!("  - name: {}\n", yaml_scalar(&m.name)));
            s.push_str(&format!("    path: {}\n", yaml_scalar(&m.path)));
            s.push_str(&format!("    type: {}\n", m.member_type.as_str()));
            s.push_str(&format!("    package: {}\n", yaml_scalar(&m.package)));
        }
        s
    }

    /// Parse a manifest from YAML text (honours hand-written manifests as-is).
    pub fn from_yaml(text: &str) -> Result<Manifest, String> {
        #[derive(Deserialize)]
        struct RawMember {
            name: String,
            path: String,
            #[serde(rename = "type")]
            member_type: String,
            package: String,
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default = "default_version")]
            version: u32,
            #[serde(default)]
            name: String,
            #[serde(default)]
            members: Vec<RawMember>,
        }
        fn default_version() -> u32 {
            1
        }
        let raw: Raw =
            serde_yaml::from_str(text).map_err(|e| format!("invalid workspace.yml: {e}"))?;
        let mut members = Vec::with_capacity(raw.members.len());
        for rm in raw.members {
            let member_type = MemberType::parse(&rm.member_type)
                .ok_or_else(|| format!("unknown member type: {}", rm.member_type))?;
            members.push(Member { name: rm.name, path: rm.path, member_type, package: rm.package });
        }
        // Honour hand-written order but keep the deterministic path sort.
        members.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Manifest { version: raw.version, name: raw.name, members })
    }

    /// Load the manifest from `<root>/.cce/workspace.yml`.
    pub fn load(root: &Path) -> Result<Manifest, String> {
        let path = manifest_path(root);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("no workspace manifest at {} — run `cce workspace init` first", path.display())
        })?;
        Manifest::from_yaml(&text)
    }

    /// Write the manifest to `<root>/.cce/workspace.yml`, creating `.cce/`.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = manifest_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_yaml())
    }

    /// Look up a member by name.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }
}

/// Emit a YAML scalar, quoting only when needed for safety/round-trip. Neutral
/// identifiers (our member names/types) stay unquoted; anything with special
/// characters is double-quoted with minimal escaping.
fn yaml_scalar(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
        && !s.starts_with('-')
        && !s.starts_with('.');
    if safe {
        s.to_string()
    } else {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

// --- Cross-member dependency edges (SPEC-V2.2 §5) ---

/// A cross-member dependency edge `from -> to`, recording the manifest it came
/// from (`gemspec` | `gemfile` | `package.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub via: String,
}

/// The workspace dependency graph written to `workspace-graph.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceGraph {
    pub members: Vec<String>,
    pub edges: Vec<Edge>,
}

/// A dependency name declared by a member, plus the manifest it came from.
struct DeclaredDep {
    name: String,
    via: &'static str,
}

/// Extract every `add_dependency` / `add_runtime_dependency` /
/// `add_development_dependency` name from a gemspec (SPEC-V2.2 §5).
pub fn deps_from_gemspec(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        for kw in ["add_runtime_dependency", "add_development_dependency", "add_dependency"] {
            if let Some(idx) = t.find(kw) {
                let after = &t[idx + kw.len()..];
                if let Some(name) = first_string_literal(after) {
                    out.push(name);
                }
                break;
            }
        }
    }
    out
}

/// Extract every `gem "name"` from a Gemfile (SPEC-V2.2 §5). The first string arg
/// is the name; option keys (`path:`/`git:`) are ignored by construction.
pub fn deps_from_gemfile(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // `gem` as a whole leading token, then its first string literal.
        let rest = t.strip_prefix("gem").filter(|r| {
            r.starts_with(' ') || r.starts_with('(') || r.starts_with('\t') || r.starts_with('"')
        });
        if let Some(rest) = rest {
            if let Some(name) = first_string_literal(rest) {
                out.push(name);
            }
        }
    }
    out
}

/// Extract dependency names from a `package.json`: the keys of `dependencies`,
/// `devDependencies`, and `peerDependencies` (SPEC-V2.2 §5).
pub fn deps_from_package_json(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return out,
    };
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = v.get(section).and_then(|s| s.as_object()) {
            for key in obj.keys() {
                out.push(key.clone());
            }
        }
    }
    out
}

/// Read every dependency a member declares, from whatever manifests exist in its
/// root directory (SPEC-V2.2 §5).
fn member_declared_deps(root: &Path, member: &Member) -> Vec<DeclaredDep> {
    let dir = root.join(&member.path);
    let mut out: Vec<DeclaredDep> = Vec::new();

    if let Some(gemspec) = find_gemspec(&dir) {
        if let Ok(text) = std::fs::read_to_string(&gemspec) {
            for name in deps_from_gemspec(&text) {
                out.push(DeclaredDep { name, via: "gemspec" });
            }
        }
    }
    let gemfile = dir.join("Gemfile");
    if gemfile.is_file() {
        if let Ok(text) = std::fs::read_to_string(&gemfile) {
            for name in deps_from_gemfile(&text) {
                out.push(DeclaredDep { name, via: "gemfile" });
            }
        }
    }
    let pkg = dir.join("package.json");
    if pkg.is_file() {
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            for name in deps_from_package_json(&text) {
                out.push(DeclaredDep { name, via: "package.json" });
            }
        }
    }
    out
}

/// Build the cross-member dependency graph (SPEC-V2.2 §5). An edge `A -> B` is
/// recorded when a dependency `A` declares equals member `B`'s `package` or
/// `name`. Edges are deduplicated and sorted by `(from, to, via)`.
pub fn build_graph(root: &Path, manifest: &Manifest) -> WorkspaceGraph {
    // Resolve a declared dependency name to a member name (package or name match).
    let resolve = |dep: &str| -> Option<&str> {
        manifest.members.iter().find(|m| m.package == dep || m.name == dep).map(|m| m.name.as_str())
    };

    let mut edges: BTreeSet<(String, String, String)> = BTreeSet::new();
    for member in &manifest.members {
        for dep in member_declared_deps(root, member) {
            if let Some(target) = resolve(&dep.name) {
                if target == member.name {
                    continue; // no self-edges
                }
                edges.insert((member.name.clone(), target.to_string(), dep.via.to_string()));
            }
        }
    }

    WorkspaceGraph {
        members: manifest.members.iter().map(|m| m.name.clone()).collect(),
        edges: edges.into_iter().map(|(from, to, via)| Edge { from, to, via }).collect(),
    }
}

impl WorkspaceGraph {
    /// Serialize deterministically (compact JSON, sorted edges).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Write the graph to `<root>/.cce/workspace-graph.json`.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = graph_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json())
    }

    /// Parse a graph from its JSON text (the shape `to_json` writes; `members`
    /// and `edges` both default to empty). The inverse used by the sync layer
    /// when a *published* graph arrives as cache bytes rather than a local file
    /// (#55, the self-describing cache).
    pub fn from_json(text: &str) -> Result<WorkspaceGraph, String> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            members: Vec<String>,
            #[serde(default)]
            edges: Vec<RawEdge>,
        }
        #[derive(Deserialize)]
        struct RawEdge {
            from: String,
            to: String,
            via: String,
        }
        let raw: Raw =
            serde_json::from_str(text).map_err(|e| format!("invalid workspace-graph.json: {e}"))?;
        let edges =
            raw.edges.into_iter().map(|e| Edge { from: e.from, to: e.to, via: e.via }).collect();
        Ok(WorkspaceGraph { members: raw.members, edges })
    }

    /// Load the graph from `<root>/.cce/workspace-graph.json`, or an empty graph
    /// (members from the manifest) when absent.
    pub fn load_or_empty(root: &Path, manifest: &Manifest) -> WorkspaceGraph {
        let path = graph_path(root);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut graph) = WorkspaceGraph::from_json(&text) {
                if graph.members.is_empty() {
                    graph.members = manifest.members.iter().map(|m| m.name.clone()).collect();
                }
                return graph;
            }
        }
        WorkspaceGraph {
            members: manifest.members.iter().map(|m| m.name.clone()).collect(),
            edges: Vec::new(),
        }
    }

    /// The target member names reachable from `from` via a cross-member edge, in
    /// deterministic (sorted) order.
    pub fn targets_from(&self, from: &str) -> Vec<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for e in &self.edges {
            if e.from == from {
                set.insert(e.to.clone());
            }
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"))
    }

    #[test]
    fn detects_three_members_with_types_and_packages() {
        let members = detect_members(&fixture());
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        // Sorted by path: app, engines/billing, web.
        assert_eq!(names, vec!["app", "billing", "web"]);

        let app = &members[0];
        assert_eq!(app.path, "app");
        assert_eq!(app.member_type, MemberType::RailsApp);
        assert_eq!(app.package, "app");

        let billing = &members[1];
        assert_eq!(billing.path, "engines/billing");
        assert_eq!(billing.member_type, MemberType::RubyEngine);
        assert_eq!(billing.package, "billing");

        let web = &members[2];
        assert_eq!(web.path, "web");
        assert_eq!(web.member_type, MemberType::Typescript);
        assert_eq!(web.package, "web");
    }

    #[test]
    fn members_do_not_nest() {
        // The app member has an inner `app/` dir; it must NOT be a second member.
        let members = detect_members(&fixture());
        assert_eq!(members.iter().filter(|m| m.path.starts_with("app")).count(), 1);
    }

    #[test]
    fn degenerate_single_repo_root_is_sole_member() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("thing.gemspec"), "spec.name = \"thing\"\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        let members = detect_members(tmp.path());
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
        assert_eq!(members[0].member_type, MemberType::RubyGem);
        assert_eq!(members[0].package, "thing");
    }

    #[test]
    fn gemspec_without_engine_marker_is_a_gem() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("pkg");
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(
            d.join("pkg.gemspec"),
            "Gem::Specification.new do |s|\n  s.name = 'pkg'\nend\n",
        )
        .unwrap();
        let members = detect_members(tmp.path());
        assert_eq!(members[0].member_type, MemberType::RubyGem);
        assert_eq!(members[0].package, "pkg");
    }

    #[test]
    fn javascript_without_tsconfig_is_javascript() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("front");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), "{\"name\":\"front\"}").unwrap();
        let members = detect_members(tmp.path());
        assert_eq!(members[0].member_type, MemberType::Javascript);
        assert_eq!(members[0].package, "front");
    }

    #[test]
    fn name_collision_gets_suffix_in_path_order() {
        let tmp = tempfile::tempdir().unwrap();
        // Two members that would both be named "widget".
        for sub in ["a/widget", "b/widget"] {
            let d = tmp.path().join(sub);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("package.json"), "{\"name\":\"widget\"}").unwrap();
        }
        let members = detect_members(tmp.path());
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        // Path-sorted: a/widget first keeps "widget", b/widget gets "-2".
        assert_eq!(names, vec!["widget", "widget-2"]);
        assert_eq!(members[0].path, "a/widget");
        assert_eq!(members[1].path, "b/widget");
    }

    #[test]
    fn manifest_yaml_round_trips_and_is_deterministic() {
        let m = build_manifest(&fixture());
        let yaml = m.to_yaml();
        assert_eq!(yaml, m.to_yaml());
        // Canonical shape.
        assert!(yaml.starts_with("version: 1\n"));
        assert!(yaml.contains("    type: ruby-engine\n"));
        let parsed = Manifest::from_yaml(&yaml).unwrap();
        assert_eq!(parsed.members, m.members);
        assert_eq!(parsed.version, 1);
    }

    #[test]
    fn store_only_member_type_round_trips_and_detection_never_emits_it() {
        // The #54 consumer-mode variant: writer/parser round-trip through the
        // canonical YAML, byte-identically.
        assert_eq!(MemberType::StoreOnly.as_str(), "store-only");
        assert_eq!(MemberType::parse("store-only"), Some(MemberType::StoreOnly));
        let m = Manifest {
            version: 1,
            name: "ctx".to_string(),
            members: vec![Member {
                name: "billing".to_string(),
                path: "billing".to_string(),
                member_type: MemberType::StoreOnly,
                package: "billing".to_string(),
            }],
        };
        let yaml = m.to_yaml();
        assert!(yaml.contains("    type: store-only\n"), "got: {yaml}");
        let parsed = Manifest::from_yaml(&yaml).unwrap();
        assert_eq!(parsed, m);
        assert_eq!(parsed.to_yaml(), yaml, "round-trip must be byte-identical");

        // Detection classifies source markers only — a store-only member can never
        // be detected (there is no source), so detected manifests are unaffected
        // by the new variant.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("bare").join(".cce")).unwrap();
        assert!(detect_members(tmp.path()).is_empty());
    }

    #[test]
    fn hand_written_manifest_with_known_types_round_trips_byte_identically() {
        // Golden: the pre-#54 manifest grammar is untouched — a hand-written
        // manifest re-serializes to its exact input bytes.
        let golden = "version: 1\nname: shop\nmembers:\n  - name: api\n    path: api\n    type: rails-app\n    package: api\n  - name: web\n    path: web\n    type: javascript\n    package: web\n";
        let m = Manifest::from_yaml(golden).unwrap();
        assert_eq!(m.to_yaml(), golden);
    }

    #[test]
    fn hand_written_manifest_is_honoured() {
        let yaml = "version: 1\nname: shop\nmembers:\n  - name: web\n    path: web\n    type: javascript\n    package: web\n  - name: api\n    path: api\n    type: rails-app\n    package: api\n";
        let m = Manifest::from_yaml(yaml).unwrap();
        assert_eq!(m.name, "shop");
        // Re-sorted by path: api before web.
        assert_eq!(m.members[0].name, "api");
        assert_eq!(m.members[1].name, "web");
    }

    #[test]
    fn dep_extractors() {
        assert_eq!(
            deps_from_gemspec("  spec.add_dependency \"activesupport\"\n  spec.add_runtime_dependency 'rack'\n  spec.add_development_dependency(\"rspec\")\n"),
            vec!["activesupport", "rack", "rspec"]
        );
        assert_eq!(
            deps_from_gemfile(
                "source \"https://rubygems.org\"\ngem \"billing\"\ngem 'rails', '~> 7'\ngemspec\n"
            ),
            vec!["billing", "rails"]
        );
        assert_eq!(
            deps_from_package_json(
                "{\"dependencies\":{\"left-pad\":\"1\"},\"devDependencies\":{\"jest\":\"2\"}}"
            ),
            vec!["left-pad", "jest"]
        );
    }

    #[test]
    fn builds_exactly_the_app_to_billing_edge() {
        let root = fixture();
        let manifest = build_manifest(&root);
        let graph = build_graph(&root, &manifest);
        assert_eq!(graph.members, vec!["app", "billing", "web"]);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            graph.edges[0],
            Edge { from: "app".to_string(), to: "billing".to_string(), via: "gemfile".to_string() }
        );
    }

    #[test]
    fn graph_from_json_round_trips_and_rejects_bad_input() {
        // The published-graph parse (#55): `to_json` → `from_json` is the identity.
        let root = fixture();
        let manifest = build_manifest(&root);
        let graph = build_graph(&root, &manifest);
        let parsed = WorkspaceGraph::from_json(&graph.to_json()).unwrap();
        assert_eq!(parsed, graph);
        assert_eq!(parsed.to_json(), graph.to_json());
        // Absent fields default empty; junk is a clear error, not a panic.
        let empty = WorkspaceGraph::from_json("{}").unwrap();
        assert!(empty.members.is_empty() && empty.edges.is_empty());
        let err = WorkspaceGraph::from_json("not json").unwrap_err();
        assert!(err.contains("invalid workspace-graph.json"), "got: {err}");
    }

    #[test]
    fn graph_json_shape_and_reload() {
        let root = fixture();
        let manifest = build_manifest(&root);
        let graph = build_graph(&root, &manifest);
        let json = graph.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["members"], serde_json::json!(["app", "billing", "web"]));
        assert_eq!(v["edges"][0]["from"], "app");
        assert_eq!(v["edges"][0]["to"], "billing");
        assert_eq!(v["edges"][0]["via"], "gemfile");
        assert_eq!(graph.targets_from("app"), vec!["billing"]);
        assert!(graph.targets_from("web").is_empty());
    }
}
