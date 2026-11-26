// issue #168: daemon 日志尺寸轮转 (copy-truncate)。
// daemon.launchd.log 是 launchd StandardOutPath 重定向 — launchd 持有 fd:
// rename 会让 launchd 继续写进被轮转的 inode (当前日志变死文件), 外部重定向
// tracing-appender 也接管不了 (且只有时间轮转)。copy-truncate: 拷贝快照到 .1,
// 原地 set_len(0) 截断 — launchd 对同一 inode 追加写不受影响, 新日志从头写。
// 拷贝与截断之间写入的行会丢 (copytruncate 标准取舍), 轮转以 GB 级阈值触发,
// 频率以天计, 可接受。备份命名 <file>.1 .. <file>.N (logrotate 惯例), 超 N 隐式
// 被后续 rename 覆盖淘汰。
use std::path::{Path, PathBuf};

// 轮转判定 (纯函数): 尺寸超阈值且轮转未禁用 (max_bytes > 0)。
pub fn should_rotate(size_bytes: u64, max_bytes: u64) -> bool {
    max_bytes > 0 && size_bytes > max_bytes
}

// 备份链平移计划: (from, to) 序号对, 从最老一端开始腾位。
// keep=3 → [(2,3), (1,2)]: .2→.3 (覆盖淘汰旧 .3), .1→.2; 随后当前文件拷为 .1。
// keep < 2 → 无备份链 (0/1 = 不留备份)。纯函数可测。
pub fn backup_shifts(keep: u32) -> Vec<(u32, u32)> {
    if keep < 2 {
        return Vec::new();
    }
    (1..keep).rev().map(|i| (i, i + 1)).collect()
}

// 第 n 份备份路径: daemon.launchd.log → daemon.launchd.log.1 (追加序号后缀)。
pub fn backup_path(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

// 单文件轮转执行器。返回是否实际轮转; 文件缺失 = 无可轮转 (err log 可能不
// 存在), 静默 false 不报错。到阈值才做重活 (rename/copy/truncate), 每 tick
// 只付一次 metadata stat。
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
    // 截断原 inode (打开句柄重用 launchd 持有的同一文件 — 绝不 rename)
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

// 路径解析: ~ 展开 (config 同款) → 相对路径优先 FUSION_ROOT (launchd plist
// WorkingDirectory 即 monorepo 根, 默认路径 "fusion-supervisor/logs/..." 相对根),
// 未设 FUSION_ROOT 回落 CWD。
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

// 每 tick 检查入口: 遍历配置路径逐个轮转。单文件失败 WARN 续跑下一个
// (轮转是卫生功能, 不许它拖垮监督循环)。
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
        assert!(!should_rotate(0, 100), "空文件不轮转");
        assert!(!should_rotate(100, 100), "等于阈值不轮转 (严格大于)");
        assert!(should_rotate(101, 100), "超阈值轮转");
        assert!(!should_rotate(u64::MAX, 0), "max=0 禁用轮转");
    }

    #[test]
    fn test_backup_shifts_plan() {
        assert_eq!(backup_shifts(3), vec![(2, 3), (1, 2)]);
        assert_eq!(backup_shifts(2), vec![(1, 2)]);
        assert!(backup_shifts(1).is_empty(), "keep=1 无平移, 只拷 .1");
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
        // 第一轮: 超阈值 (40 > 32) → .1 生成, 原文件清空
        std::fs::write(&log, "x".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "原文件已截断");
        assert_eq!(std::fs::read(dir.join("test.log.1")).unwrap().len(), 40);
        // 第二轮: 再次超阈值 → 旧 .1 平移到 .2, 新快照落 .1
        std::fs::write(&log, "y".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(dir.join("test.log.2")).unwrap().len(), 40);
        assert_eq!(std::fs::read(dir.join("test.log.1")).unwrap().len(), 40);
        // 第三轮: keep=2 封顶 → .2 被下一轮 rename 覆盖, 永远只有 .1 + .2
        std::fs::write(&log, "z".repeat(40)).unwrap();
        assert!(rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(dir.join("test.log.2")).unwrap().len(), 40);
        assert!(!dir.join("test.log.3").exists(), "keep 封顶不产生 .3");
        // 未超阈值不轮转
        std::fs::write(&log, "w".repeat(10)).unwrap();
        assert!(!rotate_file(&log, 32, 2).unwrap());
        assert_eq!(std::fs::read(&log).unwrap().len(), 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_rotate_file_missing_and_disabled() {
        let missing =
            std::env::temp_dir().join(format!("fsv-logrot-absent-{}", std::process::id()));
        assert!(!rotate_file(&missing, 32, 2).unwrap(), "文件缺失静默跳过");
        let dir = std::env::temp_dir().join(format!("fsv-logrot-dis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("d.log");
        std::fs::write(&log, "x".repeat(64)).unwrap();
        assert!(!rotate_file(&log, 0, 2).unwrap(), "max=0 禁用不轮转");
        assert_eq!(std::fs::read(&log).unwrap().len(), 64, "禁用时文件不动");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_rotate_path_absolute_and_tilde() {
        assert_eq!(
            resolve_rotate_path("/var/log/x.log"),
            PathBuf::from("/var/log/x.log"),
            "绝对路径原样"
        );
        // ~ 展开依赖宿主 HOME — 只断言形态, 不硬编码具体路径
        let p = resolve_rotate_path("~/logs/x.log");
        assert!(p.is_absolute() && !p.to_string_lossy().starts_with('~'));
    }
}
