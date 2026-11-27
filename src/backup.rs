// issue #10: NAS backup primitives. Postgres (pg_dumpall → gzip) + Qdrant (storage dir snapshot) → FUSION_BACKUP_TARGET.
// Design: injectable ExecFn for testability (prod=shell docker compose exec, test=fake that never calls docker).
// fail-closed: FUSION_BACKUP_TARGET unset → refuse + BackupFailed alert (prod safety tasks never skip silently).
// Retention: keep backup_retention copies, prune old backups after each new one, log it. Result written to history.db event(kind=Backup).
// issue #77: native backup plane — when compose.enabled=false (default) use local brew PostgreSQL
// (authoritative instance, see architecture/fusion-db-conventions-1030.md §1): resolve pg_dumpall from PATH
// and connect directly to 127.0.0.1:5432; Qdrant probes the local container (docker ps), absent → INFO event skip (absence is not an error).
// compose-plane behavior unchanged. Event detail records plane=native|compose.
use crate::alert::{Alert, AlertKind, Alerter};
use crate::store::HistoryStore;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Injected docker compose exec fn. Signature: (service, args) -> stdout bytes.
// prod=real docker compose exec; test=fake returning fixed dump bytes.
pub type ExecFn = Arc<dyn Fn(&str, &[&str]) -> Result<Vec<u8>> + Send + Sync>;

// issue #77: injected native exec fn. Signature: (bin, args) -> stdout bytes.
// prod=spawn local binary directly (pg_dumpall / docker); test=fake.
// Note: only for small outputs (docker ps / tar metadata). Large output (dump stream) goes through DumpFn streaming, see #115.
pub type NativeExecFn = Arc<dyn Fn(&str, &[&str]) -> Result<Vec<u8>> + Send + Sync>;

// issue #115: streaming dump fn. Signature: (bin, args, out_path) -> gzipped byte count.
// Child stdout streams straight through GzEncoder into the file — no in-memory buffering (old impl buffered
// a full Vec<u8>, a 2.2GB dump sat in daemon RSS, prod incident 2026-11-09). prod=real spawn;
// test=fake writing gzip directly.
pub type DumpFn = Arc<dyn Fn(&str, &[&str], &Path) -> Result<u64> + Send + Sync>;

// Read the tail of the dump stderr file (failure message carries the root cause, at most lines lines).
fn read_file_tail(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let v: Vec<&str> = s.lines().collect();
            let start = v.len().saturating_sub(lines);
            v[start..].join(" | ")
        }
        Err(_) => "(stderr log unreadable)".to_string(),
    }
}

// Prod streaming dump: spawn bin, gzip stdout chunk by chunk → out_path, collect the stderr tail.
// Injects PGPASSWORD when FUSION_PG_PASSWORD is set (same semantics as real_native_exec).
// Failure semantics: spawn failure / non-zero exit → bail (fail-closed alerting done by run()).
pub fn real_dump_fn() -> DumpFn {
    Arc::new(|bin: &str, args: &[&str], out_path: &Path| {
        tracing::info!(bin, args = ?args, out = %out_path.display(), "exec dump (stream)");
        let mut cmd = std::process::Command::new(bin);
        // stderr goes to a file instead of inherit — under launchd inherited output lands in no log,
        // exit 1 unattributable (2026-11-09 incident: after PG-side cancel only exit Some(1) was visible).
        // Direct file write has no pipe-full blocking risk.
        let err_path = out_path.with_file_name(format!(
            "{}.stderr.log",
            out_path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let err_file = std::fs::File::create(&err_path)
            .with_context(|| format!("create dump stderr log {}", err_path.display()))?;
        let err_for_child = err_file
            .try_clone()
            .with_context(|| format!("clone dump stderr handle {}", err_path.display()))?;
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::from(err_for_child));
        if let Ok(pw) = std::env::var("FUSION_PG_PASSWORD")
            && !pw.is_empty()
        {
            cmd.env("PGPASSWORD", pw);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {bin}: {e}"))?;
        let Some(stdout) = child.stdout.take() else {
            // Reap on the error path too — skipping wait leaves a zombie process
            let _ = child.kill();
            let _ = child.wait();
            bail!("{bin} stdout unreadable");
        };
        let f = std::fs::File::create(out_path)
            .with_context(|| format!("create backup file {}", out_path.display()))?;
        let mut enc = GzEncoder::new(f, Compression::default());
        let mut rdr = std::io::BufReader::new(stdout);
        let streamed = (|| -> anyhow::Result<u64> {
            let n = std::io::copy(&mut rdr, &mut enc)
                .with_context(|| format!("stream {bin} → gzip"))?;
            enc.finish().with_context(|| format!("gzip finish {bin}"))?;
            Ok(n)
        })();
        let n = match streamed {
            Ok(n) => n,
            Err(e) => {
                // Streaming phase failed: kill and reap the pg_dumpall child before returning
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let out = child.wait().with_context(|| format!("wait {bin}"))?;
        if !out.success() {
            // Attach the stderr tail to the error on failure (root cause visible in alerts/events)
            let tail = read_file_tail(&err_path, 20);
            bail!("{bin} failed: exit {:?}, stderr tail: {}", out.code(), tail);
        }
        Ok(n)
    })
}

// Prod native exec: spawn the binary directly. Injects PGPASSWORD when FUSION_PG_PASSWORD is set
// (usually unneeded under brew trust auth, kept for remote/password auth scenarios).
pub fn real_native_exec() -> NativeExecFn {
    Arc::new(|bin: &str, args: &[&str]| {
        tracing::info!(bin, args = ?args, "exec native (backup)");
        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);
        if let Ok(pw) = std::env::var("FUSION_PG_PASSWORD")
            && !pw.is_empty()
        {
            cmd.env("PGPASSWORD", pw);
        }
        let out = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("spawn {bin}: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{bin} failed: {}", stderr.trim());
        }
        Ok(out.stdout)
    })
}

// issue #77: resolve the pg_dumpall absolute path from PATH (resolution result visible in logs).
// Falls back to the bare name "pg_dumpall" when unresolved (spawn failure fail-closed alerts at run time).
fn resolve_pg_dump_bin(override_bin: &Option<String>) -> String {
    if let Some(b) = override_bin
        && !b.trim().is_empty()
    {
        tracing::info!(bin = %b, "pg_dumpall using configured override path");
        return b.trim().to_string();
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("pg_dumpall");
            if cand.is_file() {
                tracing::info!(bin = %cand.display(), "pg_dumpall resolved from PATH");
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    tracing::warn!(
        "pg_dumpall not found in PATH, falling back to bare name (spawn failure will fail-closed alert)"
    );
    "pg_dumpall".to_string()
}

// Prod exec: shell `docker compose --env-file ... -f sidecar -f business exec <svc> <args>`
pub fn real_exec(sidecar: &Path, business: &Path, env_file: &Path) -> ExecFn {
    let sidecar = sidecar.to_path_buf();
    let business = business.to_path_buf();
    let env_file = env_file.to_path_buf();
    Arc::new(move |svc: &str, args: &[&str]| {
        let mut full = vec![
            "compose".to_string(),
            "--env-file".to_string(),
            env_file.to_string_lossy().into(),
            "-f".to_string(),
            sidecar.to_string_lossy().into(),
            "-f".to_string(),
            business.to_string_lossy().into(),
            "exec".to_string(),
            svc.to_string(),
        ];
        for a in args {
            full.push(a.to_string());
        }
        tracing::info!(cmd = ?full, "exec docker compose (backup)");
        let out = std::process::Command::new("docker")
            .args(&full[1..])
            .output()
            .map_err(|e| anyhow::anyhow!("spawn docker compose exec: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("docker compose exec {svc} failed: {}", stderr.trim());
        }
        Ok(out.stdout)
    })
}

pub struct BackupRunner {
    // compose-plane exec (docker compose exec)
    exec: ExecFn,
    // issue #77: native-plane exec (spawn local binary directly)
    native_exec: NativeExecFn,
    // issue #115: streaming dump (pg_dumpall stdout → gzip file, zero in-memory buffering)
    dump_fn: DumpFn,
    // issue #115: full docker args for the compose-plane dump (env-file/-f prefix mirrors
    // real_exec, prebuilt in new(); None in test constructors → fail-closed)
    compose_dump_args: Option<Vec<String>>,
    // issue #77: true = native plane (compose.enabled=false), uses local brew PG
    plane_native: bool,
    // issue #77: resolved native pg_dumpall path (config override or PATH)
    pg_dump_bin: String,
    // issue #77: native PG connection params; -U omitted when pg_user=None (default OS user)
    pg_host: String,
    pg_port: u16,
    pg_user: Option<String>,
    target: Option<PathBuf>,
    retention: u32,
    // issue #132: weekly retention count (0 = disabled)
    weekly_retention: u32,
}

impl BackupRunner {
    // Prod constructor: resolve target from env/env_file, real exec. store/alerter are passed into run() by the caller.
    // issue #77: plane decided by compose.enabled — off (default) uses native brew PG.
    pub fn new(cfg: &crate::config::Config) -> Self {
        let target = resolve_target();
        let exec = real_exec(
            &cfg.compose.sidecar_file,
            &cfg.compose.business_file,
            &cfg.compose.env_file,
        );
        // issue #115: full compose dump args (mirrors the real_exec command list)
        let compose_dump_args = Some(vec![
            "compose".to_string(),
            "--env-file".to_string(),
            cfg.compose.env_file.to_string_lossy().into_owned(),
            "-f".to_string(),
            cfg.compose.sidecar_file.to_string_lossy().into_owned(),
            "-f".to_string(),
            cfg.compose.business_file.to_string_lossy().into_owned(),
            "exec".to_string(),
            "-T".to_string(),
            "postgres".to_string(),
            "pg_dumpall".to_string(),
            "-U".to_string(),
            "fusion".to_string(),
        ]);
        let plane_native = !cfg.compose.enabled;
        let pg_dump_bin = resolve_pg_dump_bin(&cfg.pg_dump_bin);
        let pg_user = std::env::var("FUSION_PG_USER")
            .ok()
            .filter(|s| !s.trim().is_empty());
        tracing::info!(
            plane = if plane_native { "native" } else { "compose" },
            "backup runner plane selected (issue #77)"
        );
        Self {
            exec,
            native_exec: real_native_exec(),
            dump_fn: real_dump_fn(),
            compose_dump_args,
            plane_native,
            pg_dump_bin,
            pg_host: cfg.native_pg_host.clone(),
            pg_port: cfg.native_pg_port,
            pg_user,
            target,
            retention: cfg.backup_retention,
            weekly_retention: cfg.backup_weekly_retention,
        }
    }

    // Test constructor: inject fake exec, explicit target/retention (compose plane, original behavior).
    pub fn new_with_exec(exec: ExecFn, target: Option<PathBuf>, retention: u32) -> Self {
        Self {
            exec,
            native_exec: Arc::new(|_bin: &str, _args: &[&str]| {
                bail!("native exec must not be called in compose-plane tests")
            }),
            dump_fn: Arc::new(|_bin: &str, _args: &[&str], _out: &Path| {
                bail!("dump_fn must not be called without injection (tests must use with_dump_fn)")
            }),
            compose_dump_args: Some(vec![]),
            plane_native: false,
            pg_dump_bin: "pg_dumpall".to_string(),
            pg_host: "127.0.0.1".to_string(),
            pg_port: 5432,
            pg_user: None,
            target,
            retention,
            weekly_retention: 0,
        }
    }

    // issue #77 test constructor: inject fake native exec, native plane.
    pub fn new_native_with_exec(
        native_exec: NativeExecFn,
        target: Option<PathBuf>,
        retention: u32,
    ) -> Self {
        Self {
            exec: Arc::new(|_svc: &str, _args: &[&str]| {
                bail!("compose exec must not be called in native-plane tests")
            }),
            native_exec,
            dump_fn: Arc::new(|_bin: &str, _args: &[&str], _out: &Path| {
                bail!("dump_fn must not be called without injection (tests must use with_dump_fn)")
            }),
            compose_dump_args: None,
            plane_native: true,
            pg_dump_bin: "pg_dumpall".to_string(),
            pg_host: "127.0.0.1".to_string(),
            pg_port: 5432,
            pg_user: None,
            target,
            retention,
            weekly_retention: 0,
        }
    }

    // issue #115: inject fake dump_fn (builder, test-only) — fake gzips fixed bytes directly into the file.
    #[cfg(test)]
    fn with_dump_fn(mut self, dump: DumpFn) -> Self {
        self.dump_fn = dump;
        self
    }

    // fail-closed: target unset → refuse + alert.
    // store/alerter borrowed (avoids Clone on Connection).
    pub async fn run(
        &mut self,
        now_ts: u64,
        store: &HistoryStore,
        alerter: &mut Alerter,
    ) -> Result<()> {
        let target = match &self.target {
            Some(t) => t.clone(),
            None => {
                tracing::error!("FUSION_BACKUP_TARGET unset, fail-closed refusing backup");
                emit_alert(
                    alerter,
                    "FUSION_BACKUP_TARGET unset — backup refused (fail-closed)".to_string(),
                )
                .await;
                bail!("FUSION_BACKUP_TARGET unset — backup refused fail-closed (see .env.example)");
            }
        };
        // Ensure the target dir exists (NAS mount assumed writable; unwritable fails loud at file write)
        if let Err(e) = std::fs::create_dir_all(&target) {
            emit_alert(alerter, format!("backup target mkdir fail: {e}")).await;
            bail!("backup target mkdir fail: {e}");
        }
        // issue #113: run_stamp with hhmmss — same-day backups no longer overwrite each other
        let date = run_stamp();
        let plane = if self.plane_native {
            "native"
        } else {
            "compose"
        };
        // 1. Postgres: pg_dumpall → gzip → (optional) at-rest encryption
        // #117: write .sql.gz.tmp first, atomic rename on success — daemon killed mid-dump (SIGTERM/
        // SIGKILL) leaves only a tmp fragment, the final name always corresponds to a complete gzip (old impl
        // wrote the final name directly; 2026-11-09 daemon died mid-dump leaving a 157MB footer-less fake backup).
        // tmp name does not end with .sql.gz → prune_old filter naturally skips it.
        // issue #132: with FUSION_BACKUP_ENC_KEY set, the dump is encrypted at rest after landing (openssl
        // aes-256-cbc) — encryption failure fail-closed (never leave plaintext in target), final name gets .enc suffix.
        let enc_key = resolve_enc_key();
        if enc_key.is_some() {
            tracing::info!(
                "FUSION_BACKUP_ENC_KEY set, backup will be encrypted at rest (aes-256-cbc)"
            );
        }
        let pg_suffix = if enc_key.is_some() {
            "sql.gz.enc"
        } else {
            "sql.gz"
        };
        let pg_path = target.join(format!("fusion-pg-{date}.{pg_suffix}"));
        let pg_tmp = target.join(format!("fusion-pg-{date}.sql.gz.tmp"));
        let _ = std::fs::remove_file(&pg_tmp);
        let pg_started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(now_ts);
        let pg_res = if self.plane_native {
            self.pg_dump_native(&pg_tmp)
        } else {
            self.pg_dump(&pg_tmp)
        };
        let pg_res = match pg_res {
            Ok(n) => {
                // issue #132: at-rest encryption — encrypt tmp into .enc.tmp, remove the plaintext tmp then rename on success.
                let enc_step: Result<Option<PathBuf>> = match &enc_key {
                    None => Ok(None),
                    Some(key) => {
                        let enc_tmp = target.join(format!("fusion-pg-{date}.sql.gz.enc.tmp"));
                        encrypt_file(&pg_tmp, &enc_tmp, key)
                            .map(|_| {
                                let _ = std::fs::remove_file(&pg_tmp);
                                Some(enc_tmp)
                            })
                            .map_err(|e| {
                                let _ = std::fs::remove_file(&pg_tmp);
                                anyhow::anyhow!(e)
                                    .context("backup at-rest encryption failed (fail-closed, no plaintext left)")
                            })
                    }
                };
                match enc_step {
                    Err(e) => Err(e),
                    Ok(Some(enc_tmp)) => match std::fs::rename(&enc_tmp, &pg_path) {
                        Ok(()) => Ok(n),
                        Err(e) => Err(anyhow::anyhow!(e).context("backup tmp rename failed")),
                    },
                    Ok(None) => match std::fs::rename(&pg_tmp, &pg_path) {
                        Ok(()) => Ok(n),
                        Err(e) => Err(anyhow::anyhow!(e).context("backup tmp rename failed")),
                    },
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&pg_tmp);
                Err(e)
            }
        };
        // issue #115: dur uses the real clock (old impl subtracted the injected now_ts, always 0, fake event data)
        let pg_dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(now_ts)
            .saturating_sub(pg_started);
        let pg_size = pg_path.metadata().map(|m| m.len()).unwrap_or(0);
        record_outcome(
            store, alerter, "postgres", plane, &pg_res, pg_size, pg_dur, &pg_path, now_ts,
        )
        .await;
        pg_res.context("postgres pg_dumpall failed")?;
        // 2. Qdrant: storage dir snapshot (copy storage dir → target/fusion-qdrant-{date}/)
        let qd_dir = target.join(format!("fusion-qdrant-{date}"));
        // issue #77: native-plane qdrant absence is not an error (INFO event skip), compose plane stays fail-closed
        if self.plane_native {
            let qd_res = self.qdrant_snapshot_native(&qd_dir, store, now_ts);
            let qd_size = qd_dir
                .join("storage.tar")
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            record_outcome(
                store, alerter, "qdrant", plane, &qd_res, qd_size, 0, &qd_dir, now_ts,
            )
            .await;
            qd_res.context("qdrant snapshot failed")?;
        } else {
            let qd_res = self.qdrant_snapshot(&qd_dir);
            record_outcome(
                store, alerter, "qdrant", plane, &qd_res, 0, 0, &qd_dir, now_ts,
            )
            .await;
            qd_res.context("qdrant snapshot failed")?;
        }
        // 2.5 issue #132: qdrant tar also encrypted at rest (when key set; absent plane skips, no dir)
        if let Some(key) = &enc_key {
            let tar = qd_dir.join("storage.tar");
            if tar.exists() {
                let tar_enc = qd_dir.join("storage.tar.enc");
                if let Err(e) = encrypt_file(&tar, &tar_enc, key) {
                    // fail-closed: encryption failure never leaves a plaintext tar in target (same semantics as the pg branch)
                    let _ = std::fs::remove_file(&tar);
                    return Err(e.context("qdrant snapshot at-rest encryption failed"));
                }
                tracing::info!(file = %tar_enc.display(), "qdrant snapshot encrypted");
            }
        }
        // 3. Retention pruning
        if let Err(e) = self.prune_old(&target) {
            tracing::warn!(err = %e, "backup retention prune failed (non-fatal)");
        }
        tracing::info!(target = %target.display(), "backup complete");
        Ok(())
    }

    // pg_dumpall via docker compose exec postgres — issue #115: stdout streams → gzip
    // file (old impl buffered a full Vec<u8>, a 2.2GB large-DB dump sat in daemon RSS).
    fn pg_dump(&self, out_path: &Path) -> Result<()> {
        // issue #115: compose prefix (env-file/-f) mirrors real_exec — prebuilt in new(),
        // missing (test constructor) fail-closed
        let Some(prefix) = &self.compose_dump_args else {
            bail!("compose dump args not configured (only test constructors omit them)");
        };
        let argv: Vec<&str> = prefix.iter().map(String::as_str).collect();
        let n = (self.dump_fn)("docker", &argv, out_path)?;
        tracing::info!(path = %out_path.display(), bytes = n, "pg_dumpall streamed to gzip file");
        Ok(())
    }

    // issue #77: native-plane pg_dump — local pg_dumpall connects directly to brew PG (127.0.0.1:5432).
    // -U omitted when pg_user=None (pg_dumpall defaults to the OS user, the installing user under brew trust auth).
    // issue #115: stdout streams → gzip file (old impl buffered it whole, 2.2GB sat in daemon RSS).
    fn pg_dump_native(&self, out_path: &Path) -> Result<()> {
        let port = self.pg_port.to_string();
        let mut args: Vec<&str> = vec!["-h", &self.pg_host, "-p", &port];
        if let Some(u) = &self.pg_user {
            args.push("-U");
            args.push(u);
        }
        let n = (self.dump_fn)(&self.pg_dump_bin, &args, out_path)?;
        tracing::info!(
            path = %out_path.display(),
            bytes = n,
            bin = %self.pg_dump_bin,
            "native pg_dumpall streamed to gzip file"
        );
        Ok(())
    }

    // issue #77: native-plane Qdrant snapshot — probe local containers (docker ps, including
    // non-sidecar-managed instances like OrbStack); if found, docker exec tar packs the storage.
    // Absence (docker unavailable / no qdrant container) records an INFO event and returns Ok
    // (absence is not an error; no qdrant on the native plane is the norm).
    fn qdrant_snapshot_native(
        &self,
        out_dir: &Path,
        store: &HistoryStore,
        now_ts: u64,
    ) -> Result<()> {
        let names = match (self.native_exec)("docker", &["ps", "--format", "{{.Names}}"]) {
            Ok(n) => n,
            Err(e) => {
                tracing::info!(err = %e, "docker unavailable, no qdrant to back up on the native plane");
                record_absent(store, now_ts, "docker unavailable");
                return Ok(());
            }
        };
        let names = String::from_utf8_lossy(&names);
        let qdrant = names.lines().map(str::trim).find(|n| n.contains("qdrant"));
        let Some(container) = qdrant else {
            tracing::info!("no qdrant container, skipping qdrant backup on the native plane");
            record_absent(store, now_ts, "no qdrant container");
            return Ok(());
        };
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("mkdir qdrant backup dir {}", out_dir.display()))?;
        let tar_bytes = (self.native_exec)(
            "docker",
            &[
                "exec",
                container,
                "tar",
                "cf",
                "-",
                "-C",
                "/qdrant/storage",
                ".",
            ],
        )
        .with_context(|| format!("docker exec {container} tar qdrant storage failed"))?;
        let tar_path = out_dir.join("storage.tar");
        std::fs::write(&tar_path, &tar_bytes)
            .with_context(|| format!("write qdrant backup {}", tar_path.display()))?;
        tracing::info!(dir = %out_dir.display(), container, bytes = tar_bytes.len(), "native qdrant snapshot written");
        Ok(())
    }

    // Qdrant snapshot: docker compose exec qdrant, copy to target.
    // MVP: copy the in-container storage dir contents (prod creates via qdrant --snapshot; here exec reads storage).
    // Test fake exec returns placeholder bytes; writing the placeholder file is enough.
    fn qdrant_snapshot(&self, out_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("mkdir qdrant backup dir {}", out_dir.display()))?;
        // Pack qdrant storage via exec tar (avoids per-file exec)
        let tar_bytes = (self.exec)("qdrant", &["tar", "cf", "-", "-C", "/qdrant/storage", "."])?;
        let tar_path = out_dir.join("storage.tar");
        std::fs::write(&tar_path, &tar_bytes)
            .with_context(|| format!("write qdrant backup {}", tar_path.display()))?;
        tracing::info!(dir = %out_dir.display(), bytes = tar_bytes.len(), "qdrant snapshot written");
        Ok(())
    }

    // Retention pruning (issue #132 two-tier retention): sort by filename date descending
    // (lexicographic = chronological; date is YYYYMMDD[-hhmmss] or d{day-index}), the first
    // retention copies are daily keeps; among the rest keep one newest per natural week
    // (day_index/7), at most weekly_retention weeks — for month-level lookback.
    // weekly_retention=0 → old flat behavior. Pruning only touches pg files + same-date qdrant dirs.
    fn prune_old(&self, target: &Path) -> Result<()> {
        if self.retention == 0 {
            return Ok(());
        }
        // Collect pg backups (path, date_str, day_index), sorted by date descending
        let mut pg_files: Vec<(PathBuf, String, i64)> = Vec::new();
        for entry in std::fs::read_dir(target)? {
            let e = entry?;
            let p = e.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str())
                && name.starts_with("fusion-pg-")
                && (name.ends_with(".sql.gz") || name.ends_with(".sql.gz.enc"))
            {
                let date = name
                    .trim_start_matches("fusion-pg-")
                    .trim_end_matches(".sql.gz.enc")
                    .trim_end_matches(".sql.gz")
                    .to_string();
                let day = parse_backup_day(&date).unwrap_or(i64::MAX);
                pg_files.push((p, date, day));
            }
        }
        pg_files.sort_by(|a, b| b.1.cmp(&a.1));
        // Daily keep = first retention copies; weekly keep = one newest per week among the rest (up to N weeks)
        let daily_keep = (self.retention as usize).min(pg_files.len());
        let mut prune: Vec<(PathBuf, String)> = Vec::new();
        let mut last_week: Option<i64> = None;
        let mut weeks_kept: u32 = 0;
        for (i, (p, date, day)) in pg_files.iter().enumerate() {
            if i < daily_keep {
                continue;
            }
            if self.weekly_retention > 0 && weeks_kept < self.weekly_retention {
                let week = day / 7;
                if last_week != Some(week) {
                    // Keep the first copy of each week (the week's newest), prune the rest of that week
                    last_week = Some(week);
                    weeks_kept += 1;
                    continue;
                }
            }
            prune.push((p.clone(), date.clone()));
        }
        for (p, date) in &prune {
            tracing::info!(file = %p.display(), "prune old pg backup (daily keep {}, weekly keep {})", self.retention, self.weekly_retention);
            if let Err(e) = std::fs::remove_file(p) {
                tracing::warn!(file = %p.display(), err = %e, "prune pg file failed");
            }
            // Matching qdrant dir (same date)
            let qd_dir = target.join(format!("fusion-qdrant-{date}"));
            if qd_dir.exists() {
                tracing::info!(dir = %qd_dir.display(), "prune old qdrant backup");
                if let Err(e) = std::fs::remove_dir_all(&qd_dir) {
                    tracing::warn!(dir = %qd_dir.display(), err = %e, "prune qdrant dir failed");
                }
            }
        }
        Ok(())
    }

    // Retention prune helper stays inside the impl (above).
}

// issue #77: native-plane qdrant absence → INFO event (absence is not an error, no alert).
fn record_absent(store: &HistoryStore, now_ts: u64, reason: &str) {
    let detail = format!("qdrant backup skipped ({reason}, native plane)");
    if let Err(e) = store.record_event("qdrant", "Backup", &detail, now_ts, "backup", "info") {
        tracing::warn!(err = %e, "qdrant absent event write failed (non-fatal)");
    }
    tracing::info!(reason, "qdrant native absence recorded");
}

// Record the backup outcome to history.db (kind=Backup) + alert on failure.
// issue #77: detail carries plane=native|compose so history shows backup coverage.
#[allow(clippy::too_many_arguments)]
async fn record_outcome(
    store: &HistoryStore,
    alerter: &mut Alerter,
    svc: &str,
    plane: &str,
    res: &Result<()>,
    size: u64,
    dur: u64,
    path: &Path,
    now_ts: u64,
) {
    let (status, detail) = match res {
        Ok(()) => (
            "ok",
            format!(
                "{} backup ok plane={} size={} dur={}s path={}",
                svc,
                plane,
                size,
                dur,
                path.display()
            ),
        ),
        Err(e) => ("fail", format!("{} backup fail plane={}: {e}", svc, plane)),
    };
    if let Err(e) = store.record_event(svc, "Backup", &detail, now_ts, "backup", "info") {
        tracing::warn!(svc, err = %e, "backup event write failed (non-fatal)");
    }
    if res.is_err() {
        emit_alert(alerter, detail).await;
    }
    tracing::info!(svc, status, "backup outcome recorded");
}

async fn emit_alert(alerter: &mut Alerter, detail: String) {
    let _ = alerter
        .alert(Alert {
            service: "backup".to_string(),
            kind: AlertKind::BackupFailed,
            detail,
            ts: 0,
        })
        .await;
}

// Resolve FUSION_BACKUP_TARGET: process env first, then the .env file (compose env_file path).
// Neither present → None (caller fail-closes).
pub fn resolve_target() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("FUSION_BACKUP_TARGET")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    // Fallback: read .env (compose env_file defaults to .env)
    let env_path = PathBuf::from(".env");
    if let Ok(text) = std::fs::read_to_string(&env_path) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "FUSION_BACKUP_TARGET"
                && !v.trim().is_empty()
            {
                return Some(PathBuf::from(v.trim()));
            }
        }
    }
    None
}

// issue #132: resolve FUSION_BACKUP_ENC_KEY — process env first, then the .env file (same
// chain as resolve_target). The value never reaches logs/events. None = no encryption (old behavior).
pub fn resolve_enc_key() -> Option<String> {
    if let Ok(v) = std::env::var("FUSION_BACKUP_ENC_KEY")
        && !v.is_empty()
    {
        return Some(v);
    }
    let env_path = PathBuf::from(".env");
    if let Ok(text) = std::fs::read_to_string(&env_path) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "FUSION_BACKUP_ENC_KEY"
                && !v.trim().is_empty()
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

// issue #132: file-level at-rest encryption — openssl enc aes-256-cbc (pbkdf2, 200k iterations).
// key passed out-of-band (avoiding the argv/env leak surface), openssl missing/failure → Err (caller
// fail-closes and removes plaintext fragments). Streaming copy, no whole-buffer in memory.
pub fn encrypt_file(src: &Path, dst: &Path, key: &str) -> Result<u64> {
    // key goes through child-private env (-pass env:VAR) — never in argv, never in the parent env, never on disk
    let mut child = std::process::Command::new("openssl")
        .args([
            "enc",
            "-aes-256-cbc",
            "-pbkdf2",
            "-iter",
            "200000",
            "-salt",
            "-in",
        ])
        .arg(src)
        .args(["-pass", "env:FUSION_BACKUP_ENC_KEY"])
        .env("FUSION_BACKUP_ENC_KEY", key)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn openssl: {e}"))?;
    let Some(stdout) = child.stdout.take() else {
        bail!("openssl stdout unreadable");
    };
    let mut out =
        std::fs::File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let mut rdr = std::io::BufReader::new(stdout);
    let n = std::io::copy(&mut rdr, &mut out)
        .with_context(|| format!("stream encrypt → {}", dst.display()))?;
    drop(out);
    let status = child.wait().with_context(|| "wait openssl")?;
    if !status.success() {
        let _ = std::fs::remove_file(dst);
        bail!("openssl encryption failed: exit {:?}", status.code());
    }
    Ok(n)
}

// issue #132: backup file date → days since 1970-01-01 (for week bucketing).
// Supports "YYYYMMDD[-hhmmss]" (first 8 chars) and legacy "d{day-index}".
fn parse_backup_day(date: &str) -> Option<i64> {
    if let Some(rest) = date.strip_prefix('d') {
        return rest.parse::<i64>().ok();
    }
    let ymd = &date.get(..8)?;
    if ymd.len() != 8 || !ymd.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i64 = ymd[..4].parse().ok()?;
    let m: i64 = ymd[4..6].parse().ok()?;
    let d: i64 = ymd[6..8].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

// Howard Hinnant days_from_civil (pure calendar math, no chrono dependency).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// Pure scheduling predicate (shared by the daemon tick loop + tests).
// Due when now >= last + schedule; last=0 (first run) due at any now.
pub fn backup_due(now_ts: u64, last_backup_ts: u64, schedule_sec: u64) -> bool {
    if last_backup_ts == 0 {
        return true;
    }
    now_ts.saturating_sub(last_backup_ts) >= schedule_sec
}

// Date stamp YYYYMMDD. Tests cannot use chrono (no dep added); the env FUSION_BACKUP_DATE override pins the date for tests.
fn date_stamp() -> String {
    if let Ok(v) = std::env::var("FUSION_BACKUP_DATE")
        && !v.is_empty()
    {
        return v;
    }
    // Simple: SystemTime → secs. Date precision is enough for prod (daily backups).
    // std cannot reliably extract Y/M/D, so fall back to a fixed placeholder overridden by the caller. Tests must set FUSION_BACKUP_DATE.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 86400s/day, days since 1970-01-01 → compose a day-index placeholder in the YYYYMMDD slot
    // (the real date format is overridden by test FUSION_BACKUP_DATE; a day-index unique id is acceptable in prod)
    let day = secs / 86400;
    format!("d{day}")
}

// issue #113: filename timestamp — returned verbatim under env override (test determinism), prod appends a UTC
// hhmmss suffix so same-day backups no longer overwrite in place (d20766 overwritten to an 85MB truncated file
// by an orphan daemon, incident 2026-11-09).
fn run_stamp() -> String {
    let date = date_stamp();
    if std::env::var("FUSION_BACKUP_DATE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return date;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hhmmss = format!(
        "{:02}{:02}{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    );
    format!("{date}-{hhmmss}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::Alerter;
    use crate::store::HistoryStore;
    use std::io::Write as _;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn mem_store() -> HistoryStore {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "fusion-sv-backup-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&p);
        HistoryStore::open(&p).unwrap()
    }

    fn fake_exec(dump: Vec<u8>) -> ExecFn {
        let dump = Arc::new(dump);
        Arc::new(move |_svc: &str, _args: &[&str]| Ok((*dump).clone()))
    }

    // issue #115: fake dump_fn — gzips fixed bytes straight into out_path (zero spawns).
    // Returned byte count matches real streaming semantics (compressed size irrelevant, asserts only check the file is non-empty).
    fn fake_dump(content: &'static [u8]) -> DumpFn {
        Arc::new(move |_bin: &str, _args: &[&str], out: &Path| {
            let f = std::fs::File::create(out)?;
            let mut enc = GzEncoder::new(f, Compression::default());
            enc.write_all(content)?;
            enc.finish()?;
            Ok(content.len() as u64)
        })
    }

    fn fake_dump_err(msg: &'static str) -> DumpFn {
        Arc::new(move |_bin: &str, _args: &[&str], _out: &Path| bail!("{}", msg))
    }

    // issue #113: env override → run_stamp returned verbatim (test determinism intact)
    #[test]
    fn test_run_stamp_env_override_verbatim() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        assert_eq!(run_stamp(), "20260903");
    }

    // issue #113: no override → date-hhmmss suffix, same-day backups do not overwrite
    #[test]
    fn test_run_stamp_time_suffix_when_no_env() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::remove("FUSION_BACKUP_DATE");
        let s = run_stamp();
        let (date, tail) = s
            .rsplit_once('-')
            .expect("stamp should be date-hhmmss shaped");
        assert_eq!(date, date_stamp(), "prefix should match date_stamp: {s}");
        assert_eq!(tail.len(), 6, "hhmmss 6 digits: {s}");
        assert!(
            tail.chars().all(|c| c.is_ascii_digit()),
            "hhmmss all digits: {s}"
        );
    }

    // issue #113: prune compatible with hhmmss-stamped names (lexicographic sort + qdrant pairing)
    #[test]
    fn test_prune_keeps_stamped_names() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        for name in [
            "fusion-pg-d20766-101500.sql.gz",
            "fusion-pg-d20766-153000.sql.gz",
            "fusion-pg-d20765.sql.gz",
        ] {
            std::fs::File::create(target.join(name)).unwrap();
        }
        let native: NativeExecFn =
            Arc::new(|_bin: &str, _args: &[&str]| bail!("docker not installed"));
        let runner = BackupRunner::new_native_with_exec(native, Some(target.to_path_buf()), 1);
        runner.prune_old(target).unwrap();
        // Keep the newest copy (153000), delete the rest
        assert!(target.join("fusion-pg-d20766-153000.sql.gz").exists());
        assert!(!target.join("fusion-pg-d20766-101500.sql.gz").exists());
        assert!(!target.join("fusion-pg-d20765.sql.gz").exists());
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        // Hold the env test lock (#134 CI flake): set/remove/restore is mutually exclusive with other env tests.
        _lock: crate::env_map::tests::EnvLockGuard,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let lock = crate::env_map::tests::env_lock();
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
        fn remove(key: &'static str) -> Self {
            let lock = crate::env_map::tests::env_lock();
            let prev = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // test_backup_pg_dump_success: mock exec returns dump bytes → file written to target, Backup event recorded, success.
    #[tokio::test]
    // issue #31: lock must be held across run().await (date_stamp reads env inside run).
    // tokio::test defaults to a single-threaded runtime, no concurrent task contention, await-holding safe.
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_pg_dump_success() {
        // issue #31: env is globally shared, a parallel test's Drop would remove the DATE this test relies on — shared lock serializes
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        let mut runner = BackupRunner::new_with_exec(
            fake_exec(b"-- pg dump content --".to_vec()),
            Some(target.clone()),
            7,
        )
        .with_dump_fn(fake_dump(b"-- pg dump content --"));
        runner.run(1000, &store, &mut alerter).await.unwrap();
        // pg file exists and is non-empty
        let pg_file = target.join("fusion-pg-20260903.sql.gz");
        assert!(
            pg_file.exists(),
            "pg backup file should exist: {}",
            pg_file.display()
        );
        assert!(pg_file.metadata().unwrap().len() > 0);
        // qdrant dir exists
        let qd_dir = target.join("fusion-qdrant-20260903");
        assert!(qd_dir.exists(), "qdrant backup dir should exist");
    }

    // test_dump_tmp_removed_on_failure: dump dies mid-stream (same class as daemon killed) →
    // final name absent + tmp fragment cleaned, no fake complete backup (#117).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_dump_tmp_removed_on_failure() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        let dump_fn: DumpFn = Arc::new(move |_bin: &str, _args: &[&str], out: &Path| {
            std::fs::write(out, b"partial garbage no gzip footer")?;
            bail!("killed mid-dump")
        });
        let mut runner =
            BackupRunner::new_with_exec(fake_exec(Vec::new()), Some(target.clone()), 7)
                .with_dump_fn(dump_fn);
        let res = runner.run(1000, &store, &mut alerter).await;
        assert!(res.is_err(), "dump failure should bail");
        assert!(
            !target.join("fusion-pg-20260903.sql.gz").exists(),
            "final name must not exist after failure (fake complete backup)"
        );
        assert!(
            !target.join("fusion-pg-20260903.sql.gz.tmp").exists(),
            "tmp fragment should be cleaned after failure"
        );
    }

    // test_backup_fail_closed_no_target: FUSION_BACKUP_TARGET unset → refuse + alert.
    #[tokio::test]
    // issue #31: lock must be held across run().await (date_stamp reads env inside run).
    // tokio::test defaults to a single-threaded runtime, no concurrent task contention, await-holding safe.
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_fail_closed_no_target() {
        // issue #31: same as above, env lock serializes (env_map/supervisor tests share the same lock)
        let _g0 = crate::env_map::tests::env_lock();
        let _g1 = EnvGuard::remove("FUSION_BACKUP_TARGET");
        let _g2 = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        let mut runner = BackupRunner::new_with_exec(
            fake_exec(b"dump".to_vec()),
            None, // no target
            7,
        );
        let res = runner.run(1000, &store, &mut alerter).await;
        assert!(res.is_err(), "no target should fail-closed refuse");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("FUSION_BACKUP_TARGET"),
            "err should name the target: {msg}"
        );
    }

    // test_backup_retention_prunes_old: 8 backups + retention=7 → oldest pruned.
    #[tokio::test]
    // issue #31: lock must be held across run().await (date_stamp reads env inside run).
    // tokio::test defaults to a single-threaded runtime, no concurrent task contention, await-holding safe.
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_retention_prunes_old() {
        // issue #31: same as above, env lock serializes
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        // Pre-seed 8 old pg backups (dates all earlier than this run's 20260903, pruned by filename date sort)
        for i in 0..8u32 {
            // 20260820 .. 20260827 (lexicographic = chronological, all earlier than 20260903)
            let day = 20 + i;
            let p = target.join(format!("fusion-pg-202608{day}.sql.gz"));
            std::fs::write(&p, b"x").unwrap();
        }
        let store = mem_store();
        let mut alerter = Alerter::new(0, String::new());
        let mut runner =
            BackupRunner::new_with_exec(fake_exec(b"dump".to_vec()), Some(target.clone()), 7)
                .with_dump_fn(fake_dump(b"dump"));
        runner.run(1000, &store, &mut alerter).await.unwrap();
        // 8 old + 1 new = 9, retention 7 → the 2 oldest should be deleted
        let pg_files: Vec<_> = std::fs::read_dir(&target)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("fusion-pg-") && n.ends_with(".sql.gz"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            pg_files.len(),
            7,
            "retention=7 should leave 7 pg backups (incl. the new one), got {}",
            pg_files.len()
        );
        // The new backup should be kept
        assert!(target.join("fusion-pg-20260903.sql.gz").exists());
    }

    // issue #77: native plane — pg_dumpall succeeds + docker unavailable (qdrant absence INFO skip).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_native_pg_success_docker_absent() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260930");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        // native exec: docker → Err (no docker on this host); dump streams via fake dump_fn
        let native: NativeExecFn =
            Arc::new(|_bin: &str, _args: &[&str]| bail!("docker not installed"));
        let mut runner = BackupRunner::new_native_with_exec(native, Some(target.clone()), 7)
            .with_dump_fn(fake_dump(b"-- native pg dump --"));
        runner.run(1000, &store, &mut alerter).await.unwrap();
        let pg_file = target.join("fusion-pg-20260930.sql.gz");
        assert!(
            pg_file.exists(),
            "native pg backup file should exist: {}",
            pg_file.display()
        );
        assert!(pg_file.metadata().unwrap().len() > 0);
        // qdrant absent: no dir, but an INFO skip event exists
        assert!(
            !target.join("fusion-qdrant-20260930").exists(),
            "qdrant dir should not exist"
        );
        let evs = store.recent_events("qdrant", 5).unwrap();
        assert!(
            evs.iter()
                .any(|e| e.detail.contains("skipped") && e.severity == "info"),
            "qdrant absence should have an INFO skip event, got {evs:?}"
        );
        // event detail records plane=native (issue #77 acceptance)
        let evs = store.recent_events("postgres", 5).unwrap();
        assert!(
            evs.iter().any(|e| e.detail.contains("plane=native")),
            "postgres event should carry plane=native, got {evs:?}"
        );
    }

    // issue #77: native plane — pg_dumpall missing → fail-closed refuse + alert.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_native_missing_pg_dump_fail_closed() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260930");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        let native: NativeExecFn =
            Arc::new(|_bin: &str, _args: &[&str]| bail!("pg_dumpall: command not found"));
        let mut runner = BackupRunner::new_native_with_exec(native, Some(target.clone()), 7)
            .with_dump_fn(fake_dump_err("spawn pg_dumpall: No such file or directory"));
        let res = runner.run(1000, &store, &mut alerter).await;
        assert!(res.is_err(), "missing pg_dumpall should fail-closed refuse");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("pg_dumpall"),
            "err should name pg_dumpall: {msg}"
        );
        let evs = store.recent_events("postgres", 5).unwrap();
        assert!(
            evs.iter().any(|e| e.detail.contains("fail")),
            "postgres failure event should be recorded, got {evs:?}"
        );
    }

    // issue #77: native plane — local qdrant container present → docker exec tar snapshot written.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_backup_native_qdrant_container_found() {
        let _g = crate::env_map::tests::env_lock();
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260930");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let mut alerter = Alerter::new(0, String::new());
        let store = mem_store();
        let native: NativeExecFn = Arc::new(|bin: &str, args: &[&str]| {
            if bin == "pg_dumpall" {
                Ok(b"dump".to_vec())
            } else if bin == "docker" && args[0] == "ps" {
                Ok(b"other-svc\nqdrant\n".to_vec())
            } else {
                Ok(b"tar-bytes".to_vec())
            }
        });
        let mut runner = BackupRunner::new_native_with_exec(native, Some(target.clone()), 7)
            .with_dump_fn(fake_dump(b"dump"));
        runner.run(1000, &store, &mut alerter).await.unwrap();
        let tar = target.join("fusion-qdrant-20260930/storage.tar");
        assert!(
            tar.exists(),
            "qdrant storage.tar should exist: {}",
            tar.display()
        );
        assert_eq!(std::fs::read(&tar).unwrap(), b"tar-bytes");
    }

    // issue #132: date → day index (civil algorithm vs known anchors)
    #[test]
    fn test_parse_backup_day() {
        assert_eq!(parse_backup_day("19700101"), Some(0));
        let a = parse_backup_day("20260101").unwrap();
        let b = parse_backup_day("20261118").unwrap();
        assert_eq!(b - a, 321);
        assert_eq!(
            parse_backup_day("20261118-153000"),
            parse_backup_day("20261118")
        );
        assert_eq!(parse_backup_day("d20766"), Some(20766));
        assert_eq!(parse_backup_day("garbage"), None);
    }

    // issue #132: two-tier retention — beyond daily retention, one newest per week kept for up to N weeks
    #[test]
    fn test_prune_weekly_tier() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let d0 = parse_backup_day("20260920").unwrap();
        let d1 = parse_backup_day("20261118").unwrap();
        for d in d0..=d1 {
            std::fs::File::create(target.join(format!("fusion-pg-d{d}.sql.gz"))).unwrap();
        }
        let native: NativeExecFn = Arc::new(|_bin: &str, _args: &[&str]| bail!("no docker"));
        let mut runner = BackupRunner::new_native_with_exec(native, Some(target.to_path_buf()), 14);
        runner.weekly_retention = 4;
        runner.prune_old(target).unwrap();
        let left = std::fs::read_dir(target)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(
            left, 18,
            "14 daily + 4 weekly should keep 18 files, got {left}"
        );
        assert!(
            target.join(format!("fusion-pg-d{d1}.sql.gz")).exists(),
            "newest daily backup should be kept"
        );
    }

    // issue #132: weekly=0 → old flat behavior
    #[test]
    fn test_prune_weekly_disabled_flat() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        for d in 100..=110 {
            std::fs::File::create(target.join(format!("fusion-pg-d{d}.sql.gz"))).unwrap();
        }
        let native: NativeExecFn = Arc::new(|_bin: &str, _args: &[&str]| bail!("no docker"));
        let runner = BackupRunner::new_native_with_exec(native, Some(target.to_path_buf()), 7);
        runner.prune_old(target).unwrap();
        let left = std::fs::read_dir(target)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(left, 7, "weekly=0 should keep only 7 files, got {left}");
    }

    // issue #132: .enc-suffixed backups also participate in collection and pruning
    #[test]
    fn test_prune_covers_enc_suffix() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        for d in 100..=104 {
            let n = if d % 2 == 0 {
                format!("fusion-pg-d{d}.sql.gz.enc")
            } else {
                format!("fusion-pg-d{d}.sql.gz")
            };
            std::fs::File::create(target.join(n)).unwrap();
        }
        let native: NativeExecFn = Arc::new(|_bin: &str, _args: &[&str]| bail!("no docker"));
        let runner = BackupRunner::new_native_with_exec(native, Some(target.to_path_buf()), 2);
        runner.prune_old(target).unwrap();
        assert!(target.join("fusion-pg-d104.sql.gz.enc").exists());
        assert!(target.join("fusion-pg-d103.sql.gz").exists());
        let left = std::fs::read_dir(target)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(left, 2, "got {left}");
    }

    // issue #132: openssl at-rest encryption roundtrip (skipped when openssl is unavailable)
    #[test]
    fn test_encrypt_file_roundtrip() {
        let has = std::process::Command::new("openssl")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has {
            eprintln!("openssl unavailable, skip");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("plain.bin");
        let dst = tmp.path().join("enc.bin");
        std::fs::write(&src, b"secret-pg-dump-content").unwrap();
        let n = encrypt_file(&src, &dst, "test-key-123").unwrap();
        assert!(n > 0);
        let enc = std::fs::read(&dst).unwrap();
        assert_ne!(enc, b"secret-pg-dump-content");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 -in {} -pass env:FUSION_BACKUP_ENC_KEY",
                dst.display()
            ))
            .env("FUSION_BACKUP_ENC_KEY", "test-key-123")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"secret-pg-dump-content");
    }

    // test_backup_schedule_triggers: schedule due → trigger one backup.
    // Logic test: backup_due predicate (now >= last + schedule).
    #[test]
    fn test_backup_schedule_triggers() {
        // schedule=100, last=1000 → now=1100 not due; now=1101 due
        let schedule: u64 = 100;
        let last: u64 = 1000;
        assert!(!backup_due(1099, last, schedule), "1099 < 1100 not due");
        assert!(backup_due(1100, last, schedule), "1100 >= 1100 due");
        // First run (last=0) due at any now>0
        assert!(backup_due(1, 0, schedule), "first run last=0 should be due");
    }
}
