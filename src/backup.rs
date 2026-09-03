// issue #10: NAS 备份原语。Postgres (pg_dumpall → gzip) + Qdrant (storage 目录快照) → FUSION_BACKUP_TARGET。
// 设计: injectable ExecFn 便于测试 (生产=shell docker compose exec, 测试=fake 不调 docker)。
// fail-closed: FUSION_BACKUP_TARGET 未设 → 拒绝 + 告警 BackupFailed (生产安全任务不静默跳过)。
// 保留: backup_retention 份, 新备份后裁剪旧备份, 记日志。结果写 history.db event(kind=Backup)。
use crate::alert::{Alert, AlertKind, Alerter};
use crate::store::HistoryStore;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// 注入的 docker compose exec 函数。签名: (service, args) -> stdout bytes。
// 生产=真 docker compose exec; 测试=fake 返回固定 dump bytes。
pub type ExecFn = Arc<dyn Fn(&str, &[&str]) -> Result<Vec<u8>> + Send + Sync>;

// 生产 exec: shell `docker compose --env-file ... -f sidecar -f business exec <svc> <args>`
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
            bail!("docker compose exec {svc} 失败: {}", stderr.trim());
        }
        Ok(out.stdout)
    })
}

pub struct BackupRunner {
    exec: ExecFn,
    target: Option<PathBuf>,
    retention: u32,
}

impl BackupRunner {
    // 生产构造: 从 env/env_file 解析 target, 真 exec。store/alerter 由调用方传入 run()。
    pub fn new(cfg: &crate::config::Config) -> Self {
        let target = resolve_target();
        let exec = real_exec(
            &cfg.compose.sidecar_file,
            &cfg.compose.business_file,
            &cfg.compose.env_file,
        );
        Self {
            exec,
            target,
            retention: cfg.backup_retention,
        }
    }

    // 测试构造: 注入 fake exec, 显式 target/retention。
    pub fn new_with_exec(exec: ExecFn, target: Option<PathBuf>, retention: u32) -> Self {
        Self {
            exec,
            target,
            retention,
        }
    }

    // fail-closed: target 未设 → 拒绝 + 告警。
    // store/alerter 借用传入 (避免 Clone Connection)。
    pub async fn run(
        &mut self,
        now_ts: u64,
        store: &HistoryStore,
        alerter: &mut Alerter,
    ) -> Result<()> {
        let target = match &self.target {
            Some(t) => t.clone(),
            None => {
                tracing::error!("FUSION_BACKUP_TARGET 未设, fail-closed 拒绝备份");
                emit_alert(
                    alerter,
                    "FUSION_BACKUP_TARGET unset — backup refused (fail-closed)".to_string(),
                )
                .await;
                bail!("FUSION_BACKUP_TARGET 未设 — 备份 fail-closed 拒绝 (参考 .env.example)");
            }
        };
        // 确保目标目录存在 (NAS 挂载点假设可写, 不可写则写文件时 fail loud)
        if let Err(e) = std::fs::create_dir_all(&target) {
            emit_alert(alerter, format!("backup target mkdir fail: {e}")).await;
            bail!("backup target mkdir fail: {e}");
        }
        let date = date_stamp();
        // 1. Postgres: pg_dumpall → gzip
        let pg_path = target.join(format!("fusion-pg-{date}.sql.gz"));
        let pg_started = now_ts;
        let pg_res = self.pg_dump(&pg_path);
        let pg_dur = now_ts.saturating_sub(pg_started);
        let pg_size = pg_path.metadata().map(|m| m.len()).unwrap_or(0);
        record_outcome(
            store, alerter, "postgres", &pg_res, pg_size, pg_dur, &pg_path, now_ts,
        )
        .await;
        pg_res.context("postgres pg_dumpall 失败")?;
        // 2. Qdrant: storage 目录快照 (copy storage dir → target/fusion-qdrant-{date}/)
        let qd_dir = target.join(format!("fusion-qdrant-{date}"));
        let qd_res = self.qdrant_snapshot(&qd_dir);
        record_outcome(store, alerter, "qdrant", &qd_res, 0, 0, &qd_dir, now_ts).await;
        qd_res.context("qdrant snapshot 失败")?;
        // 3. 保留裁剪
        if let Err(e) = self.prune_old(&target) {
            tracing::warn!(err = %e, "backup 保留裁剪失败 (非致命)");
        }
        tracing::info!(target = %target.display(), "备份完成");
        Ok(())
    }

    // pg_dumpall via docker compose exec postgres, stdout → gzip 文件
    fn pg_dump(&self, out_path: &Path) -> Result<()> {
        let dump = (self.exec)("postgres", &["pg_dumpall", "-U", "fusion"])?;
        let f = std::fs::File::create(out_path)
            .with_context(|| format!("create backup file {}", out_path.display()))?;
        let mut enc = GzEncoder::new(f, Compression::default());
        enc.write_all(&dump).context("gzip write pg_dumpall")?;
        enc.finish().context("gzip finish pg_dumpall")?;
        tracing::info!(path = %out_path.display(), bytes = dump.len(), "pg_dumpall 已压缩写入");
        Ok(())
    }

    // Qdrant 快照: docker compose exec qdrant 创建快照目录, 复制到 target。
    // MVP: 拷贝容器内 storage 目录内容 (生产由 qdrant --snapshot 创建, 此处 exec 读 storage)。
    // 测试 fake exec 返回占位 bytes, 写入占位文件即可。
    fn qdrant_snapshot(&self, out_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("mkdir qdrant backup dir {}", out_dir.display()))?;
        // 用 exec tar 打包 qdrant storage (避免逐文件 exec)
        let tar_bytes = (self.exec)("qdrant", &["tar", "cf", "-", "-C", "/qdrant/storage", "."])?;
        let tar_path = out_dir.join("storage.tar");
        std::fs::write(&tar_path, &tar_bytes)
            .with_context(|| format!("write qdrant backup {}", tar_path.display()))?;
        tracing::info!(dir = %out_dir.display(), bytes = tar_bytes.len(), "qdrant 快照已写入");
        Ok(())
    }

    // 保留裁剪: 按文件名 date 排序 (词序=时序, 因 date 为 YYYYMMDD 或 d{日序号}),
    // 删除超出 retention 的最旧备份。日志记录每次裁剪。
    fn prune_old(&self, target: &Path) -> Result<()> {
        if self.retention == 0 {
            return Ok(());
        }
        // 收集 pg 备份 (path, date_str), 按日期降序, 保留前 retention 份
        let mut pg_files: Vec<(PathBuf, String)> = Vec::new();
        for entry in std::fs::read_dir(target)? {
            let e = entry?;
            let p = e.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str())
                && name.starts_with("fusion-pg-")
                && name.ends_with(".sql.gz")
            {
                let date = name
                    .trim_start_matches("fusion-pg-")
                    .trim_end_matches(".sql.gz")
                    .to_string();
                pg_files.push((p, date));
            }
        }
        // 按日期降序 (新→旧), 保留前 retention, 删其余
        pg_files.sort_by(|a, b| b.1.cmp(&a.1));
        if pg_files.len() <= self.retention as usize {
            return Ok(());
        }
        let prune_count = pg_files.len() - self.retention as usize;
        // 取最旧的 prune_count 份 (降序后尾部)
        for (p, date) in pg_files.iter().rev().take(prune_count) {
            tracing::info!(file = %p.display(), "prune 旧 pg 备份 (超出 retention {})", self.retention);
            if let Err(e) = std::fs::remove_file(p) {
                tracing::warn!(file = %p.display(), err = %e, "prune pg 文件失败");
            }
            // 对应 qdrant 目录 (同 date)
            let qd_dir = target.join(format!("fusion-qdrant-{date}"));
            if qd_dir.exists() {
                tracing::info!(dir = %qd_dir.display(), "prune 旧 qdrant 备份");
                if let Err(e) = std::fs::remove_dir_all(&qd_dir) {
                    tracing::warn!(dir = %qd_dir.display(), err = %e, "prune qdrant 目录失败");
                }
            }
        }
        Ok(())
    }

    // 保留裁剪内部方法保留在 impl 内 (上面)。
}

// 记录备份结果到 history.db (kind=Backup) + 失败告警。
#[allow(clippy::too_many_arguments)]
async fn record_outcome(
    store: &HistoryStore,
    alerter: &mut Alerter,
    svc: &str,
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
                "{} backup ok size={} dur={}s path={}",
                svc,
                size,
                dur,
                path.display()
            ),
        ),
        Err(e) => ("fail", format!("{} backup fail: {e}", svc)),
    };
    if let Err(e) = store.record_event(svc, "Backup", &detail, now_ts, "backup") {
        tracing::warn!(svc, err = %e, "backup event write fail (非致命)");
    }
    if res.is_err() {
        emit_alert(alerter, detail).await;
    }
    tracing::info!(svc, status, "backup outcome 已记录");
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

// 解析 FUSION_BACKUP_TARGET: 先进程 env, 再 .env 文件 (compose env_file 路径)。
// 两处均无 → None (调用方 fail-closed)。
pub fn resolve_target() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("FUSION_BACKUP_TARGET")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    // 兜底: 读 .env (compose env_file 默认 .env)
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

// 调度判定纯函数 (供 daemon tick 循环 + 测试共用)。
// now >= last + schedule 即到期; last=0 (首次) 任何 now 到期。
pub fn backup_due(now_ts: u64, last_backup_ts: u64, schedule_sec: u64) -> bool {
    if last_backup_ts == 0 {
        return true;
    }
    now_ts.saturating_sub(last_backup_ts) >= schedule_sec
}

// 日期戳 YYYYMMDD。测试无法用 chrono (未加依赖), 用 env FUSION_BACKUP_DATE 覆盖便于测试固定日期。
fn date_stamp() -> String {
    if let Ok(v) = std::env::var("FUSION_BACKUP_DATE")
        && !v.is_empty()
    {
        return v;
    }
    // 简易: 用 SystemTime → 秒。生产环境日期精度足够 (日级备份)。
    // 用 std 不可靠取年月日, 故回退固定占位由调用方覆盖。测试必须设 FUSION_BACKUP_DATE。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 86400s/日, 从 1970-01-01 起算天数 → 拼成 YYYYMMDD 占位用日序号
    // (真实日期格式由测试 FUSION_BACKUP_DATE 覆盖; 生产可接受日序号唯一标识)
    let day = secs / 86400;
    format!("d{day}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::Alerter;
    use crate::store::HistoryStore;
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

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
        fn remove(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
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

    // test_backup_pg_dump_success: mock exec 返回 dump bytes → 文件写入 target, Backup event 记录, 成功。
    #[tokio::test]
    async fn test_backup_pg_dump_success() {
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        let store = mem_store();
        let mut alerter = Alerter::new(0, String::new());
        let mut runner = BackupRunner::new_with_exec(
            fake_exec(b"-- pg dump content --".to_vec()),
            Some(target.clone()),
            7,
        );
        runner.run(1000, &store, &mut alerter).await.unwrap();
        // pg 文件存在且非空
        let pg_file = target.join("fusion-pg-20260903.sql.gz");
        assert!(pg_file.exists(), "pg 备份文件应存在: {}", pg_file.display());
        assert!(pg_file.metadata().unwrap().len() > 0);
        // qdrant 目录存在
        let qd_dir = target.join("fusion-qdrant-20260903");
        assert!(qd_dir.exists(), "qdrant 备份目录应存在");
    }

    // test_backup_fail_closed_no_target: FUSION_BACKUP_TARGET 未设 → 拒绝 + 告警。
    #[tokio::test]
    async fn test_backup_fail_closed_no_target() {
        let _g1 = EnvGuard::remove("FUSION_BACKUP_TARGET");
        let _g2 = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let store = mem_store();
        let mut alerter = Alerter::new(0, String::new());
        let mut runner = BackupRunner::new_with_exec(
            fake_exec(b"dump".to_vec()),
            None, // 无 target
            7,
        );
        let res = runner.run(1000, &store, &mut alerter).await;
        assert!(res.is_err(), "无 target 应 fail-closed 拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("FUSION_BACKUP_TARGET"),
            "err 应点名 target: {msg}"
        );
    }

    // test_backup_retention_prunes_old: 8 份备份 + retention=7 → 最旧被裁剪。
    #[tokio::test]
    async fn test_backup_retention_prunes_old() {
        let _d = EnvGuard::set("FUSION_BACKUP_DATE", "20260903");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        // 预置 8 份旧 pg 备份 (日期均早于本次 20260903, 按文件名 date 排序裁剪)
        for i in 0..8u32 {
            // 20260820 .. 20260827 (词序=时序, 均早于 20260903)
            let day = 20 + i;
            let p = target.join(format!("fusion-pg-202608{day}.sql.gz"));
            std::fs::write(&p, b"x").unwrap();
        }
        let store = mem_store();
        let mut alerter = Alerter::new(0, String::new());
        let mut runner =
            BackupRunner::new_with_exec(fake_exec(b"dump".to_vec()), Some(target.clone()), 7);
        runner.run(1000, &store, &mut alerter).await.unwrap();
        // 旧 8 份 + 新 1 份 = 9, retention 7 → 应删 2 份最旧
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
            "retention=7 应剩 7 份 pg 备份 (含本次新增), got {}",
            pg_files.len()
        );
        // 本次新增应保留
        assert!(target.join("fusion-pg-20260903.sql.gz").exists());
    }

    // test_backup_schedule_triggers: 调度到期 → 触发一次 backup。
    // 逻辑测试: backup_due 判定 (now >= last + schedule)。
    #[test]
    fn test_backup_schedule_triggers() {
        // schedule=100, last=1000 → now=1100 未到期; now=1101 到期
        let schedule: u64 = 100;
        let last: u64 = 1000;
        assert!(!backup_due(1099, last, schedule), "1099 < 1100 未到期");
        assert!(backup_due(1100, last, schedule), "1100 >= 1100 到期");
        // 首次 (last=0) 任何 now>0 到期
        assert!(backup_due(1, 0, schedule), "首次 last=0 应到期");
    }
}
