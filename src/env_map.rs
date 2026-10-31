// issue #8: 依赖 sidecar (PG/Redis/Qdrant) 的 native 服务需显式注入连接 env。
// 本模块按服务名给出所需连接变量, 并从 .env / 环境变量解析具体值。
// fail-closed: 必需值解析不出 → 返回 Err, 拒绝启动该服务 (非静默降级)。
// issue #78: 注入不再 gate 在 compose 开关上 — 此前两处调用点都先判
// compose.enabled, 默认 (compose off) 时 fusion-cowork 静默跑 SQLite、
// fusion-rag 静默跑 :memory:。现在两个平面 (bare-metal compose off /
// 容器化 compose on) 均注入; Qdrant 例外: 本地 6333 不可达时跳过
// QDRANT_URL 注入 (rag 有 :memory: 兜底), 不 fail-closed。

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
// issue #85: FUSION_CLIENT_KEY 反转默认 — 全部托管 native 服务统一注入,
// 仅明确排除的服务例外。手工 allowlist 已三次漏登 (fusion-rag 2026-10-31
// 夜 / ai-trysuite #84 / fusion-code-modenization #85), 每个新 gateway
// 消费服务都会静默降级 (回退 daemon env 快照里已轮换失效的旧别名
// FUSION_MLX_API_KEY → 401) 直到有人注意到。不读该 env 的服务直接忽略,
// 注入无副作用; fail-fast `:?` 消费方由注入满足; 解析链不变
// (进程 env → env_file → gateway config 兜底, 全缺 Err 拒启动)。
// 排除表 (不注入 FUSION_CLIENT_KEY):
// - fusion-gateway: 上游 config.go 刻意不 BindEnv FUSION_CLIENT_KEY —
//   ambient 客户端 key 会经 api_key_env 引用覆写 mlx backend key
//   (fusion-gateway#412), 注入属于越权。
// - fusion-mlx: 推理服务端, 鉴权单一真源是 settings.json (strip_env_for
//   同理), 客户端 key 不该进服务端 env。
// - 纯静态托管 (http.server web/dist, 不打 gateway)。
fn shell_vars(svc: &str) -> Vec<&'static str> {
    let mut vars: Vec<&'static str> = Vec::new();
    if !matches!(
        svc,
        "fusion-gateway"
            | "fusion-mlx"
            | "customer-insight"
            | "customer-insight-api"
            | "ai-apparelmockup-web"
    ) {
        vars.push("FUSION_CLIENT_KEY");
    }
    match svc {
        // issue #54: 上游已迁 conventions §6 新名 — cowork shell_registry.py 读
        // FUSION_SHELL_GATEWAY_URL + FUSION_ADMIN_KEY (URL 尚未迁 BASE_URL,
        // 迁移期保留旧名); plugins console v0.4.7 (fusion-plugins-ecosystem#39)
        // 读 FUSION_GATEWAY_BASE_URL + FUSION_ADMIN_KEY, 不再认旧名。
        "fusion-cowork-web" => vars.extend(["FUSION_SHELL_GATEWAY_URL", "FUSION_ADMIN_KEY"]),
        "fusion-plugins-ecosystem-console" => {
            vars.extend(["FUSION_GATEWAY_BASE_URL", "FUSION_ADMIN_KEY"])
        }
        "fusion-trainer-bridge" => vars.push("FUSION_TRAINER_BRIDGE_KEY"),
        _ => {}
    }
    vars
}

// 该服务是否需要 shell 注册 env (调用方无条件注入, 不 gate 在 compose 上)。
// issue #85 反转默认后, 除排除表外的全部服务都需要 (FUSION_CLIENT_KEY)。
pub fn needs_shell_env(svc: &str) -> bool {
    !shell_vars(svc).is_empty()
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
// issue #78: QDRANT_URL 注入前先探本地 6333 可达性 — qdrant 容器没起时
// 跳过该 key (fusion-rag 上游有 :memory: 兜底), 不让整个注入失败。
// issue #78 补: FUSION_PG_PASSWORD 缺失时探本地 5432 — native 平面 (compose_on=false)
// 是 brew trust auth (fusion-db-conventions-1030 §1, 根本没有密码), 可达则注入
// 无密码连接串; 不可达则跳过 DATABASE_URL (应用走 SQLite 兜底)。
// compose 平面 (compose_on=true) sidecar PG 是密码认证, 缺密码 = 配置错误,
// 保持 fail-closed 拒绝 (不静默降级到 trust 串打错库)。
pub fn native_env_for(
    svc: &str,
    env_file: &std::path::Path,
    compose_on: bool,
) -> Result<HashMap<String, String>> {
    let qdrant_up = qdrant_reachable();
    let pg_up = pg_reachable();
    native_env_for_opts(svc, env_file, compose_on, pg_up, qdrant_up)
}

// issue #78: 可测版本 — pg_up/qdrant_up 由测试注入 (不做真实网络探测)。
pub fn native_env_for_opts(
    svc: &str,
    env_file: &std::path::Path,
    compose_on: bool,
    pg_up: bool,
    qdrant_up: bool,
) -> Result<HashMap<String, String>> {
    let Some(keys) = required_vars(svc) else {
        return Ok(HashMap::new());
    };
    // PG 连接串依赖的公共片段
    let pg_user = resolve("FUSION_PG_USER", env_file);
    let pg_pw = resolve("FUSION_PG_PASSWORD", env_file);
    let pg_host = "localhost";
    let pg_port = "5432";

    let mut out = HashMap::new();
    let mut skipped = false;
    for k in keys {
        let val = match *k {
            "DATABASE_URL" => {
                let db = pg_db_for(svc, env_file, compose_on);
                match (&pg_pw, &pg_user) {
                    (Some(pw), user) => {
                        let user = user.as_deref().unwrap_or("fusion");
                        format!("postgresql://{user}:{pw}@{pg_host}:{pg_port}/{db}")
                    }
                    // issue #78 补: compose 平台 sidecar PG 密码认证, 缺密码 =
                    // 配置错误, 保持 fail-closed (check_env 之外的二道防线)。
                    (None, _) if compose_on => {
                        bail!(
                            "FUSION_PG_PASSWORD 缺失 — 无法为 {svc} 构造 DATABASE_URL (compose 平面 fail-closed)"
                        );
                    }
                    // issue #78 补: native 平面 brew trust auth 无密码 —
                    // PG 可达即注入无密码串 (FUSION_PG_USER 缺省省略 user
                    // 段, libpq/asyncpg 默认 OS user); 不可达跳过让应用兜底。
                    (None, _) if pg_up => {
                        tracing::info!(
                            svc,
                            "FUSION_PG_PASSWORD 未设但 5432 可达 (trust auth), 注入无密码 DATABASE_URL"
                        );
                        match &pg_user {
                            Some(u) => format!("postgresql://{u}@{pg_host}:{pg_port}/{db}"),
                            None => format!("postgresql://{pg_host}:{pg_port}/{db}"),
                        }
                    }
                    (None, _) => {
                        skipped = true;
                        tracing::info!(
                            svc,
                            "FUSION_PG_PASSWORD 未设且 5432 不可达, 跳过 DATABASE_URL 注入"
                        );
                        continue;
                    }
                }
            }
            "REDIS_URL" => "redis://localhost:6379/1".to_string(),
            "QDRANT_URL" => {
                if !qdrant_up {
                    skipped = true;
                    tracing::info!(
                        svc,
                        "qdrant 127.0.0.1:6333 不可达, 跳过 QDRANT_URL 注入 (rag 走 :memory: 兜底)"
                    );
                    continue;
                }
                "http://localhost:6333".to_string()
            }
            other => bail!("env_map: 未知连接 key {other} for {svc}"),
        };
        out.insert((*k).to_string(), val);
    }
    // 被跳过的 key (qdrant down / pg down) 允许空 map (应用有兜底);
    // 其余情况空 map = 解析异常, fail-closed。
    if out.is_empty() && !skipped {
        bail!("env_map: {svc} 所需连接变量解析为空 (fail-closed)");
    }
    tracing::info!(svc, keys = ?keys, "native env 注入解析完成");
    Ok(out)
}

// issue #78: 本地 qdrant (127.0.0.1:6333) 可达性探测, 300ms 超时。
// 探测失败不阻断注入 (调用方跳过 QDRANT_URL, rag 走上游兜底)。
fn qdrant_reachable() -> bool {
    tcp_reachable(6333, "qdrant")
}

// issue #78 补: 本地 PG (127.0.0.1:5432) 可达性探测 (trust auth 佐证)。
fn pg_reachable() -> bool {
    tcp_reachable(5432, "postgres")
}

fn tcp_reachable(port: u16, what: &str) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)) {
        Ok(_) => true,
        Err(e) => {
            tracing::info!(err = %e, what, port, "探测失败, 对应连接 env 将跳过注入");
            false
        }
    }
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
    let keys = shell_vars(svc);
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    for k in &keys {
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
// issue #78 补: db 名回退按平面分 — compose sidecar pg-init 建的是去前缀短名
// (cowork/identity/rag, 见 docker-compose.sidecar.yml FUSION_PG_DATABASES),
// native brew PG 建的是下划线全名 (fusion_cowork/fusion_identity, 见
// fusion-db-conventions-1030)。FUSION_*_DB env 覆盖两平面均优先。
fn pg_db_for(svc: &str, env_file: &std::path::Path, compose_on: bool) -> String {
    let fallback = if compose_on {
        svc.replace("fusion-", "")
    } else {
        svc.replace('-', "_")
    };
    let key = match svc {
        "fusion-model-hub" => "FUSION_MODELHUB_DB",
        "fusion-cowork" => "FUSION_COWORK_DB",
        "fusion-identity" => "FUSION_IDENTITY_DB",
        "fusion-rag" => "FUSION_RAG_DB",
        _ => return fallback,
    };
    resolve(key, env_file).unwrap_or(fallback)
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
        let env = native_env_for("fusion-identity", f.path(), false).unwrap();
        let url = env.get("DATABASE_URL").unwrap();
        assert!(
            url.contains("appuser:secret123@localhost:5432/fusion_identity"),
            "got {url}"
        );
    }

    // issue #78 补: FUSION_PG_PASSWORD 缺失 + PG 可达 (brew trust auth) → 无密码连接串
    #[test]
    fn test_native_env_trust_auth_when_pg_up() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_USER=fusion\n");
        let env = native_env_for_opts("fusion-identity", f.path(), false, true, true).unwrap();
        assert_eq!(
            env.get("DATABASE_URL").unwrap(),
            "postgresql://fusion@localhost:5432/fusion_identity"
        );
    }

    // issue #78 补: 密码与 FUSION_PG_USER 双缺 + PG 可达 → 省略 user 段 (libpq 默认 OS user)
    #[test]
    fn test_native_env_trust_auth_no_user() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        unsafe { std::env::remove_var("FUSION_PG_USER") };
        let f = env_file("");
        let env = native_env_for_opts("fusion-identity", f.path(), false, true, true).unwrap();
        assert_eq!(
            env.get("DATABASE_URL").unwrap(),
            "postgresql://localhost:5432/fusion_identity"
        );
    }

    // issue #78 补: 密码缺失 + PG 不可达 → 跳过 DATABASE_URL (应用走 SQLite 兜底),
    // 不让整个注入失败; REDIS_URL 仍无条件注入
    #[test]
    fn test_native_env_pg_down_skips_db_keeps_redis() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("");
        let env = native_env_for_opts("fusion-cowork", f.path(), false, false, true).unwrap();
        assert!(
            !env.contains_key("DATABASE_URL"),
            "PG 不可达且无密码不应注入 DATABASE_URL"
        );
        assert!(env.contains_key("REDIS_URL"), "REDIS_URL 不受 PG 状态影响");
    }

    // issue #78 补: compose 平面缺密码 → 仍 fail-closed Err (sidecar PG 密码认证,
    // 静默降级 trust 串会打错库)
    #[test]
    fn test_compose_plane_missing_password_fail_closed() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_USER=fusion\n");
        let r = native_env_for_opts("fusion-identity", f.path(), true, true, true);
        assert!(r.is_err(), "compose 平面缺密码应 fail-closed 拒绝");
        assert!(
            r.unwrap_err().to_string().contains("FUSION_PG_PASSWORD"),
            "err 应点名缺失 key"
        );
    }

    // 非依赖服务 → 空注入 (保留现状)
    #[test]
    fn test_native_env_unknown_service_no_injection() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=x\n");
        let env = native_env_for("fusion-mlx", f.path(), false).unwrap();
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
        let env = native_env_for("fusion-identity", f.path(), false).unwrap();
        let url = env.get("DATABASE_URL").unwrap();
        assert!(url.contains("fromproc"), "进程 env 应优先: {url}");
    }

    // cowork 需 DATABASE_URL + REDIS_URL 两条
    #[test]
    fn test_cowork_gets_db_and_redis() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=pw\n");
        let env = native_env_for("fusion-cowork", f.path(), false).unwrap();
        assert!(env.contains_key("DATABASE_URL"));
        assert!(env.contains_key("REDIS_URL"));
        assert!(env["REDIS_URL"].contains("6379"));
    }

    // rag 需 QDRANT_URL (issue #78: 用 opts 可测版本, 不做真实网络探测)
    #[test]
    fn test_rag_gets_qdrant() {
        let f = env_file("");
        let env = native_env_for_opts("fusion-rag", f.path(), false, true, true).unwrap();
        assert_eq!(env.get("QDRANT_URL").unwrap(), "http://localhost:6333");
    }

    // issue #78: qdrant 不可达 → 跳过 QDRANT_URL 注入, rag 允许空 map (兜底 :memory:)
    #[test]
    fn test_rag_qdrant_down_skips_key() {
        let f = env_file("");
        let env = native_env_for_opts("fusion-rag", f.path(), false, true, false).unwrap();
        assert!(
            !env.contains_key("QDRANT_URL"),
            "qdrant 不可达不应注入 QDRANT_URL"
        );
        assert!(env.is_empty(), "rag 唯一 key 被跳过时允许空 map");
    }

    // issue #78 补: db 名回退按平面分 — compose=短名 (pg-init), native=下划线全名 (brew)
    #[test]
    fn test_pg_db_name_per_plane() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        unsafe { std::env::remove_var("FUSION_COWORK_DB") };
        let f = env_file("FUSION_PG_PASSWORD=pw\n");
        let native = native_env_for_opts("fusion-cowork", f.path(), false, true, true).unwrap();
        assert!(
            native["DATABASE_URL"].ends_with("/fusion_cowork"),
            "got {}",
            native["DATABASE_URL"]
        );
        let compose = native_env_for_opts("fusion-cowork", f.path(), true, true, true).unwrap();
        assert!(
            compose["DATABASE_URL"].ends_with("/cowork"),
            "got {}",
            compose["DATABASE_URL"]
        );
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
        // issue #85: 反转默认后 cowork-web 也注入 FUSION_CLIENT_KEY, 测试显式供 key
        let _ck = EnvGuard::set("FUSION_CLIENT_KEY", "ck-proc");
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
        let f = env_file("FUSION_ADMIN_KEY=fromfile\nFUSION_CLIENT_KEY=ck-file\n");
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
        let f = env_file("FUSION_CLIENT_KEY=ck-file\n");
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
        // issue #55: 批量登记的 FastAPI 条目同注入
        assert!(needs_shell_env("ai-ads-videobuilder"));
        assert!(needs_shell_env("ai-charactervoice"));
        // issue #85 反转默认: 排除表外全部注入 — 静态托管/gateway/mlx 除外
        assert!(!needs_shell_env("customer-insight"));
        assert!(!needs_shell_env("customer-insight-api"));
        assert!(!needs_shell_env("ai-apparelmockup-web"));
        assert!(!needs_shell_env("fusion-gateway"));
        assert!(!needs_shell_env("fusion-mlx"));
        // 未知/未登记服务同样注入 (不读则忽略)
        assert!(needs_shell_env("ai-family"));
        assert!(needs_shell_env("fusion-rag"));
        assert!(needs_shell_env("ai-trysuite"));
        // issue #85 直接触发服务: /health gateway 检查 401
        assert!(needs_shell_env("fusion-code-modenization"));
        assert!(needs_shell_env("fusion-unknown-new-service"));
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
