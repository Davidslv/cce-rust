//! # tests/knowledge_sync — end-to-end knowledge corpus sync (SPEC-SYNC-KNOWLEDGE §11)
//!
//! **Why this file exists:** M5 moves knowledge corpora through the
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
//! and the MCP search parity between producer and pulled consumer. The M5.3
//! section covers the consumer surface: the `sync list` knowledge section
//! (byte-pinned human + JSON goldens), `pull --all` corpus install (selection,
//! byte-identity to a direct pull, idempotent refresh), the
//! `verify --checksum-only` knowledge row, the MCP `index_status` knowledge
//! block, and the §7 bare-directory end-to-end story. Hermetic: no network, LFS
//! off (the tests/sync.rs rule — no `git-lfs` binary needed).
//!
//! **Responsibilities:**
//! - Own the process-level M5 acceptance tests (M5.1–M5.3).
//! - It does NOT cover the code-artifact sync surfaces (tests/sync.rs).

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
    drive_env(args, input, &[])
}

/// `drive` with extra environment variables (e.g. a hermetic `CCE_HOME` so the
/// server's best-effort remote lookups use the test's working clone).
fn drive_env(args: &[&str], input: &str, envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
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

/// §5 push guard (#90), end to end at the CLI: a partial local rebuild that
/// would DROP a published record is refused naming the removed ids and
/// `--force`; `--dry-run` prints the diff and leaves the remote byte-untouched;
/// `--force` publishes the shrink.
#[test]
fn push_guard_refuses_a_shrink_dry_run_pushes_nothing_and_force_overrides() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let root = project_root(&url, "fixture", "all");
    // Publish the full two-record corpus (kn:1 + kn:2)…
    let published = index_knowledge(home.path(), root.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]),
        "first push",
    );
    // …then rebuild locally from a one-record subset feed (kn:1 only).
    let subset = feed().lines().next().unwrap().to_string() + "\n";
    let shrunk = index_knowledge(home.path(), root.path(), &subset);
    assert_ne!(published, shrunk);

    // --dry-run: the diff prints, exit 0, and NOTHING lands on the remote.
    let out = cce(
        home.path(),
        &["knowledge", "push", "--dry-run", "--dir", root.path().to_str().unwrap()],
    );
    let stdout = assert_ok(&out, "push --dry-run");
    assert!(stdout.contains(&format!("outgoing {shrunk} vs remote current {published}")));
    assert!(stdout.contains("removed : 1 — kn:2"), "got: {stdout}");
    assert!(stdout.contains("Nothing pushed (--dry-run)."), "got: {stdout}");
    let co = checkout_of(&url);
    let corpus = co.path().join("cache").join("knowledge").join("v1").join("fixture");
    assert_eq!(std::fs::read_to_string(corpus.join("current")).unwrap().trim(), published);
    assert!(!corpus.join(format!("{shrunk}.cck")).exists(), "dry-run must upload nothing");

    // A plain push refuses the shrink, naming the removed id and the override.
    let out = cce(home.path(), &["knowledge", "push", "--dir", root.path().to_str().unwrap()]);
    assert_err(&out, "removed : 1 — kn:2", "shrinking push");
    assert_err(&out, "--force", "shrinking push");

    // --force publishes it.
    let out =
        cce(home.path(), &["knowledge", "push", "--force", "--dir", root.path().to_str().unwrap()]);
    let stdout = assert_ok(&out, "push --force");
    assert!(stdout.contains(&format!("Pushed corpus fixture@{shrunk}")), "got: {stdout}");
    let co = checkout_of(&url);
    let corpus = co.path().join("cache").join("knowledge").join("v1").join("fixture");
    assert_eq!(std::fs::read_to_string(corpus.join("current")).unwrap().trim(), shrunk);
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
        // Each snapshot carries a DIFFERENT single record, so every re-push is
        // an intentional replacement — --force past the §5 shrink guard (#90);
        // this test exercises retention, not the guard.
        let out = cce(
            home.path(),
            &["knowledge", "push", "--force", "--dir", root.path().to_str().unwrap()],
        );
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

// --- M5.3: the consumer surface (SPEC-SYNC-KNOWLEDGE §6/§7) ---

/// Run a git command in `dir`, asserting success (the tests/sync.rs helper).
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=t@e"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Seed a cache repo with arbitrary `(path, bytes)` entries via plain git — a
/// hermetic stand-in for pushes (fixed bytes ⇒ byte-pinnable listings).
fn seed_cache(url: &str, files: &[(&str, &[u8])]) {
    let work = tempfile::tempdir().unwrap();
    let d = work.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["remote", "add", "origin", url]);
    let _ = Command::new("git").arg("-C").arg(d).args(["fetch", "-q", "origin"]).output();
    let _ = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["reset", "-q", "--hard", "origin/main"])
        .output();
    for (path, bytes) in files {
        let p = d.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "seed"]);
    git(d, &["push", "-q", "origin", "HEAD:main"]);
}

/// A tiny one-file git repo with the given content, committed on `main`.
fn tiny_repo(file: &str, content: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join(file), content).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// `cce sync init` + `cce sync push` for a code repo (LFS off — hermetic).
fn init_and_push_code(home: &Path, url: &str, dir: &Path, repo_id: &str) {
    let out = cce(
        home,
        &[
            "sync",
            "init",
            "--remote",
            url,
            "--no-lfs",
            "--repo-id",
            repo_id,
            "--dir",
            dir.to_str().unwrap(),
        ],
    );
    assert_ok(&out, "sync init");
    let out = cce(home, &["sync", "push", "--dir", dir.to_str().unwrap()]);
    assert_ok(&out, "sync push");
}

/// The `modified` time of the consumer's installed knowledge snapshot.
fn knowledge_store_mtime(root: &Path, snapshot: &str) -> std::time::SystemTime {
    std::fs::metadata(knowledge_dir(root).join(format!("{snapshot}.json")))
        .unwrap()
        .modified()
        .unwrap()
}

/// §6, byte-pinned end to end: a cache seeded with fixed bytes — one code repo
/// plus two corpora (one fully published, one bare `.cck` with no pointer and
/// no corpus.json) — renders the EXACT human knowledge section and the EXACT
/// `cce.synclist/v1` + optional `knowledge` JSON, nullable fields as `null`.
#[test]
fn sync_list_knowledge_section_is_byte_pinned_human_and_json() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let corpus_meta = br#"{
  "chunk_count": 9,
  "corpus_id": "internal-tickets",
  "current": "9f1c2a3b4c5d6e7f",
  "data_as_of": "2026-07-01T09:00:00Z",
  "pushed_at": "2026-07-08T03:00:00Z",
  "records": 2,
  "schema": "cce.knowledgemeta/v1"
}
"#;
    seed_cache(
        &url,
        &[
            ("hash/2.3/aaa__one/1111111.cce", b"AA\n"),
            ("hash/2.3/aaa__one/refs/main", b"1111111\n"),
            ("knowledge/v1/internal-tickets/9f1c2a3b4c5d6e7f.cck", b"NEWCORPUS\n"),
            ("knowledge/v1/internal-tickets/0000000011111111.cck", b"OLD\n"),
            ("knowledge/v1/internal-tickets/current", b"9f1c2a3b4c5d6e7f\n"),
            ("knowledge/v1/internal-tickets/corpus.json", corpus_meta),
            ("knowledge/v1/runbooks/aaaabbbbccccdddd.cck", b"R\n"),
        ],
    );

    let consumer = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &["sync", "list", "--remote", &url, "--dir", consumer.path().to_str().unwrap()],
    );
    let human = assert_ok(&out, "sync list");
    let golden_human = format!(
        "remote        : {url}\n\
         \n\
         repo_id   latest   artifacts  bytes\n\
         aaa__one  1111111          1      3\n\
         \n\
         total         : 1 repo, 1 artifact, 3 bytes\n\
         \n\
         knowledge:\n\
         corpus_id         current           snapshots  bytes  data as-of\n\
         internal-tickets  9f1c2a3b4c5d6e7f          2     14  2026-07-01T09:00:00Z\n\
         runbooks          -                         1      2  -\n"
    );
    assert_eq!(human, golden_human);

    let out = cce(
        home.path(),
        &["sync", "list", "--remote", &url, "--json", "--dir", consumer.path().to_str().unwrap()],
    );
    let json = assert_ok(&out, "sync list --json");
    let golden_json = format!(
        r#"{{
  "knowledge": [
    {{
      "bytes": 14,
      "corpus_id": "internal-tickets",
      "current": "9f1c2a3b4c5d6e7f",
      "data_as_of": "2026-07-01T09:00:00Z",
      "pushed_at": "2026-07-08T03:00:00Z",
      "snapshots": 2
    }},
    {{
      "bytes": 2,
      "corpus_id": "runbooks",
      "current": null,
      "data_as_of": null,
      "pushed_at": null,
      "snapshots": 1
    }}
  ],
  "remote": "{url}",
  "repos": [
    {{
      "artifacts": 1,
      "bytes": 3,
      "latest_sha": "1111111",
      "repo_id": "aaa__one"
    }}
  ],
  "schema": "cce.synclist/v1"
}}
"#
    );
    assert_eq!(json, golden_json);
}

/// §6 both ways at the CLI level: a knowledge-free cache prints no knowledge
/// section (human OR JSON) — the pre-M5 bytes exactly — and a real
/// `knowledge push` makes the corpus appear with the real pointer + metadata.
#[test]
fn sync_list_knowledge_section_appears_only_when_a_corpus_exists() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    seed_cache(
        &url,
        &[
            ("hash/2.3/aaa__one/1111111.cce", b"AA\n"),
            ("hash/2.3/aaa__one/refs/main", b"1111111\n"),
        ],
    );

    // Knowledge-free: byte-identical to the pre-M5 listing (no knowledge key).
    let human = assert_ok(&cce(home.path(), &["sync", "list", "--remote", &url]), "list");
    assert!(!human.contains("knowledge:"), "got: {human}");
    let json = assert_ok(&cce(home.path(), &["sync", "list", "--remote", &url, "--json"]), "list");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v.get("knowledge").is_none(), "got: {json}");

    // A real push: the corpus appears, with the real current + corpus.json fields.
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );
    let json = assert_ok(&cce(home.path(), &["sync", "list", "--remote", &url, "--json"]), "list");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let k = &v["knowledge"][0];
    assert_eq!(k["corpus_id"], "fixture");
    assert_eq!(k["current"], snapshot);
    assert_eq!(k["snapshots"], 1);
    assert_eq!(k["data_as_of"], "2026-02-01T10:00:00Z");
    assert!(k["pushed_at"].as_str().unwrap().ends_with('Z'));
    assert!(k["bytes"].as_u64().unwrap() > 0);
    let human = assert_ok(&cce(home.path(), &["sync", "list", "--remote", &url]), "list");
    assert!(human.contains("\nknowledge:\n"), "got: {human}");
    assert!(human.contains("fixture"), "got: {human}");
}

/// §7: `pull --all` on a cache carrying code members AND a corpus gives the
/// consumer both — and the installed knowledge store, `current` pointer, and
/// sync marker are BYTE-IDENTICAL to a direct `cce knowledge pull`.
#[test]
fn pull_all_installs_the_corpus_byte_identical_to_a_direct_knowledge_pull() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("rocket.py", "def alpha_rocket_launch(thrust):\n    return thrust * 2\n");
    init_and_push_code(home.path(), &url, alpha.path(), "example.com__team__alpha");
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "knowledge push",
    );

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    let report = assert_ok(&out, "pull --all");
    assert!(
        report.contains(&format!("knowledge        pulled      fixture@{snapshot}")),
        "got: {report}"
    );
    assert!(ctx.join("alpha/.cce/index.json").exists(), "code member missing");
    assert!(knowledge_dir(&ctx).join(format!("{snapshot}.json")).exists(), "corpus missing");

    // The reference: a direct `knowledge pull` into a fresh root.
    let direct = tempfile::tempdir().unwrap();
    assert_ok(
        &cce(
            home.path(),
            &[
                "knowledge",
                "pull",
                "--corpus",
                "fixture",
                "--remote",
                &url,
                "--dir",
                direct.path().to_str().unwrap(),
            ],
        ),
        "direct knowledge pull",
    );
    for name in [format!("{snapshot}.json"), "current".to_string(), "synced.json".to_string()] {
        assert_eq!(
            std::fs::read(knowledge_dir(&ctx).join(&name)).unwrap(),
            std::fs::read(knowledge_dir(direct.path()).join(&name)).unwrap(),
            "{name} must be byte-identical to a direct knowledge pull"
        );
    }
}

/// §7 idempotent refresh, the member rule applied to knowledge: an unmoved
/// corpus `current` is `up-to-date` (nothing re-fetched, nothing re-written);
/// a moved `current` refreshes EXACTLY the corpus while up-to-date members
/// stay untouched.
#[test]
fn pull_all_knowledge_refresh_is_idempotent_and_moved_current_refreshes_only_the_corpus() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push_code(home.path(), &url, alpha.path(), "example.com__team__alpha");
    let producer = project_root(&url, "fixture", "all");
    let first = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let run = || {
        cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
        )
    };
    assert_ok(&run(), "first pull --all");
    let member_t1 =
        std::fs::metadata(ctx.join("alpha/.cce/index.json")).unwrap().modified().unwrap();
    let corpus_t1 = knowledge_store_mtime(&ctx, &first);

    // Second run: everything up-to-date; the corpus is NOT re-fetched.
    let report = assert_ok(&run(), "second pull --all");
    assert!(
        report.contains(&format!("knowledge        up-to-date  fixture@{first}")),
        "got: {report}"
    );
    assert!(
        report.contains("summary       : 0 pulled · 1 up-to-date · 0 skipped"),
        "got: {report}"
    );
    assert_eq!(
        knowledge_store_mtime(&ctx, &first),
        corpus_t1,
        "an unmoved current must not re-write"
    );

    // Move the corpus `current` (a newer snapshot) — the member stays put.
    let newer_feed = feed().replace("Password hashing policy", "Password hashing policy v2");
    let second = index_knowledge(home.path(), producer.path(), &newer_feed);
    assert_ne!(first, second);
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "re-push",
    );
    let report = assert_ok(&run(), "third pull --all");
    assert!(
        report.contains(&format!("knowledge        pulled      fixture@{second}")),
        "got: {report}"
    );
    assert!(report.contains("alpha            up-to-date"), "got: {report}");
    assert_eq!(
        std::fs::metadata(ctx.join("alpha/.cce/index.json")).unwrap().modified().unwrap(),
        member_t1,
        "an up-to-date member must not be re-written by a corpus refresh"
    );
    assert_eq!(
        std::fs::read_to_string(knowledge_dir(&ctx).join("current")).unwrap().trim(),
        second,
        "the consumer's current must follow the moved pointer"
    );
}

/// §7 corpus selection: with SEVERAL corpora and no flag the run warns and
/// skips knowledge — naming the ids — and never fails the member pulls;
/// `--corpus <id>` installs exactly the named one; an unknown id warns.
#[test]
fn pull_all_multi_corpus_warns_and_skips_and_corpus_flag_selects() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push_code(home.path(), &url, alpha.path(), "example.com__team__alpha");
    for corpus in ["tickets", "runbooks"] {
        let producer = project_root(&url, corpus, "all");
        index_knowledge(home.path(), producer.path(), &feed());
        assert_ok(
            &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
            "push",
        );
    }

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    let report = assert_ok(&out, "pull --all (multi-corpus)");
    assert!(
        report.contains(
            "warning: skipped knowledge — the cache carries 2 corpora (runbooks, tickets)"
        ),
        "got: {report}"
    );
    assert!(report.contains("pass --corpus <id>"), "got: {report}");
    assert!(report.contains("summary       : 1 pulled"), "member pulls must succeed: {report}");
    assert!(!knowledge_dir(&ctx).exists(), "skipped knowledge must install nothing");

    // An unknown corpus id: warned, never fatal.
    let out = cce(
        home.path(),
        &[
            "sync",
            "pull",
            "--all",
            "--into",
            ctx.to_str().unwrap(),
            "--remote",
            &url,
            "--corpus",
            "nope",
        ],
    );
    let report = assert_ok(&out, "pull --all --corpus nope");
    assert!(
        report.contains("warning: skipped knowledge — corpus `nope` is not in the cache"),
        "got: {report}"
    );

    // --corpus selects: exactly the named corpus installs.
    let out = cce(
        home.path(),
        &[
            "sync",
            "pull",
            "--all",
            "--into",
            ctx.to_str().unwrap(),
            "--remote",
            &url,
            "--corpus",
            "runbooks",
        ],
    );
    let report = assert_ok(&out, "pull --all --corpus runbooks");
    assert!(report.contains("knowledge        pulled      runbooks@"), "got: {report}");
    let marker: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(knowledge_dir(&ctx).join("synced.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["corpus_id"], "runbooks");
}

/// §7: `verify --checksum-only` covers the pulled knowledge store — a pass row
/// naming the corpus; a byte-flip fails LOUDLY naming the corpus (with the
/// sharpened no-escalation caveat); a marker without `installed_sha256` is an
/// explicit notice with exit 0.
#[test]
fn verify_checksum_only_knowledge_row_pass_flip_fail_and_markerless_notice() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push_code(home.path(), &url, alpha.path(), "example.com__team__alpha");
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );
    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    assert_ok(
        &cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
        ),
        "pull --all",
    );

    // Intact: the knowledge row passes beside the member row.
    let out =
        cce(home.path(), &["sync", "verify", "--checksum-only", "--dir", ctx.to_str().unwrap()]);
    let stdout = assert_ok(&out, "verify intact");
    assert!(stdout.contains("verify OK (checksum-only): 1 member"), "got: {stdout}");
    assert!(stdout.contains(&format!("knowledge        fixture@{snapshot}")), "got: {stdout}");

    // Flip one byte in the pulled knowledge store: a loud failure naming the corpus.
    let store = knowledge_dir(&ctx).join(format!("{snapshot}.json"));
    let intact = std::fs::read(&store).unwrap();
    let mut bytes = intact.clone();
    let idx = bytes.iter().position(|&b| b == b'a').unwrap();
    bytes[idx] = b'b';
    std::fs::write(&store, &bytes).unwrap();
    let out =
        cce(home.path(), &["sync", "verify", "--checksum-only", "--dir", ctx.to_str().unwrap()]);
    assert!(!out.status.success(), "a corrupted knowledge store must fail verify");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("verify FAILED (checksum-only) for knowledge corpus `fixture`"),
        "got: {stderr}"
    );
    assert!(stderr.contains("NO rebuild-verify escalation path"), "got: {stderr}");

    // Restore the store; strip installed_sha256 (an older-cce marker) ⇒ notice, exit 0.
    std::fs::write(&store, &intact).unwrap();
    let marker_path = knowledge_dir(&ctx).join("synced.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&marker_path).unwrap()).unwrap();
    assert!(v.as_object_mut().unwrap().remove("installed_sha256").is_some());
    std::fs::write(&marker_path, v.to_string()).unwrap();
    let out =
        cce(home.path(), &["sync", "verify", "--checksum-only", "--dir", ctx.to_str().unwrap()]);
    let stdout = assert_ok(&out, "verify with an old marker is a notice, not a failure");
    assert!(
        stdout.contains("knowledge        no install checksum recorded (pulled by an older cce)"),
        "got: {stdout}"
    );
    assert!(stdout.contains("re-pull with `cce knowledge pull`"), "got: {stdout}");
}

/// §7: a root with ONLY a pulled corpus (no code store, no manifest) still
/// verifies — and a knowledge-free root verifies exactly as today.
#[test]
fn verify_checksum_only_on_a_knowledge_only_root() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );
    let consumer = tempfile::tempdir().unwrap();
    assert_ok(
        &cce(
            home.path(),
            &[
                "knowledge",
                "pull",
                "--corpus",
                "fixture",
                "--remote",
                &url,
                "--dir",
                consumer.path().to_str().unwrap(),
            ],
        ),
        "knowledge pull",
    );
    let out = cce(
        home.path(),
        &["sync", "verify", "--checksum-only", "--dir", consumer.path().to_str().unwrap()],
    );
    let stdout = assert_ok(&out, "knowledge-only verify");
    assert!(
        stdout.contains(&format!("verify OK (checksum-only): knowledge corpus fixture@{snapshot}")),
        "got: {stdout}"
    );

    // A root with neither marker keeps the original clear error.
    let empty = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &["sync", "verify", "--checksum-only", "--dir", empty.path().to_str().unwrap()],
    );
    assert_err(&out, "nothing to verify", "verify on an empty root");
}

/// §4.4: MCP `index_status` gains the knowledge block with the exact pinned
/// fields — and stays byte-free of it when no knowledge store exists. The
/// remote lines are best-effort: reachable ⇒ `remote current` + behind-status;
/// a moved remote pointer flips `behind remote` to the actionable `yes`.
#[test]
fn mcp_index_status_knowledge_block_is_pinned_and_absent_without_a_store() {
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push_code(home.path(), &url, alpha.path(), "example.com__team__alpha");
    let producer = project_root(&url, "fixture", "all");
    let snapshot = index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "push",
    );

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    assert_ok(
        &cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
        ),
        "pull --all",
    );

    // The exact §4.4 block, byte-pinned (records/chunks read from the store).
    let store: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(knowledge_dir(&ctx).join(format!("{snapshot}.json"))).unwrap(),
    )
    .unwrap();
    let (records, chunks) =
        (store["records"].as_u64().unwrap(), store["chunks"].as_array().unwrap().len());
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n";
    let text = tool_text(
        &drive_env(
            &["mcp", "--workspace", "--dir", ctx.to_str().unwrap()],
            input,
            &[("CCE_HOME", &home_str)],
        ),
        1,
    );
    let expected = format!(
        "  knowledge :\n    corpus         : fixture\n    snapshot       : {snapshot}\n    \
         records/chunks : {records} / {chunks}\n    data as-of     : 2026-02-01T10:00:00Z\n    \
         remote current : {snapshot}\n    behind remote  : no\n"
    );
    assert!(text.contains(&expected), "expected pinned block:\n{expected}\nin:\n{text}");

    // The remote pointer moves ⇒ behind remote flips to the actionable `yes`.
    let newer = feed().replace("Password hashing policy", "Password hashing policy v2");
    let second = index_knowledge(home.path(), producer.path(), &newer);
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "re-push",
    );
    let text = tool_text(
        &drive_env(
            &["mcp", "--workspace", "--dir", ctx.to_str().unwrap()],
            input,
            &[("CCE_HOME", &home_str)],
        ),
        1,
    );
    assert!(text.contains(&format!("remote current : {second}")), "got: {text}");
    assert!(text.contains("behind remote  : yes — run `cce knowledge pull`"), "got: {text}");

    // Without a knowledge store the report carries NO knowledge block at all.
    let bare_consumer = tempfile::tempdir().unwrap();
    let ctx2 = bare_consumer.path().join("ctx");
    let (_bare2, url2) = bare_remote();
    let beta = tiny_repo("b.py", "def beta_two():\n    return 2\n");
    init_and_push_code(home.path(), &url2, beta.path(), "example.com__team__beta");
    assert_ok(
        &cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx2.to_str().unwrap(), "--remote", &url2],
        ),
        "pull --all (knowledge-free)",
    );
    let text = tool_text(
        &drive_env(
            &["mcp", "--workspace", "--dir", ctx2.to_str().unwrap()],
            input,
            &[("CCE_HOME", &home_str)],
        ),
        1,
    );
    assert!(!text.contains("knowledge :"), "knowledge-free status must be unchanged: {text}");
}

/// The whole M5 promise in one test (§7 acceptance): a BARE directory →
/// `cce sync pull --all` → `cce mcp --workspace` → `context_search source:
/// both` blends a code hit and the knowledge hit with its full provenance —
/// no source checkout, no adapter, no prior state anywhere.
#[test]
fn end_to_end_bare_dir_pull_all_then_context_search_both_blends_knowledge_with_provenance() {
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let (_bare, url) = bare_remote();
    // A code repo whose content shares the query vocabulary…
    let app = tiny_repo(
        "auth.py",
        "def hash_password(password):\n    return slow_salted_hash(password)\n",
    );
    init_and_push_code(home.path(), &url, app.path(), "example.com__team__auth");
    // …and the corpus carrying the WHY (the policy record from `feed()`).
    let producer = project_root(&url, "fixture", "all");
    index_knowledge(home.path(), producer.path(), &feed());
    assert_ok(
        &cce(home.path(), &["knowledge", "push", "--dir", producer.path().to_str().unwrap()]),
        "knowledge push",
    );

    // The consumer story: one command from a bare directory.
    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    assert_ok(
        &cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
        ),
        "pull --all",
    );

    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"password hashing policy\",\"source\":\"both\",\"no_graph\":true}}}\n";
    let text = tool_text(
        &drive_env(
            &["mcp", "--workspace", "--dir", ctx.to_str().unwrap()],
            input,
            &[("CCE_HOME", &home_str)],
        ),
        1,
    );
    // The knowledge hit, with the byte-pinned provenance grammar
    // (`[knowledge] <title> — <state> · <updated_at> · <url>`).
    assert!(
        text.contains("[knowledge] Password hashing policy — closed · 2026-02-01T10:00:00Z · https://example.test/1"),
        "missing the knowledge hit with provenance: {text}"
    );
    // Blended with a code hit from the pulled member.
    assert!(text.contains("auth.py"), "missing the blended code hit: {text}");
}
