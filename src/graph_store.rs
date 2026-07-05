//! # graph_store — the import graph and neighbor lookup
//!
//! **Why this file exists:** SPEC §6.7 optionally expands a result set with
//! chunks from files that are import-related to the top hits. That needs a small
//! directed graph over files built from extracted imports.
//!
//! **What it is / does:** Resolves each file's import module names to corpus
//! files (by matching a module to a file whose stem equals it, or whose path
//! ends with `<module>.py`/`.js`) and records directed edges `A -> B`. Offers a
//! neighbor lookup that returns files connected in either direction.
//!
//! **Responsibilities:**
//! - Own edge construction from `file_imports` + the set of corpus files.
//! - Own `neighbors(file)`: successors and predecessors, deduplicated.
//! - It does NOT rank or pull chunks; the retriever does that using this graph.

use std::collections::{BTreeMap, BTreeSet};

/// A directed import graph over corpus file paths.
#[derive(Debug, Default)]
pub struct Graph {
    /// A -> {B, ...}: file A imports files B.
    out_edges: BTreeMap<String, BTreeSet<String>>,
    /// B -> {A, ...}: reverse edges for predecessor lookup.
    in_edges: BTreeMap<String, BTreeSet<String>>,
}

/// Resolve a module name to a corpus file path (SPEC §6.7).
fn resolve_module<'a>(module: &str, files: &'a [String]) -> Option<&'a String> {
    for f in files {
        // path stem (filename without extension) equals the module
        let file_name = f.rsplit('/').next().unwrap_or(f);
        let stem = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(file_name);
        if stem == module {
            return Some(f);
        }
    }
    for f in files {
        if f.ends_with(&format!("{module}.py")) || f.ends_with(&format!("{module}.js")) {
            return Some(f);
        }
    }
    None
}

impl Graph {
    /// Build the graph from `file_imports` (file path -> module names) and the
    /// set of corpus file paths (used to resolve modules to files).
    pub fn build(file_imports: &BTreeMap<String, Vec<String>>, files: &[String]) -> Graph {
        let mut g = Graph::default();
        for (file, modules) in file_imports {
            for m in modules {
                if let Some(target) = resolve_module(m, files) {
                    if target == file {
                        continue; // no self-edges
                    }
                    g.out_edges.entry(file.clone()).or_default().insert(target.clone());
                    g.in_edges.entry(target.clone()).or_default().insert(file.clone());
                }
            }
        }
        g
    }

    /// True if there is a directed edge `from -> to`.
    pub fn has_edge(&self, from: &str, to: &str) -> bool {
        self.out_edges.get(from).map(|s| s.contains(to)).unwrap_or(false)
    }

    /// Every directed edge `from -> to` as a pair, sorted. Lets a caller union
    /// several graphs (SPEC-V2.2 §6 builds the combined graph as the union of each
    /// member's intra-store import graph over member-namespaced paths).
    pub fn out_pairs(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (from, tos) in &self.out_edges {
            for to in tos {
                pairs.push((from.clone(), to.clone()));
            }
        }
        pairs
    }

    /// Build a graph directly from directed `from -> to` pairs (both directions
    /// recorded for neighbor lookup).
    pub fn from_pairs(pairs: &[(String, String)]) -> Graph {
        let mut g = Graph::default();
        for (from, to) in pairs {
            if from == to {
                continue;
            }
            g.out_edges.entry(from.clone()).or_default().insert(to.clone());
            g.in_edges.entry(to.clone()).or_default().insert(from.clone());
        }
        g
    }

    /// Neighbors of `file` in either direction (successors + predecessors),
    /// returned sorted and deduplicated.
    pub fn neighbors(&self, file: &str) -> Vec<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        if let Some(s) = self.out_edges.get(file) {
            set.extend(s.iter().cloned());
        }
        if let Some(s) = self.in_edges.get(file) {
            set.extend(s.iter().cloned());
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imports(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(f, ms)| (f.to_string(), ms.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn edge_from_import() {
        let files = vec!["auth.py".to_string(), "payments.py".to_string()];
        let fi = imports(&[("payments.py", &["auth"])]);
        let g = Graph::build(&fi, &files);
        assert!(g.has_edge("payments.py", "auth.py"));
        assert!(!g.has_edge("auth.py", "payments.py"));
    }

    #[test]
    fn neighbors_both_directions() {
        let files = vec!["auth.py".to_string(), "payments.py".to_string()];
        let fi = imports(&[("payments.py", &["auth"])]);
        let g = Graph::build(&fi, &files);
        assert_eq!(g.neighbors("auth.py"), vec!["payments.py"]);
        assert_eq!(g.neighbors("payments.py"), vec!["auth.py"]);
    }

    #[test]
    fn resolve_by_path_suffix() {
        let files = vec!["pkg/sub/mod.py".to_string(), "main.py".to_string()];
        let fi = imports(&[("main.py", &["mod"])]);
        let g = Graph::build(&fi, &files);
        assert!(g.has_edge("main.py", "pkg/sub/mod.py"));
    }

    #[test]
    fn unresolved_module_no_edge() {
        let files = vec!["a.py".to_string()];
        let fi = imports(&[("a.py", &["numpy"])]);
        let g = Graph::build(&fi, &files);
        assert!(g.neighbors("a.py").is_empty());
    }
}
