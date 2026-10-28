// issue #8: compose enabled 时, 依赖 sidecar 的 native 服务需显式注入连接 env。
// 本模块按服务名给出所需连接变量, 并从 .env / 环境变量解析具体值。
// fail-closed: 必需值解析不出 → 返回 Err, 拒绝启动该服务 (非静默降级)。
// compose disabled 时 (单节点开发), 调用方跳过本模块, 服务照旧读自身 env。

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

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

// issue #29: shell-ecosystem 模块服务的注册 env。
// issue #45: business 平台服务的 FUSION_CLIENT_KEY (gateway 客户端 key, 约定 1)
// 也走此注入通道 — 三个 arg-less start.sh 均硬性要求该 key, 缺失 fail-fast。
// 这些注入不受 compose 开关控制 (与 sidecar 连接 env 不同)。
fn shell_vars(svc: &str) -> Option<&'static [&'static str]> {
    match svc {
        // issue #54: 上游已迁 conventions §6 新名 — cowork shell_registry.py 读
        // FUSION_SHELL_GATEWAY_URL + FUSION_ADMIN_KEY (URL 尚未迁 BASE_URL,
        // 迁移期保留旧名); plugins console v0.4.7 (fusion-plugins-ecosystem#39)
        // 读 FUSION_GATEWAY_BASE_URL + FUSION_ADMIN_KEY, 不再认旧名。
        "fusion-cowork-web" => Some(&["FUSION_SHELL_GATEWAY_URL", "FUSION_ADMIN_KEY"]),
        "fusion-plugins-ecosystem-console" => {
            Some(&["FUSION_GATEWAY_BASE_URL", "FUSION_ADMIN_KEY"])
        }
        "fusion-trainer-bridge" => Some(&["FUSION_TRAINER_BRIDGE_KEY"]),
        // issue #55: 2026-10-27/28 批量登记的 FastAPI business 服务同样消费
        // gateway 客户端 key (约定 1); 纯静态托管 (customer-insight ×2,
        // apparelmockup-web) 不打 gateway, 不注入。
        "ai-fit"
        | "ai-speech-exam"
        | "ai-apparelmockup"
        | "ai-ads-videobuilder"
        | "ai-doc-automation"
        | "ai-information-search"
        | "ai-OperaBase"
        | "ai-charactervoice"
        | "ai-charactervoice-session" => Some(&["FUSION_CLIENT_KEY"]),
        _ => None,
    }
}

// 该服务是否需要 shell 注册 env (调用方无条件注入, 不 gate 在 compose 上)。
pub fn needs_shell_env(svc: &str) -> bool {
    shell_vars(svc).is_some()
}

// issue #39/#40: 启动子进程前须剥离的 env 变量名。
// fusion-mlx 的 start.sh 中 FUSION_MLX_API_KEY env 优先级高于 settings.json,
// daemon env 快照 (来自 monorepo .env) 会钉死旧 key → supervisor 每次重启
// 都让新实例用旧 key 鉴权, 所有 settings.json 客户端 401。
// 剥离后 start.sh 每次现场从 ~/.fusion-mlx/settings.json 解析, 单一真源。
// compose check_env 仍要求 daemon 自身持有该 var (gate compose 容器, 不受影响)。
pub fn strip_env_for(svc: &str) -> &'static [&'static str] {
    match svc {
        "fusion-mlx" => &["FUSION_MLX_API_KEY"],
        // issue #45: fusion-health 的 config.py env 优先级高于 ~/.fusion-mlx/
        // settings.json — daemon 快照里的旧 key 让 backend 探活 401 (503 degraded)。
        // 剥离后其配置回退解析 settings.json, 与 mlx 实例的 key 单一真源一致。
        "fusion-health" => &["FUSION_MLX_API_KEY"],
        _ => &[],
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

// issue #29: 解析某服务的 shell 注册 env。
// 解析顺序 (与 trainer-bridge.sh 自读 gateway config 的模式一致):
//   进程 env → env_file → 兜底
// - FUSION_SHELL_GATEWAY_URL / FUSION_GATEWAY_BASE_URL: 兜底 http://127.0.0.1:11432
//   (本地网关固定端口; issue #54 起按消费方分用新旧 URL 名)
// - FUSION_ADMIN_KEY: 兜底读 fusion-gateway config.yaml web_shell.admin_key
//   (只读, 不改写上游), 路径由 FUSION_GATEWAY_CONFIG 覆盖
// - FUSION_TRAINER_BRIDGE_KEY: 无兜底, env/env_file 均无 → fail-closed Err
//   (bridge 自身要求此 key, 缺了起不来, 早失败早告警)
// 非 shell 模块服务 → 空 map。
pub fn shell_env_for(svc: &str, env_file: &Path) -> Result<HashMap<String, String>> {
    let Some(keys) = shell_vars(svc) else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for k in keys {
        match *k {
            "FUSION_SHELL_GATEWAY_URL" => {
                let v = resolve(k, env_file).unwrap_or_else(|| "http://127.0.0.1:11432".into());
                out.insert(k.to_string(), v);
            }
            // issue #54: admin key 发布到新名 FUSION_ADMIN_KEY (conventions §1),
            // 兜底仍只读 gateway config web_shell.admin_key, 不改上游。
            "FUSION_ADMIN_KEY" => {
                let v = match resolve(k, env_file) {
                    Some(v) => v,
                    None => read_gateway_admin_key().with_context(|| {
                        format!(
                            "FUSION_ADMIN_KEY 缺失 — env/.env 均无且 gateway config 不可读, \
                                 无法为 {svc} 注册 shell (fail-closed)"
                        )
                    })?,
                };
                out.insert(k.to_string(), v);
            }
            // issue #54: plugins console v0.4.7 读的新 URL 变量, 兜底同本地网关。
            "FUSION_GATEWAY_BASE_URL" => {
                let v = resolve(k, env_file).unwrap_or_else(|| "http://127.0.0.1:11432".into());
                out.insert(k.to_string(), v);
            }
            "FUSION_TRAINER_BRIDGE_KEY" => {
                let v = resolve(k, env_file).with_context(|| {
                    format!(
                        "FUSION_TRAINER_BRIDGE_KEY 缺失 — bridge 拒绝启动 (fail-closed, svc {svc})"
                    )
                })?;
                out.insert(k.to_string(), v);
            }
            // issue #45: business 平台 gateway 客户端 key — 解析顺序同 shell 注册:
            // 进程 env → env_file → 兜底读 gateway config auth.api_keys (优先 name=admin
            // 且 allowed_models 含 * 的全模型 key; 否则第一条)。三服务 start.sh 均
            // `:?` 硬性要求, 全缺 → fail-closed Err (早失败早告警)。
            "FUSION_CLIENT_KEY" => {
                let v = match resolve(k, env_file) {
                    Some(v) => v,
                    None => read_gateway_client_key().with_context(|| {
                        format!(
                            "FUSION_CLIENT_KEY 缺失 — env/.env 均无且 gateway config 不可读, \
                             无法为 {svc} 启动 (fail-closed)"
                        )
                    })?,
                };
                out.insert(k.to_string(), v);
            }
            other => bail!("env_map: 未知 shell 注册 key {other} for {svc}"),
        }
    }
    tracing::info!(svc, keys = ?keys, "shell 注册 env 解析完成");
    Ok(out)
}

// 只读 fusion-gateway config.yaml 的 web_shell.admin_key。
// 路径: FUSION_GATEWAY_CONFIG 覆盖, 默认 ~/fusion/fusion-gateway/config.yaml。
fn read_gateway_admin_key() -> Result<String> {
    let path = std::env::var("FUSION_GATEWAY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join("fusion/fusion-gateway/config.yaml")
        });
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读 gateway config 失败: {}", path.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 gateway config 失败: {}", path.display()))?;
    let key = cfg
        .get("web_shell")
        .and_then(|w| w.get("admin_key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .with_context(|| format!("gateway config 无 web_shell.admin_key: {}", path.display()))?;
    Ok(key.to_string())
}

// issue #45: 只读 fusion-gateway config.yaml 的 auth.api_keys, 取 name=admin 的
// key (全模型通配, 业务后端打 gateway 的 LLM 通道需要), 无 admin 则取第一条。
// 路径: FUSION_GATEWAY_CONFIG 覆盖, 默认 ~/fusion/fusion-gateway/config.yaml。
fn read_gateway_client_key() -> Result<String> {
    let path = std::env::var("FUSION_GATEWAY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join("fusion/fusion-gateway/config.yaml")
        });
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读 gateway config 失败: {}", path.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 gateway config 失败: {}", path.display()))?;
    let keys = cfg
        .get("auth")
        .and_then(|a| a.get("api_keys"))
        .and_then(|k| k.as_sequence())
        .with_context(|| format!("gateway config 无 auth.api_keys: {}", path.display()))?;
    let admin = keys
        .iter()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("admin"));
    let entry = admin
        .or_else(|| keys.first())
        .with_context(|| format!("gateway config auth.api_keys 为空: {}", path.display()))?;
    let key = entry
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .with_context(|| format!("gateway config api_keys 条目无 key: {}", path.display()))?;
    Ok(key.to_string())
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

    // ---- issue #29: shell 注册 env ----

    // 进程 env 直接给 admin key → 注入成功, 不读 gateway config。
    // issue #54: 注入名迁 FUSION_ADMIN_KEY; cowork 仍读旧 URL 名。
    #[test]
    fn test_shell_env_from_process_env() {
        let _g = env_lock().lock().unwrap();
        let _c = EnvGuard::set("FUSION_ADMIN_KEY", "fromproc");
        let f = env_file("");
        let env = shell_env_for("fusion-cowork-web", f.path()).unwrap();
        assert_eq!(env.get("FUSION_ADMIN_KEY").unwrap(), "fromproc");
        assert_eq!(
            env.get("FUSION_SHELL_GATEWAY_URL").unwrap(),
            "http://127.0.0.1:11432",
            "URL 未配置应兜底本地网关"
        );
    }

    // .env 给 admin key → 进程 env 缺失时从文件解析;
    // plugins console 读新 URL 名 FUSION_GATEWAY_BASE_URL (issue #54)
    #[test]
    fn test_shell_env_from_env_file() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_ADMIN_KEY") };
        unsafe { std::env::remove_var("FUSION_GATEWAY_BASE_URL") };
        let f = env_file("FUSION_ADMIN_KEY=fromfile\n");
        let env = shell_env_for("fusion-plugins-ecosystem-console", f.path()).unwrap();
        assert_eq!(env.get("FUSION_ADMIN_KEY").unwrap(), "fromfile");
        assert_eq!(
            env.get("FUSION_GATEWAY_BASE_URL").unwrap(),
            "http://127.0.0.1:11432",
            "console URL 未配置应兜底本地网关"
        );
    }

    // admin key 全缺 + gateway config 不可读 (指到不存在路径) → fail-closed Err
    #[test]
    fn test_shell_env_fail_closed_no_admin_key() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_ADMIN_KEY") };
        let _c = EnvGuard::set(
            "FUSION_GATEWAY_CONFIG",
            "/nonexistent/fusion-gateway-config-test.yaml",
        );
        let f = env_file("");
        let r = shell_env_for("fusion-cowork-web", f.path());
        assert!(r.is_err(), "admin key 全缺应 fail-closed");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("FUSION_ADMIN_KEY"),
            "err 应点名缺失 key: {msg}"
        );
    }

    // trainer-bridge key 无兜底: env/.env 均无 → fail-closed Err
    #[test]
    fn test_shell_env_bridge_key_fail_closed() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_TRAINER_BRIDGE_KEY") };
        let f = env_file("");
        let r = shell_env_for("fusion-trainer-bridge", f.path());
        assert!(r.is_err(), "bridge key 缺失应 fail-closed");
        assert!(
            r.unwrap_err()
                .to_string()
                .contains("FUSION_TRAINER_BRIDGE_KEY")
        );
    }

    // trainer-bridge key 从 .env 解析成功
    #[test]
    fn test_shell_env_bridge_key_from_file() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_TRAINER_BRIDGE_KEY") };
        let f = env_file("FUSION_TRAINER_BRIDGE_KEY=bridge-k\n");
        let env = shell_env_for("fusion-trainer-bridge", f.path()).unwrap();
        assert_eq!(env.get("FUSION_TRAINER_BRIDGE_KEY").unwrap(), "bridge-k");
        assert!(
            !env.contains_key("FUSION_SHELL_GATEWAY_URL"),
            "bridge 只需自身 key"
        );
    }

    // 非 shell 模块服务 → 空 map (保留现状)
    #[test]
    fn test_shell_env_unknown_service_empty() {
        let f = env_file("");
        let env = shell_env_for("fusion-mlx", f.path()).unwrap();
        assert!(env.is_empty());
        assert!(!needs_shell_env("fusion-mlx"));
        assert!(needs_shell_env("fusion-cowork-web"));
        assert!(needs_shell_env("fusion-trainer-bridge"));
    }

    // issue #39/#40: fusion-mlx 子进程剥离 FUSION_MLX_API_KEY (防旧 key 钉死),
    // 其它服务不剥离。issue #45: fusion-health 同剥离 (config.py env 优先于
    // settings.json, 旧 key 让 backend 探活 401)。
    #[test]
    fn test_strip_env_for_mlx_only() {
        assert_eq!(strip_env_for("fusion-mlx"), &["FUSION_MLX_API_KEY"]);
        assert_eq!(strip_env_for("fusion-health"), &["FUSION_MLX_API_KEY"]);
        assert!(strip_env_for("fusion-gateway").is_empty());
        assert!(strip_env_for("fusion-rag").is_empty());
    }

    // ---- issue #45: business 平台 FUSION_CLIENT_KEY 注入 ----

    // 进程 env 直接给 key → 注入成功, 不读 gateway config
    #[test]
    fn test_client_key_from_process_env() {
        let _g = env_lock().lock().unwrap();
        let _c = EnvGuard::set("FUSION_CLIENT_KEY", "fromproc");
        let f = env_file("");
        let env = shell_env_for("ai-fit", f.path()).unwrap();
        assert_eq!(env.get("FUSION_CLIENT_KEY").unwrap(), "fromproc");
        assert!(needs_shell_env("ai-fit"));
        assert!(needs_shell_env("ai-speech-exam"));
        assert!(needs_shell_env("ai-apparelmockup"));
        // issue #55: 批量登记的 FastAPI 条目同注入; 纯静态托管不注入
        assert!(needs_shell_env("ai-ads-videobuilder"));
        assert!(needs_shell_env("ai-charactervoice"));
        assert!(!needs_shell_env("customer-insight"));
        assert!(!needs_shell_env("ai-apparelmockup-web"));
        // ai-family 源码不消费该 key (自建鉴权), 不注入
        assert!(!needs_shell_env("ai-family"));
    }

    // .env 给 key → 进程 env 缺失时从文件解析
    #[test]
    fn test_client_key_from_env_file() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_CLIENT_KEY") };
        let f = env_file("FUSION_CLIENT_KEY=fromfile\n");
        let env = shell_env_for("ai-speech-exam", f.path()).unwrap();
        assert_eq!(env.get("FUSION_CLIENT_KEY").unwrap(), "fromfile");
    }

    // 全缺 + gateway config 不可读 → fail-closed Err, 点名缺失 key
    #[test]
    fn test_client_key_fail_closed_no_source() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_CLIENT_KEY") };
        let _c = EnvGuard::set(
            "FUSION_GATEWAY_CONFIG",
            "/nonexistent/fusion-gateway-config-test.yaml",
        );
        let f = env_file("");
        let r = shell_env_for("ai-apparelmockup", f.path());
        assert!(r.is_err(), "client key 全缺应 fail-closed");
        assert!(
            r.unwrap_err().to_string().contains("FUSION_CLIENT_KEY"),
            "err 应点名缺失 key"
        );
    }

    // 兜底读 gateway config auth.api_keys — 优先 name=admin 条目
    #[test]
    fn test_client_key_gateway_fallback_prefers_admin() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_CLIENT_KEY") };
        let f = env_file("");
        std::fs::write(
            f.path(),
            "auth:\n  api_keys:\n  - name: demo\n    key: demokey123\n  - name: admin\n    key: adminkey456\n",
        )
        .unwrap();
        let _c = EnvGuard::set("FUSION_GATEWAY_CONFIG", f.path().to_str().unwrap());
        let env = shell_env_for("ai-fit", f.path()).unwrap();
        assert_eq!(env.get("FUSION_CLIENT_KEY").unwrap(), "adminkey456");
    }

    // env guard (RAII 恢复进程 env), 与 config.rs EnvGuard 同构
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
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
