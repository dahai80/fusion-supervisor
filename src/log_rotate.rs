// issue #168: size-based rotation for the launchd daemon log (copy-truncate).
// daemon.launchd.log is a launchd StandardOutPath redirect — launchd holds the fd:
// rename would make launchd keep writing into the rotated-away inode (current log
// becomes a dead file), and external redirection / tracing-appender cannot take over
// (the latter only does time-based rotation). copy-truncate: copy a snapshot to .1,
// truncate in place via set_len(0) — launchd keeps appending to the same inode, new
// log starts from zero. Lines written between copy and truncate are lost (standard
// copytruncate tradeoff); rotation triggers at a GB-scale threshold, frequency is
// on the order of days, acceptable. Backups are named <file>.1 .. <file>.N
// (logrotate convention); beyond N they are implicitly evicted by later renames.
use std::path::{Path, PathBuf};

// Rotation decision (pure function): size over threshold and rotation not disabled (max_bytes > 0).
pub fn should_rotate(size_bytes: u64, max_bytes: u64) -> bool {
    max_bytes > 0 && size_bytes > max_bytes
}

// Backup chain shift plan: (from, to) index pairs, freeing slots from the oldest end.
// keep=3 → [(2,3), (1,2)]: .2→.3 (evicting old .3), .1→.2; then the current file is copied to .1.
// keep < 2 → no backup chain (0/1 = keep no backups). Pure function, testable.
pub fn backup_shifts(keep: u32) -> Vec<(u32, u32)> {
    if keep < 2 {
        return Vec::new();
    }
    (1..keep).rev().map(|i| (i, i + 1)).collect()
}

// Backup path for the n-th slot: daemon.launchd.log → daemon.launchd.log.1 (numeric suffix appended).
pub fn backup_path(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

// Single-file rotation executor. Returns whether a rotation actually happened; a missing
// file means nothing to rotate (the err log may not exist) — silently returns false without
// error. Heavy work (rename/copy/truncate) only runs at threshold; one metadata stat per tick.
pub fn rotate_file(path: &Path, max_bytes: u64, keep: u32) -> anyhow::Result<bool> {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if !should_rotate(size, max_bytes) {
        return Ok(false);
    }
    for (from, to) in backup_shifts(keep) {
        let src = backup_path(path, from);
        if src.exists() {
            std::fs::rename(&src, backup_path(path, to))?;
        }
    }
    if keep >= 1 {
        std::fs::copy(path, backup_path(path, 1))?;
    }
    // Truncate the original inode (reopen the same file launchd holds — never rename)
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_len(0)?;
    tracing::info!(
        path = %path.display(),
        rotated_mb = size / (1024 * 1024),
        keep,
        "daemon log rotated (copy-truncate)"
    );
    Ok(true)
}

// Path resolution: ~ expansion (same as config) → relative paths prefer FUSION_ROOT
// (launchd plist WorkingDirectory is the monorepo root; default paths like
// "fusion-supervisor/logs/..." are root-relative), falls back to CWD without FUSION_ROOT.
pub fn resolve_rotate_path(raw: &str) -> PathBuf {
    let p = crate::config::expand_tilde(Path::new(raw));
    if p.is_absolute() {
        return p;
    }
    match std::env::var("FUSION_ROOT") {
        Ok(root) if !root.trim().is_empty() => PathBuf::from(root).join(p),
        _ => p,
    }
}

// Per-tick check entry: rotate each configured path in turn. A single-file failure logs a
// WARN and continues with the next (rotation is hygiene; it must never take down the loop).
pub fn check_paths(paths: &[String], max_mb: u64, keep: u32) -> usize {
    if max_mb == 0 {
        return 0;
    }
    let max_bytes = max_mb.saturating_mul(1024 * 1024);
    let mut rotated = 0;
    for raw in paths {
        match rotate_file(&resolve_rotate_path(raw), max_bytes, keep) {
            Ok(true) => rotated += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(path = %raw, err = %e, "log rotate failed");
            }
        }
    }
    rotated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_rotate_threshold_and_disable() {
        assert!(!should_rotate(0, 100), "empty file never rotates");
        assert!(
            !should_rotate(100, 100),
            "at-threshold does not rotate (strictly greater)"
        );
        assert!(should_rotate(101, 100), "over-threshold rotates");
        assert!(!should_rotate(u64::MAX, 0), "max=0 disables rotation");
    }

    #[test]
    fn test_backup_shifts_plan() {
        assert_eq!(backup_shifts(3), vec![(2, 3), (1, 2)]);
        assert_eq!(backup_shifts(2), vec![(1, 2)]);
        assert!(
            backup_shifts(1).is_empty(),
            "keep=1 shifts nothing, only copies .1"
        );
        assert!(backup_shifts(0).is_empty());
    }

    #[test]
    fn test_backup_path_suffix() {
        assert_eq!(
            backup_path(Path::new("/logs/daemon.launchd.log"), 2),
            PathBuf::from("/logs/daemon.launchd.log.2")
        );
    }

    #[test]
    fn test_rotate_file_copy_truncate_and_retention() {
        let dir = std::env::temp_dir().join(format!("fsv-logrot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("test.log");
        // Round 1: over threshold (40 > 32) → .1 created, original truncated to empty
        std::fs::write(&log, "x".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            0,
            "original file truncated"
        );
        assert_eq!(std::fs::read(dir.join("test.log.1")).unwrap().len(), 40);
        // Round 2: over threshold again → old .1 shifts to .2, new snapshot lands in .1
        std::fs::write(&log, "y".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(dir.join("test.log.2")).unwrap().len(), 40);
        assert_eq!(std::fs::read(dir.join("test.log.1")).unwrap().len(), 40);
        // Round 3: keep=2 cap → .2 is overwritten by the next round's rename, only .1 + .2 ever exist
        std::fs::write(&log, "z".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(dir.join("test.log.2")).unwrap().len(), 40);
        assert!(
            !dir.join("test.log.3").exists(),
            "keep cap never produces .3"
        );
        // Under threshold: no rotation
        std::fs::write(&log, "w".repeat(10)).unwrap();
        assert!(!rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(&log).unwrap().len(), 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_rotate_file_missing_and_disabled() {
        let missing =
            std::env::temp_dir().join(format!("fsv-logrot-absent-{}", std::process::id()));
        assert!(
            !rotate_file(&missing, 32, 2).unwrap(),
            "missing file skipped silently"
        );
        let dir = std::env::temp_dir().join(format!("fsv-logrot-dis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("d.log");
        std::fs::write(&log, "x".repeat(64)).unwrap();
        assert!(!rotate_file(&log, 0, 2).unwrap(), "max=0 disables rotation");
        assert_eq!(
            std::fs::read(&log).unwrap().len(),
            64,
            "file untouched while disabled"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_rotate_path_absolute_and_tilde() {
        assert_eq!(
            resolve_rotate_path("/var/log/x.log"),
            PathBuf::from("/var/log/x.log"),
            "absolute path passes through unchanged"
        );
        // ~ expansion depends on host HOME — assert shape only, no hardcoded path
        let p = resolve_rotate_path("~/logs/x.log");
        assert!(p.is_absolute() && !p.to_string_lossy().starts_with('~'));
    }
}
