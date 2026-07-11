//! # atomic — crash-safe file writes (temp-file + fsync + rename)
//!
//! **Why this file exists:** `std::fs::write` truncates the destination *before*
//! writing, so a crash, `SIGKILL`, OOM, or disk-full mid-write destroys the
//! previous good file and can expose a truncated/0-byte file to a concurrent
//! reader (#101). The `.cce` store (`index.json`, the knowledge snapshots, the
//! fingerprint) is read by long-lived processes (the MCP server, `sync push`,
//! `stats`) while another process re-indexes, so a torn read is a live bug.
//!
//! **What it is / does:** [`atomic_write`] stages the bytes to a uniquely-named
//! temp file in the SAME directory as the destination (`rename(2)` is only atomic
//! within a single filesystem — a system temp dir is often a different mount),
//! flushes and fsyncs it, then `rename`s it over the destination. A reader
//! therefore observes either the old complete file or the new complete file,
//! never a partial one, and an interrupted write leaves the previous file intact.
//! Any error removes the temp file, so a failed write never leaves a stray. This
//! mirrors the staged-then-`rename` idiom already used for the self-update binary
//! (`update::replace_binary`).

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic, process-local counter so two concurrent `atomic_write`s to the same
/// destination from the same process pick distinct temp names — the pid alone
/// would collide. A counter (not `rand`) keeps the temp name reproducible.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically: stage to a uniquely-named temp file in the
/// SAME directory, flush + `fsync` it, then `rename` it over `path`. On any
/// failure the temp file is removed and `path` is left untouched. The destination
/// is thus never a truncated or 0-byte file, and a crash cannot destroy the
/// previous good file. The caller is responsible for creating parent directories.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // The temp file MUST live in the destination's directory: `rename(2)` is only
    // atomic within one filesystem. An empty/absent parent means the cwd.
    let dir: PathBuf = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    // Base the temp name on the destination's file name so several files staged in
    // the same directory never collide, and tag it with pid + a process-local
    // sequence so two concurrent saves of the SAME file get distinct temp paths.
    let stem = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.tmp.{}.{seq}", std::process::id()));

    // Stage the full bytes and durably flush them BEFORE the rename, so the file
    // the rename publishes is already complete on disk.
    let staged = (|| -> io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // temp-file + rename creates the staged file at the umask default and would
    // DROP the destination's existing mode, silently widening a user-tightened
    // store (e.g. `chmod 600 index.json`) back to 0o644 — a regression vs main's
    // write-through `std::fs::write`. So if the destination already exists, carry
    // its mode over to the temp BEFORE the rename (the sibling `replace_binary`
    // set_permissions after a temp copy for the same reason). A fresh file keeps
    // the umask default, matching main's behaviour on a new store. Mode bits are a
    // unix concept; on other platforms rename-preserves-nothing is left as-is.
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        if let Err(e) = std::fs::set_permissions(&tmp, meta.permissions()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }

    // The atomic publish. If it fails, clean up the stray temp file and leave the
    // previous destination untouched.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_strays(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect()
    }

    #[test]
    fn overwrites_with_exactly_the_new_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("index.json");
        std::fs::write(&dest, b"old contents").unwrap();
        atomic_write(&dest, b"brand new bytes").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"brand new bytes");
    }

    #[test]
    fn writes_a_fresh_file_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("sub").join("index.json");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        atomic_write(&dest, b"payload").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
    }

    #[test]
    fn leaves_no_temp_file_after_a_successful_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("index.json");
        atomic_write(&dest, b"a").unwrap();
        atomic_write(&dest, b"bb").unwrap();
        // Only the destination remains; the temp file was renamed away.
        assert!(temp_strays(tmp.path()).is_empty(), "a stray temp file was left behind");
        let entries: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["index.json".to_string()]);
    }

    // The core of #101: a write that FAILS must leave the previous good file
    // byte-for-byte intact and must not truncate it to 0 bytes — the exact
    // property `std::fs::write` (open-with-truncate) does NOT have. We force the
    // failure by making the destination's directory unwritable, so staging the
    // temp file cannot even begin. (Skipped for root, which ignores mode bits.)
    #[cfg(unix)]
    #[test]
    fn a_failed_write_preserves_the_existing_destination() {
        use std::os::unix::fs::PermissionsExt;
        // Running as root bypasses permission bits, so the failure can't be forced.
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join("index.json");
        std::fs::write(&dest, b"PREVIOUS GOOD STORE").unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = atomic_write(&dest, b"new bytes that must not land");
        // Restore write permission so the tempdir can be cleaned up regardless.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "staging into an unwritable dir must fail");
        // The previous good file survives, byte-for-byte — never truncated/0-byte.
        assert_eq!(std::fs::read(&dest).unwrap(), b"PREVIOUS GOOD STORE");
        // And no half-written temp file was left behind.
        assert!(temp_strays(&dir).is_empty(), "a failed write left a stray temp file");
    }

    // An atomic re-save must PRESERVE the destination's existing mode: temp-file +
    // rename would otherwise drop a user-tightened `chmod 600 index.json` back to
    // the umask default (0o644), silently widening a private store — a regression
    // vs main's write-through `std::fs::write`. (Skipped as root: mode bits ignored.)
    #[cfg(unix)]
    #[test]
    fn re_save_preserves_the_destination_mode() {
        use std::os::unix::fs::PermissionsExt;
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("index.json");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write(&dest, b"new bytes").unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new bytes");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the user-tightened store mode must survive a re-save");
    }

    // Control: a FRESH atomic_write (no pre-existing dest) keeps the umask default,
    // matching main's `std::fs::write` on a new file (0o644 under the usual 022).
    #[cfg(unix)]
    #[test]
    fn fresh_write_uses_the_umask_default_mode() {
        use std::os::unix::fs::PermissionsExt;
        extern "C" {
            fn geteuid() -> u32;
            fn umask(mask: u32) -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        // Pin the umask to the conventional 022 for the duration of the test so the
        // expected 0o644 is deterministic regardless of the caller's environment,
        // then restore it.
        let prev = unsafe { umask(0o022) };
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("index.json");
        atomic_write(&dest, b"fresh").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        unsafe { umask(prev) };
        assert_eq!(mode, 0o644, "a fresh store must keep the umask-default mode, like main");
    }
}
