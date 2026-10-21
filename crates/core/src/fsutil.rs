//! Atomic file writes — the one filesystem primitive the kernel's own
//! persistence paths (session save, write_file / edit_file) share with the
//! CLI-side sinks (result.json / report.json). It lives in the kernel crate
//! (not next to the harness that once grew it) because session.rs and
//! tools/file.rs are its callers; the CLI reaches it through the same
//! public module.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

/// Monotonic per-process suffix for atomic_write temp names. Module-level
/// (not inside the fn) so tests can predict the next allocation.
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `<target>.<pid>.<n>.tmp` sibling of `path` — same directory, so the final
/// rename stays on one filesystem.
fn tmp_name(path: &Path, n: u64) -> std::path::PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut base = name.to_os_string();
            base.push(format!(".{}.{}.tmp", std::process::id(), n));
            path.with_file_name(base)
        }
        None => path.with_extension("tmp"),
    }
}

/// Move `tmp` onto `path`: the cfg split IS the fix. POSIX `rename(2)`
/// replaces an existing target on permission bits alone (only the
/// DIRECTORY's write permission matters), so the non-Windows arm is the
/// plain rename and stays exactly that. Windows `MoveFileExW(
/// REPLACE_EXISTING)` — what std::fs::rename lowers to — has two failure
/// faces posix never shows:
///
/// (a) the target carries the read-only attribute → ERROR_ACCESS_DENIED.
///     Unix rename overwrites a read-only file without a second thought;
///     rather than fork the observable behavior per-OS at every call site,
///     we clear the target's bit here and retry — explicitly aligning
///     Windows with the semantics the callers were written against.
/// (b) an AV scanner / search indexer / editor holds the target without
///     FILE_SHARE_DELETE → ERROR_SHARING_VIOLATION, characteristically
///     transient (clears within a few tens of ms).
///
/// Both get a short bounded backoff (3 tries at 50-100ms); every other
/// error still fails fast, unchanged.
#[cfg(windows)]
// Windows-only code: the lint's unix world-writable footgun can't apply.
#[allow(clippy::permissions_set_readonly_false)]
fn rename_into_place(tmp: &Path, path: &Path) -> std::io::Result<()> {
    // winerror.h values, named so the retry set reads as intent.
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(first) => {
            let retryable = matches!(
                first.raw_os_error(),
                Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
            );
            if !retryable {
                return Err(first);
            }
            // Face (a): clear the target's read-only bit before retrying.
            // Best-effort — if the clear fails too (say the file is also
            // held with no sharing), the retries below still run and the
            // reported error stays the rename's own. The attribute is NOT
            // lost on success: the tmp inherited it (permission-preserving
            // copy in atomic_write), and the rename moves the tmp's
            // attributes onto the target — overwriting keeps the read-only
            // bit, just as unix keeps the replaced file's mode.
            if first.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                if let Ok(meta) = std::fs::metadata(path) {
                    let mut perms = meta.permissions();
                    if perms.readonly() {
                        perms.set_readonly(false);
                        let _ = std::fs::set_permissions(path, perms);
                    }
                }
            }
            let mut last = first;
            for delay_ms in [50, 100, 100] {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                match std::fs::rename(tmp, path) {
                    Ok(()) => return Ok(()),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
    }
}

/// See [`rename_into_place`]: posix rename needs no papering-over, so the
/// non-Windows arm is the plain rename — byte-for-byte the old behavior.
#[cfg(not(windows))]
fn rename_into_place(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, path)
}

/// Failure-path tmp cleanup. On Windows a tmp that INHERITED the target's
/// read-only attribute cannot be removed as-is — deletion is denied for a
/// read-only file — and the old bare `let _ = remove_file(&tmp)` swallowed
/// that failure whole, littering a stray READ-ONLY .tmp that anything
/// globbing the directory (or a later cleanup sweep) then trips over just
/// the same. Clear the bit first, then remove.
#[cfg(windows)]
// Windows-only code: the lint's unix world-writable footgun can't apply.
#[allow(clippy::permissions_set_readonly_false)]
fn remove_tmp_file(tmp: &Path) {
    if let Ok(meta) = std::fs::metadata(tmp) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(tmp, perms);
        }
    }
    let _ = std::fs::remove_file(tmp);
}

/// Unix unlink ignores the file's own permission bits (the directory's
/// write bit is what counts), so a read-only tmp removes fine as-is.
#[cfg(not(windows))]
fn remove_tmp_file(tmp: &Path) {
    let _ = std::fs::remove_file(tmp);
}

/// Atomically write bytes to `path`: write a temp file in the same
/// directory, then rename. A crash mid-write never leaves a half-file —
/// result.json and report.json are read by CI, so they must not corrupt.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // The pid + counter suffix avoids collisions between concurrent writers
    // (batch workers sharing a workdir), but it is NOT unpredictable — a
    // hostile workdir can pre-place a symlink at the exact predicted name —
    // so the open is O_EXCL (`create_new`): it fails on ANY pre-existing
    // path, symlinks included (O_EXCL never follows them), where plain
    // `File::create` would write THROUGH the link and redirect the write
    // outside the workdir (write_file / edit_file land here after their own
    // target-path sandbox check, which can't see the temp sibling). On
    // collision the counter advances and a fresh suffix is tried; a stale
    // tmp from a pid-reused predecessor process is skipped the same way.
    for _ in 0..8 {
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = tmp_name(path, n);
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", tmp.display())),
        };
        // Write + fsync the temp file before renaming, so a crash after rename
        // never exposes a zero-length file. (result.json / report.json are read
        // by CI and must not corrupt on power loss.) A failure anywhere below
        // removes the temp file first — the rename branch already did, but a
        // write/sync failure used to leave the stray .tmp behind (visible to
        // anything globbing the directory, e.g. the next batch round's task
        // scan).
        let written = (|| -> Result<()> {
            // Preserve the existing target's permissions — above all the
            // executable bit: a fresh default-mode file would silently strip
            // -x from an edited script. Best-effort no-op when the target
            // doesn't exist yet (write_file creating a new file).
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = f.set_permissions(meta.permissions());
            }
            f.write_all(bytes)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("cannot sync {}", tmp.display()))?;
            Ok(())
        })();
        if let Err(e) = written {
            remove_tmp_file(&tmp);
            return Err(e);
        }
        if let Err(e) = rename_into_place(&tmp, path) {
            remove_tmp_file(&tmp); // don't litter on failure
            return Err(e).with_context(|| format!("cannot save {}", path.display()));
        }
        // fsync the parent directory so the rename's directory-entry update
        // survives a power loss. The file DATA is durable (sync_all above), but
        // on ext4 (when auto_da_alloc doesn't fire) and xfs the rename itself
        // can be lost without an explicit directory fsync — a crash could then
        // expose the pre-rename state (old content, or no file). APFS/NTFS
        // handle directory durability via their own transaction logs, so this
        // is Unix-only and the error is ignored: best-effort hardening, and
        // the rename already succeeded.
        #[cfg(unix)]
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        return Ok(());
    }
    // Every candidate name was already taken — either a stale-tmp pile-up or
    // a hostile directory planted across the whole name window.
    Err(anyhow::anyhow!(
        "cannot create a temp file next to {} (all candidate names already exist)",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrips_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("ta-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        atomic_write(&path, b"{\"ok\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"ok\":true}");
        // The temp file must be gone after the rename (the name is now unique
        // per call, so scan the dir for any *.tmp leftover rather than a fixed
        // name).
        let no_tmp = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(no_tmp, "temp file must be gone after rename");
        // Overwriting an existing file is also atomic.
        atomic_write(&path, b"v2").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ta-atomic-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.sh");
        // Create the target with the executable bit set.
        std::fs::write(&path, b"#!/bin/sh\necho old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // atomic_write must carry the +x through to the new file.
        atomic_write(&path, b"#!/bin/sh\necho new\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "executable bit lost after atomic_write: mode {mode:o}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho new\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_skips_preplaced_symlink_at_predicted_tmp_name() {
        // P1-1 regression: write_file/edit_file sandbox-check the TARGET path,
        // but the temp sibling name is predictable (pid + counter). A
        // pre-placed symlink at that exact name must NOT be written through —
        // the O_EXCL (`create_new`) open fails on the existing link and the
        // write retries on a fresh suffix instead of redirecting outside the
        // workdir. With plain `File::create` the victim received the bytes.
        let dir = std::env::temp_dir().join(format!("ta-atomic-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.json");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        // Cover a few consecutive counter values: a parallel test in this
        // process may bump TMP_COUNTER between our read and the write under
        // test.
        let n = TMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        for k in n..n + 3 {
            std::os::unix::fs::symlink(&victim, tmp_name(&target, k)).unwrap();
        }
        atomic_write(&target, b"{\"ok\":true}").unwrap();
        // The write landed on the target…
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"ok\":true}");
        // …and never went THROUGH any planted link: the victim is untouched
        // and the link itself still is one.
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert!(
            std::fs::symlink_metadata(tmp_name(&target, n))
                .unwrap()
                .file_type()
                .is_symlink(),
            "planted symlink must not be replaced or followed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_skips_stale_tmp_file_at_predicted_name() {
        // The pid-reuse case: a crashed predecessor process with our pid can
        // leave a stale `<target>.<pid>.<n>.tmp` behind. O_EXCL must skip it
        // (write to a fresh suffix) rather than truncate-write a name someone
        // else owns.
        let dir = std::env::temp_dir().join(format!("ta-atomic-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.json");
        let n = TMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        let stale = tmp_name(&target, n);
        std::fs::write(&stale, "stale junk from a pid-reused predecessor").unwrap();
        atomic_write(&target, b"fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fresh");
        // The stale file is inert garbage — skipped, not written through.
        assert!(
            std::fs::read_to_string(&stale)
                .unwrap()
                .starts_with("stale junk"),
            "stale tmp must not be truncated and reused"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_creates_missing_parent_dirs() {
        let base = std::env::temp_dir().join(format!("ta-atomic-parent-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        // Neither `a/b` nor `a` exists — atomic_write must mkdir -p them (the
        // create_dir_all branch the basic test skips by pre-making the dir).
        let path = base.join("a/b/out.json");
        atomic_write(&path, b"ok").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok");
        assert!(!base.join("a/b/out.tmp").exists(), "temp file left behind");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Scan `dir` for any leftover `<target>...tmp` sibling — the residue
    /// both failure-path bugs left behind.
    #[cfg(windows)]
    fn assert_no_tmp_left(dir: &Path) {
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray tmp files: {leftovers:?}");
    }

    /// Windows face (a): a read-only target makes MoveFileEx(REPLACE_EXISTING)
    /// fail with ACCESS_DENIED where posix rename would replace the file. The
    /// fallback clears the bit and retries, so the overwrite goes through —
    /// with no stray .tmp either (the tmp itself is read-only at that point:
    /// it inherited the target's attribute, so the cleanup path must clear
    /// before remove or it litters).
    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)] // Windows-only: the
                                                     // lint's unix world-writable footgun can't apply.
    fn atomic_write_overwrites_readonly_target_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("ta-atomic-ro-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        std::fs::write(&path, b"old").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // Attribute carried through the overwrite (the tmp inherited it and
        // the rename moves the tmp's attributes onto the target), mirroring
        // how unix keeps the mode of a replaced file.
        assert!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            "read-only attribute lost after atomic_write"
        );
        assert_no_tmp_left(&dir);
        // Cleanup: clear the bit again or remove_dir_all fails on the file.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Windows face (b): a target held without FILE_SHARE_DELETE (the AV /
    /// indexer / editor shape) makes the rename fail with
    /// ERROR_SHARING_VIOLATION. The backoff retries must ride out a lock
    /// that clears inside the retry window (retries at ~50/150/250ms)
    /// instead of failing the save outright.
    #[cfg(windows)]
    #[test]
    fn atomic_write_retries_through_transient_target_lock() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("ta-atomic-lock-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        std::fs::write(&path, b"old").unwrap();
        // share_mode(0): grant no FILE_SHARE_* at all — the rename's
        // DELETE-open of the target fails with ERROR_SHARING_VIOLATION (32)
        // until the handle drops.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let target = path.clone();
        let writer = std::thread::spawn(move || atomic_write(&target, b"new").unwrap());
        // Release inside the retry window.
        std::thread::sleep(Duration::from_millis(150));
        drop(lock);
        writer
            .join()
            .expect("atomic_write must survive a transient target lock");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_no_tmp_left(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A lock that NEVER clears must end in a clean Err after the bounded
    /// retries — with the tmp cleaned up. The target is ALSO read-only here,
    /// so (when the write-phase metadata peek succeeds through the lock) the
    /// tmp inherits the attribute and the cleanup's clear-before-remove is
    /// the difference between "gone" and "stray read-only .tmp".
    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)] // Windows-only: the
                                                     // lint's unix world-writable footgun can't apply.
    fn atomic_write_fails_cleanly_when_target_stays_locked() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("ta-atomic-lock2-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        std::fs::write(&path, b"old").unwrap();
        // Read-only first (a read-open of a read-only file is still allowed),
        // THEN locked with no sharing — set_permissions on a locked file
        // can't succeed, so the order matters.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let err = atomic_write(&path, b"new").unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot save"),
            "unexpected error: {err:#}"
        );
        // The tmp must be gone after the failed attempt, not littered.
        assert_no_tmp_left(&dir);
        // Target untouched by the failed write. Drop the lock before reading.
        drop(lock);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
