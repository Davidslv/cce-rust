//! # tests/knowledge_sync — end-to-end knowledge corpus sync (SPEC-SYNC-KNOWLEDGE §11)
//!
//! **Why this file exists:** M5.1/M5.2 move knowledge corpora through the
//! content-addressed cache, and the spec's acceptance bar is process-level:
//! fixture NDJSON → `cce knowledge index` → `push` to a `file://` bare remote →
//! wipe → `pull` → a store **byte-identical** to the pre-push one, with
//! `context_search source: knowledge` returning the same hits. Only spawning the
//! real binary proves the fresh-process guarantee, the §5 refusals, retention,
//! the §4.4 freshness split (`data_as_of` inside the artifact, `pushed_at`
//! outside), and the §4.6 redaction guarantee on what actually lands remotely.
//!
//! **What it is / does:** Builds a bare git remote and project roots in temp
//! dirs, sets `CCE_HOME` to a temp dir so working clones never touch `~/.cce`,
//! and drives the binary: index, push, pull, the guards (missing/invalid
//! corpus_id, embedding-less Phase-A store, planted secret), retention pruning,
//! and the MCP search parity between producer and pulled consumer. Hermetic: no
//! network, LFS off (the tests/sync.rs rule — no `git-lfs` binary needed).
//!
//! **Responsibilities:**
//! - Own the process-level M5.1+M5.2 acceptance tests.
//! - It does NOT cover `sync list`/`pull --all`/`verify --checksum-only`
//!   knowledge surfaces (M5.3) or docs (M5.4).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// Run `cce <args>` with `CCE_HOME` pointed at `home` (hermetic working clones).
fn cce(home: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).env("CCE_HOME", home).output().unwrap()
}

fn assert_ok(out: &Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_err(out: &Output, needle: &str, what: &str) {
    assert!(!out.status.success(), "{what} unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(needle), "{what}: expected `{needle}` in:\n{stderr}");
}

/// A bare git repo acting as the remote; returns (tempdir, file:// URL).
fn bare_remote() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new("git")
        .args(["init", "--bare", "-q", "-b", "main"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let url = format!("file://{}", tmp.path().to_string_lossy());
    (tmp, url)
}

/// Clone the bare remote and return the checkout dir, so tests can inspect the
/// exact bytes the cache carries (plain git — LFS is off throughout).
fn checkout_of(url: &str) -> tempfile::TempDir {
    let dst = tempfile::tempdir().unwrap();
    let out = Command::new("git")
        .args(["clone", "-q", url])
        .arg(dst.path().join("cache"))
        .output()
        .unwrap();
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    dst
}

/// A project root with LFS off and the knowledge sync keys configured.
fn project_root(remote: &str, corpus: &str, retention: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
    std::fs::write(
        tmp.path().join(".cce").join("config"),
        format!(
            "sync:\n  remote: {remote}\n  lfs: false\nknowledge:\n  sync:\n    corpus_id: \
             {corpus}\n    retention: {retention}\n"
        ),
    )
    .unwrap();
    tmp
}

/// The two-record synthetic feed (fixed timestamps ⇒ deterministic
/// `data_as_of`; a planted secret in record 2 for the §4.6 guard).
fn feed() -> String {
    let r1 = serde_json::json!({
        "id": "kn:1",
        "title": "Password hashing policy",
        "body": "## Rule\n\nStore each password only as a salted slow hash; never keep the plaintext password.",
        "source": "handbook",
        "url": "https://example.test/1",
        "state": "closed",
        "state_reason": "completed",
        "updated_at": "2026-02-01T10:00:00Z",
        "labels": ["security"],
        "links": ["https://example.test/pull/7"],
    });
    let r2 = serde_json::json!({
        "id": "kn:2",
        "title": "Deployment config",
        "body": "## Setup\n\nSet api_key = s3cr3tvalue123 in the deployment env.",
        "source": "handbook",
        "state": "open",
        "updated_at": "2026-01-01T00:00:00Z",
        "labels": ["ops"],
    });
    format!("{}\n{}\n", serde_json::to_string(&r1).unwrap(), serde_json::to_string(&r2).unwrap())
}

/// `cce knowledge index` the feed into `root`; returns the snapshot id.
fn index_knowledge(home: &Path, root: &Path, feed_text: &str) -> String {
    let path = root.join("feed.jsonl");
    std::fs::write(&path, feed_text).unwrap();
    let out =
        cce(home, &["knowledge", "index", path.to_str().unwrap(), "--dir", root.to_str().unwrap()]);
    let stdout = assert_ok(&out, "knowledge index");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("  snapshot  : "))
        .expect("snapshot line")
        .trim()
        .to_string()
}

fn knowledge_dir(root: &Path) -> PathBuf {
    root.join(".cce").join("knowledge")
}

/// Drive an MCP session with `input` on stdin, returning stdout.
fn drive(args: &[&str], input: &str) -> String {
    let mut cmd = Command::new(bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Extract the tool-result text for the response with the given id.
fn tool_text(stdout: &str, id: i64) -> String {
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["id"] == id {
            return v["result"]["content"][0]["text"].as_str().unwrap().to_string();
        }
    }
    panic!("no response with id {id} in:\n{stdout}");
}

/// One `context_search source: knowledge` call over the MCP server at `root`.
fn knowledge_search(root: &Path, query: &str) -> String {
    let input = format!(
        "{}{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"context_search\",\"arguments\":{{\"query\":\"{query}\",\"source\":\"knowledge\",\"detail\":\"full\"}}}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
    );
    let out = drive(&["mcp", "--dir", root.to_str().unwrap()], &input);
    tool_text(&out, 2)
}

/// The spec's §7-shaped exit bar for M5.1+M5.2: index → push → wipe → pull ⇒
/// the installed store is byte-identical to the pre-push one, and
/// `context_search source: knowledge` returns the same hits.
#[test]
fn index_push_wipe_pull_is_byte_identical_and_search_matches() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), root.path(), &feed());

    // The pre-push native store bytes + the producer-side search result.
    let snap_path = knowledge_dir(root.path()).join(format!("{snapshot}.json"));
    let before_store = std::fs::read(&snap_path).unwrap();
    let before_current =
        std::fs::read_to_string(knowledge_dir(root.path()).join("current")).unwrap();
    let before_hits = knowledge_search(root.path(), "password hashing policy");
    assert!(
        before_hits.contains("[knowledge] Password hashing policy"),
        "producer search missing the hit:\n{before_hits}"
    );

    // Push (corpus_id + remote from config), then WIPE the local store.
    let out = cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]);
    let stdout = assert_ok(&out, "knowledge push");
    assert!(stdout.contains(&format!("Pushed corpus fixture@{snapshot}")), "{stdout}");
    assert!(stdout.contains("knowledge/v1/fixture/"), "{stdout}");
    std::fs::remove_dir_all(knowledge_dir(root.path())).unwrap();

    // Pull re-installs it — byte-identical, marker recorded.
    let out = cce(home.path(), &["knowledge", "pull", "--dir", root.path().to_str().unwrap()]);
    let stdout = assert_ok(&out, "knowledge pull");
    assert!(stdout.contains(&format!("Pulled corpus fixture@{snapshot}")), "{stdout}");
    let after_store = std::fs::read(&snap_path).unwrap();
    assert_eq!(before_store, after_store, "the pulled store must be byte-identical");
    assert_eq!(
        before_current,
        std::fs::read_to_string(knowledge_dir(root.path()).join("current")).unwrap()
    );
    let marker: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(knowledge_dir(root.path()).join("synced.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["corpus_id"], "fixture");
    assert_eq!(marker["snapshot"], snapshot);
    assert_eq!(marker["installed_sha256"].as_str().unwrap().len(), 64);

    // Zero retrieval changes (§1.5): the same search returns the same hits.
    let after_hits = knowledge_search(root.path(), "password hashing policy");
    assert_eq!(before_hits, after_hits, "search over the pulled store must match the producer");
}

/// A fresh consumer root (no source, no prior state) pulls with explicit flags
/// only — the repo-less consumer shape.
#[test]
fn fresh_consumer_pulls_a_byte_identical_store_with_flags_only() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );

    let consumer = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &[
            "knowledge",
            "pull",
            "--corpus",
            "fixture",
            "--latest",
            "--remote",
            &url,
            "--dir",
            consumer.path().to_str().unwrap(),
        ],
    );
    assert_ok(&out, "consumer pull");
    let name = format!("{snapshot}.json");
    assert_eq!(
        std::fs::read(knowledge_dir(producer.path()).join(&name)).unwrap(),
        std::fs::read(knowledge_dir(consumer.path()).join(&name)).unwrap(),
        "consumer store must equal the producer's"
    );
}

#[test]
fn push_refuses_missing_and_invalid_corpus_ids() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    // A store exists but no corpus_id is configured and none is passed.
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".cce")).unwrap();
    std::fs::write(
        root.path().join(".cce").join("config"),
        format!("sync:\n  remote: {url}\n  lfs: false\n"),
    )
    .unwrap();
    index_knowledge(home.path(), root.path(), &feed());

    let out = cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]);
    assert_err(&out, "cannot determine corpus_id", "push without a corpus_id");

    let out = cce(
        home.path(),
        &["knowledge", "push", "--corpus", "has space", "--dir", root.path().to_str().unwrap()],
    );
    assert_err(&out, "invalid corpus_id", "push with an invalid corpus_id");
}

#[test]
fn push_refuses_an_embedding_less_phase_a_store() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "all");
    // Hand-write a Phase-A store: `embedding` absent (serde default []).
    let dir = knowledge_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    let store = serde_json::json!({
        "schema": "cce.knowledge/v1",
        "snapshot": "00000000aaaaaaaa",
        "records": 1,
        "chunks": [{
            "chunk_id": "1111111111111111",
            "record_id": "kn:1",
            "kind": "T",
            "name": "# T",
            "start_line": 1,
            "end_line": 1,
            "token_count": 2,
            "content": "# T\n",
            "source": "handbook",
            "labels": [],
        }],
    });
    std::fs::write(dir.join("00000000aaaaaaaa.json"), store.to_string()).unwrap();
    std::fs::write(dir.join("current"), "00000000aaaaaaaa\n").unwrap();

    let out = cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]);
    assert_err(&out, "Re-ingest", "push of a Phase-A store");
}

#[test]
fn push_refuses_without_a_local_store_and_fails_cleanly_offline() {
    let home = tempfile::tempdir().unwrap();
    // No store at all.
    let empty = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &["knowledge", "push", "--corpus", "c1", "--dir", empty.path().to_str().unwrap()],
    );
    assert_err(&out, "no local knowledge store", "push without a store");

    // A store but no remote anywhere: clean message, local state intact (§10).
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".cce")).unwrap();
    std::fs::write(root.path().join(".cce").join("config"), "sync:\n  lfs: false\n").unwrap();
    let snapshot = index_knowledge(home.path(), root.path(), &feed());
    let out = cce(
        home.path(),
        &["knowledge", "push", "--corpus", "c1", "--dir", root.path().to_str().unwrap()],
    );
    assert_err(&out, "no sync remote configured", "push without a remote");
    assert!(knowledge_dir(root.path()).join(format!("{snapshot}.json")).exists());
}

/// §4.6: the redactor ran at index time, so what lands on the remote — the only
/// thing that travels — carries the redaction marker, never the raw secret. And
/// `knowledge index` exposes no redaction-bypass flag (asserted on --help).
#[test]
fn planted_secret_arrives_redacted_in_the_pushed_artifact() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), root.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]),
        "push",
    );

    let co = checkout_of(&url);
    let cck = co.path().join("cache").join("knowledge").join("v1").join("fixture");
    let artifact = std::fs::read_to_string(cck.join(format!("{snapshot}.cck"))).unwrap();
    assert!(!artifact.contains("s3cr3tvalue123"), "raw secret leaked into the artifact");
    assert!(artifact.contains("[REDACTED:SECRET]"), "redaction marker missing");
    // The raw feed never travels (§4.6): only the .cck, pointer, corpus.json.
    assert!(!cck.join("feed.jsonl").exists());

    // No bypass flag exists on `knowledge index` (§4.6, by design).
    let help = cce(home.path(), &["knowledge", "index", "--help"]);
    let text = assert_ok(&help, "knowledge index --help");
    assert!(!text.contains("allow-secrets"), "a redaction-bypass flag appeared:\n{text}");
}

/// §4.5: push N+2 snapshots with keep-last-N ⇒ the oldest are pruned, the
/// snapshot named by `current` survives, pointer + corpus.json intact.
#[test]
fn retention_prunes_oldest_snapshots_and_current_survives() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "keep-last-2");

    let mut snapshots = Vec::new();
    for n in 0..4 {
        let extra = serde_json::json!({
            "id": format!("kn:{n}"),
            "title": format!("Note {n}"),
            "body": format!("Body number {n}."),
            "source": "handbook",
            "updated_at": format!("2026-01-0{}T00:00:00Z", n + 1),
        });
        let feed_text = format!("{}\n", serde_json::to_string(&extra).unwrap());
        snapshots.push(index_knowledge(home.path(), root.path(), &feed_text));
        let out = cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]);
        assert_ok(&out, "push");
    }

    let co = checkout_of(&url);
    let corpus = co.path().join("cache").join("knowledge").join("v1").join("fixture");
    let mut ccks: Vec<String> = std::fs::read_dir(&corpus)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".cck"))
        .collect();
    ccks.sort();
    assert_eq!(ccks.len(), 2, "keep-last-2 leaves two snapshots, got {ccks:?}");
    assert!(ccks.contains(&format!("{}.cck", snapshots[3])), "current must survive");
    assert!(!ccks.contains(&format!("{}.cck", snapshots[0])), "the oldest must be pruned");
    // The pointer names the newest snapshot; corpus.json is present.
    assert_eq!(std::fs::read_to_string(corpus.join("current")).unwrap().trim(), snapshots[3]);
    assert!(corpus.join("corpus.json").exists());
}

/// §4.4: two freshness signals — a deterministic `data_as_of` INSIDE the
/// artifact (byte-pinned by the fixed fixture timestamps) and a `pushed_at`
/// OUTSIDE it, in the published corpus.json.
#[test]
fn corpus_json_carries_pushed_at_and_the_artifact_pins_data_as_of() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), root.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]),
        "push",
    );

    let co = checkout_of(&url);
    let corpus = co.path().join("cache").join("knowledge").join("v1").join("fixture");

    // corpus.json: the push-age signal, rewritten on every push.
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus.join("corpus.json")).unwrap())
            .unwrap();
    assert_eq!(meta["schema"], "cce.knowledgemeta/v1");
    assert_eq!(meta["corpus_id"], "fixture");
    assert_eq!(meta["current"], snapshot);
    assert_eq!(meta["data_as_of"], "2026-02-01T10:00:00Z");
    let pushed_at = meta["pushed_at"].as_str().unwrap();
    assert!(pushed_at.ends_with('Z') && pushed_at.contains('T'), "not ISO-8601: {pushed_at}");

    // The artifact: deterministic, provenance-free, data_as_of byte-pinned.
    let artifact = std::fs::read_to_string(corpus.join(format!("{snapshot}.cck"))).unwrap();
    let manifest = artifact.lines().next().unwrap();
    assert!(manifest.contains("\"data_as_of\":\"2026-02-01T10:00:00Z\""), "{manifest}");
    assert!(!manifest.contains("pushed_at"), "pushed_at must stay OUTSIDE the artifact");
    assert!(!artifact.contains("built_at") && !artifact.contains("built_by"));
}

/// The additivity gate: knowledge sync leaves the code key space, the code
/// artifacts, and `sync list` untouched (asserted, not assumed — §11).
#[test]
fn knowledge_keys_are_additive_beside_code_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();

    // A code repo pushed the normal way.
    let src = tempfile::tempdir().unwrap();
    let d = src.path();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["-c", "user.name=t", "-c", "user.email=t@e"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("auth.py"), "def login(user):\n    return hash(user)\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    assert_ok(
        &cce(
            home.path(),
            &[
                "sync",
                "init",
                "--remote",
                &url,
                "--no-lfs",
                "--repo-id",
                "example.com__acme__demo",
                "--dir",
                d.to_str().unwrap(),
            ],
        ),
        "sync init",
    );
    assert_ok(&cce(home.path(), &["sync", "push", "--dir", d.to_str().unwrap()]), "sync push");
    let list_before = assert_ok(
        &cce(home.path(), &["sync", "list", "--remote", &url, "--json"]),
        "sync list before",
    );

    // A knowledge corpus pushed beside it.
    let root = project_root(&url, "fixture", "all");
    index_knowledge(home.path(), root.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]),
        "knowledge push",
    );

    // Additivity, the M5.3 shape (SPEC-SYNC-KNOWLEDGE §6): the listing stays
    // `cce.synclist/v1` and every pre-existing field is byte-stable — the
    // corpus appears ONLY as the new optional `knowledge` array. (Before M5.3
    // this test pinned full-byte equality; the knowledge section landing is
    // exactly the anticipated change.)
    let list_after = assert_ok(
        &cce(home.path(), &["sync", "list", "--remote", &url, "--json"]),
        "sync list after",
    );
    let before: serde_json::Value = serde_json::from_str(&list_before).unwrap();
    let mut after: serde_json::Value = serde_json::from_str(&list_after).unwrap();
    assert!(before.get("knowledge").is_none(), "no knowledge key before a corpus exists");
    let corpora = after.as_object_mut().unwrap().remove("knowledge").expect("knowledge array");
    assert_eq!(corpora[0]["corpus_id"], "fixture");
    assert_eq!(
        before, after,
        "every pre-existing field of the listing must be byte-stable beside a corpus"
    );

    // And the code pull still works untouched.
    let out =
        cce(home.path(), &["sync", "pull", "--latest", "--force", "--dir", d.to_str().unwrap()]);
    assert_ok(&out, "code pull beside a knowledge corpus");
}
