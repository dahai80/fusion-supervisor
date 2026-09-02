// issue #8: compose enabled 时, 依赖 sidecar 的 native 服务需显式注入连接 env。
// 本模块按服务名给出所需连接变量, 并从 .env / 环境变量解析具体值。
// fail-closed: 必需值解析不出 → 返回 Err, 拒绝启动该服务 (非静默降级)。
// compose disabled 时 (单节点开发), 调用方跳过本模块, 服务照旧读自身 env。

use anyhow::{Context, Result, bail};
use std::collections::HashMap;

// 依赖 sidecar 的 native 服务及其所需连接 env key 列表。
// 未在表中的服务 → 无注入, 保留现状。
fn required_vars(svc: &str) -> Option<&'static [&'static str]> {
    match svc {
        "fusion-identity" => Some(&["DATABASE_URL"]),
        "fusion-rag" => Some(&["QDRANT_URL"]),
        "fusion-cowork" => Some(&["DATABASE_URL", "REDIS_URL"]),
        "fusion-model-hub" => Some(&["DATABASE_URL", "REDIS_URL"]),
        _ => None,
    }
}

// 返回服务所需的全部连接变量名 (供调用方判断是否需注入)。
pub fn needs_injection(svc: &str) -> bool {
    required_vars(svc).is_some()
}

// 解析单个环境变量值: 先查进程 env, 再查 env_file, 均无则用默认。
fn resolve(key: &str, env_file: &std::path::Path) -> Option<String> {
    if let Ok(v) = std::env::var(key)
        && !v.is_empty()
    {
        return Some(v);
    }
    read_env_file(env_file).get(key).cloned()
}

// 解析 .env 文件为 HashMap (key → value), 跳过注释/空行。
fn read_env_file(path: &std::path::Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return map,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

// 为某服务解析需注入的连接 env (name → value)。
// fail-closed: 任一必需值无法解析 → Err (带服务名与缺失 key)。
// 非依赖服务 (required_vars=None) → 空 map (无注入, 不报错)。
pub fn native_env_for(svc: &str, env_file: &std::path::Path) -> Result<HashMap<String, String>> {
    let Some(keys) = required_vars(svc) else {
        return Ok(HashMap::new());
    };
    // PG 连接串依赖的公共片段
    let pg_user = resolve("FUSION_PG_USER", env_file).unwrap_or_else(|| "fusion".into());
    let pg_pw = resolve("FUSION_PG_PASSWORD", env_file);
    let pg_host = "localhost";
    let pg_port = "5432";

    let mut out = HashMap::new();
    for k in keys {
        let val = match *k {
            "DATABASE_URL" => {
                let pw = pg_pw.as_ref().with_context(|| {
                    format!(
                        "FUSION_PG_PASSWORD 缺失 — 无法为 {svc} 构造 DATABASE_URL (fail-closed)"
                    )
                })?;
                let db = pg_db_for(svc, env_file);
                format!("postgresql://{pg_user}:{pw}@{pg_host}:{pg_port}/{db}")
            }
            "REDIS_URL" => "redis://localhost:6379/1".to_string(),
            "QDRANT_URL" => "http://localhost:6333".to_string(),
            other => bail!("env_map: 未知连接 key {other} for {svc}"),
        };
        out.insert((*k).to_string(), val);
    }
    if out.is_empty() {
        bail!("env_map: {svc} 所需连接变量解析为空 (fail-closed)");
    }
    tracing::info!(svc, keys = ?keys, "native env 注入解析完成");
    Ok(out)
}

// 据服务名决定 PG 库名: modelhub/cowork 用各自 env, identity/rag 用服务名。
fn pg_db_for(svc: &str, env_file: &std::path::Path) -> String {
    let key = match svc {
        "fusion-model-hub" => "FUSION_MODELHUB_DB",
        "fusion-cowork" => "FUSION_COWORK_DB",
        "fusion-identity" => "FUSION_IDENTITY_DB",
        "fusion-rag" => "FUSION_RAG_DB",
        _ => return svc.replace("fusion-", ""),
    };
    resolve(key, env_file).unwrap_or_else(|| svc.replace("fusion-", ""))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::NamedTempFile;

    // 进程 env 是全局共享的, 测试并行会互相污染。此锁强制 env_map 测试串行。
    // pub(crate): supervisor/mod.rs env 测试共用此锁, 避免双锁竞态 (#8 rebased onto #7)。
    pub(crate) fn env_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    fn env_file(content: &str) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), content).unwrap();
        f
    }

    // identity + .env 有 PG 密码 → 解析出 DATABASE_URL
    #[test]
    fn test_native_env_injected_for_identity() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=secret123\nFUSION_PG_USER=appuser\n");
        let env = native_env_for("fusion-identity", f.path()).unwrap();
        let url = env.get("DATABASE_URL").unwrap();
        assert!(
            url.contains("appuser:secret123@localhost:5432/identity"),
            "got {url}"
        );
    }

    // FUSION_PG_PASSWORD 缺失 → fail-closed Err, 不静默
    #[test]
    fn test_native_env_fail_closed_missing_password() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_USER=fusion\n");
        let r = native_env_for("fusion-identity", f.path());
        assert!(r.is_err(), "缺密码应 fail-closed 拒绝");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("FUSION_PG_PASSWORD"),
            "err 应点名缺失 key: {msg}"
        );
    }

    // 非依赖服务 → 空注入 (保留现状)
    #[test]
    fn test_native_env_unknown_service_no_injection() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=x\n");
        let env = native_env_for("fusion-mlx", f.path()).unwrap();
        assert!(env.is_empty(), "非依赖服务不应注入 env");
    }

    // 进程 env 优先于 env_file
    #[test]
    fn test_process_env_precedence() {
        let _g = env_lock().lock().unwrap();
        // edition 2024: set_var/remove_var 需 unsafe (见 CLAUDE.md EnvGuard)
        unsafe { std::env::set_var("FUSION_PG_PASSWORD", "fromproc") };
        // RAII: panic 也清掉进程 env, 避免污染其它测试
        struct EnvCleanup;
        impl Drop for EnvCleanup {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
            }
        }
        let _cleanup = EnvCleanup;
        let f = env_file("FUSION_PG_PASSWORD=fromfile\n");
        let env = native_env_for("fusion-identity", f.path()).unwrap();
        let url = env.get("DATABASE_URL").unwrap();
        assert!(url.contains("fromproc"), "进程 env 应优先: {url}");
    }

    // cowork 需 DATABASE_URL + REDIS_URL 两条
    #[test]
    fn test_cowork_gets_db_and_redis() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=pw\n");
        let env = native_env_for("fusion-cowork", f.path()).unwrap();
        assert!(env.contains_key("DATABASE_URL"));
        assert!(env.contains_key("REDIS_URL"));
        assert!(env["REDIS_URL"].contains("6379"));
    }

    // rag 需 QDRANT_URL
    #[test]
    fn test_rag_gets_qdrant() {
        let f = env_file("");
        let env = native_env_for("fusion-rag", f.path()).unwrap();
        assert_eq!(env.get("QDRANT_URL").unwrap(), "http://localhost:6333");
    }

    // read_env_file 跳过注释与空行, 解析 key=value
    #[test]
    fn test_read_env_file_skips_comments() {
        let f = env_file("# comment\n\nKEY=val\nOTHER=2\n");
        let map = read_env_file(f.path());
        assert_eq!(map.get("KEY").unwrap(), "val");
        assert_eq!(map.get("OTHER").unwrap(), "2");
        assert!(!map.contains_key("# comment"));
    }
}
