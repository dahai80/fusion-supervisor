// issue #8: native services depending on sidecars (PG/Redis/Qdrant) need
// explicit connection env injection. This module maps service names to the
// required connection variables and resolves concrete values from
// .env / process env.
// fail-closed: a required value that cannot be resolved -> Err, refusing to
// start the service (no silent degradation).
// issue #78: injection is no longer gated on the compose flag — both call
// sites previously checked compose.enabled first, so by default (compose
// off) fusion-cowork silently ran SQLite and fusion-rag silently ran
// :memory:. Both planes (bare-metal compose off / containerized compose on)
// now inject; Qdrant exception: when local 6333 is unreachable, skip
// QDRANT_URL injection (rag has a :memory: fallback), not fail-closed.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

// Native services that depend on sidecars, plus their required connection
// env key lists. Services not in the table -> no injection, unchanged.
fn required_vars(svc: &str) -> Option<&'static [&'static str]> {
    match svc {
        "fusion-identity" => Some(&["DATABASE_URL"]),
        "fusion-rag" => Some(&["QDRANT_URL"]),
        "fusion-cowork" => Some(&["DATABASE_URL", "REDIS_URL"]),
        "fusion-model-hub" => Some(&["DATABASE_URL", "REDIS_URL"]),
        _ => None,
    }
}

// issue #29: registration env for shell-ecosystem module services.
// issue #45: business-plane services' FUSION_CLIENT_KEY (gateway client key,
// conventions #1) goes through this injection channel too — all three
// arg-less start.sh scripts hard-require this key, missing = fail-fast.
// These injections are not gated on the compose flag (unlike sidecar
// connection env).
// issue #85: FUSION_CLIENT_KEY default inverted — inject into every managed
// native service, except services explicitly excluded. The manual allowlist
// missed entries three times (fusion-rag 2026-10-31 night / ai-trysuite #84 /
// fusion-code-modenization #85); every new gateway-consuming service would
// silently degrade (falling back to the stale rotated alias FUSION_MLX_API_KEY
// in the daemon env snapshot -> 401) until someone noticed. Services that
// never read this env simply ignore it, injection has no side effects;
// fail-fast `:?` consumers are satisfied by the injection; the resolve chain
// is unchanged (process env -> env_file -> gateway config fallback, all
// missing -> Err refusing to start).
// Exclusion table (no FUSION_CLIENT_KEY injection):
// - fusion-gateway: upstream config.go deliberately does not BindEnv
//   FUSION_CLIENT_KEY — an ambient client key would overwrite the mlx backend
//   key via the api_key_env reference (fusion-gateway#412), injection is
//   out of bounds.
// - fusion-mlx: inference server, the single source of auth truth is
//   settings.json (same for strip_env_for); the client key must not enter
//   the server env.
// - pure static hosting (http.server web/dist, never hits the gateway).
// - fusion-health (issue #88): non-gateway consumer connecting directly to
//   mlx :11434; since v1.2.3 the env-first _mlx_key() adopts
//   FUSION_CLIENT_KEY on sight -> 401.
fn shell_vars(svc: &str) -> Vec<&'static str> {
    let mut vars: Vec<&'static str> = Vec::new();
    if !matches!(
        svc,
        "fusion-gateway"
            | "fusion-mlx"
            | "customer-insight"
            | "customer-insight-api"
            | "ai-apparelmockup-web"
            | "fusion-health"
    ) {
        vars.push("FUSION_CLIENT_KEY");
    }
    match svc {
        // issue #54: upstream migrated to the conventions §6 new names — cowork
        // shell_registry.py reads FUSION_SHELL_GATEWAY_URL + FUSION_ADMIN_KEY
        // (URL not yet migrated to BASE_URL, old name kept during migration);
        // plugins console v0.4.7 (fusion-plugins-ecosystem#39) reads
        // FUSION_GATEWAY_BASE_URL + FUSION_ADMIN_KEY, no longer the old name.
        // issue #105: added FUSION_COWORK_WORKSPACE_ROOT — web_api.py's
        // /api/workspaces/system-dirs uses it to bound the directory picker's
        // browsable root; the Path.home() default would expose the entire home
        // directory tree to end users.
        "fusion-cowork-web" => vars.extend([
            "FUSION_SHELL_GATEWAY_URL",
            "FUSION_ADMIN_KEY",
            "FUSION_COWORK_WORKSPACE_ROOT",
        ]),
        "fusion-plugins-ecosystem-console" => {
            vars.extend(["FUSION_GATEWAY_BASE_URL", "FUSION_ADMIN_KEY"])
        }
        "fusion-trainer-bridge" => vars.push("FUSION_TRAINER_BRIDGE_KEY"),
        _ => {}
    }
    vars
}

// Whether the service needs shell registration env (caller injects
// unconditionally, not gated on compose). After the issue #85 default
// inversion, every service outside the exclusion table needs it
// (FUSION_CLIENT_KEY).
pub fn needs_shell_env(svc: &str) -> bool {
    !shell_vars(svc).is_empty()
}

// issue #39/#40: env variable names to strip before spawning a child process.
// In fusion-mlx's start.sh the FUSION_MLX_API_KEY env outranks settings.json,
// so the daemon env snapshot (from the monorepo .env) pins the old key — every
// supervisor restart would have the new instance authenticate with the old
// key, 401 for all settings.json clients. After stripping, start.sh resolves
// from ~/.fusion-mlx/settings.json each time, single source of truth.
// compose check_env still requires the daemon itself to hold the var (gates
// compose containers, unaffected).
pub fn strip_env_for(svc: &str) -> &'static [&'static str] {
    match svc {
        "fusion-mlx" => &["FUSION_MLX_API_KEY"],
        // issue #45: fusion-health's config.py env outranks ~/.fusion-mlx/
        // settings.json — the old key in the daemon snapshot makes the backend
        // probe 401 (503 degraded). After stripping its config falls back to
        // resolving settings.json, single source of truth with the mlx instance.
        // issue #88: also strip FUSION_CLIENT_KEY — since v1.2.3 (upstream #28)
        // _mlx_key() adopts this env on sight, but fusion-health connects
        // directly to :11434 (not a gateway consumer); the client key is
        // invalid for mlx -> backend 401 -> /api/v1/health permanently 503
        // degraded. Also added to the shell_vars exclusion table so it is not
        // injected — belt and braces (strip covers manual starts inheriting .env).
        "fusion-health" => &["FUSION_MLX_API_KEY", "FUSION_CLIENT_KEY"],
        _ => &[],
    }
}

// Returns all connection variable names the service needs (for callers to
// decide whether injection is required).
pub fn needs_injection(svc: &str) -> bool {
    required_vars(svc).is_some()
}

// Resolve a single env value: process env first, then env_file, else None.
fn resolve(key: &str, env_file: &std::path::Path) -> Option<String> {
    if let Ok(v) = std::env::var(key)
        && !v.is_empty()
    {
        return Some(v);
    }
    read_env_file(env_file).get(key).cloned()
}

// Parse a .env file into a HashMap (key -> value), skipping comments/blank lines.
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

// Resolve the connection env to inject for a service (name -> value).
// fail-closed: any required value unresolvable -> Err (with service name and
// the missing key).
// Non-dependent services (required_vars=None) -> empty map (no injection, no
// error).
// issue #78: probe local 6333 reachability before injecting QDRANT_URL — when
// the qdrant container is down, skip the key (fusion-rag upstream has a
// :memory: fallback) instead of failing the whole injection.
// issue #78 addendum: when FUSION_PG_PASSWORD is missing, probe local 5432 —
// the native plane (compose_on=false) uses brew trust auth
// (fusion-db-conventions-1030 §1, there is no password at all); reachable ->
// inject a passwordless connection string, unreachable -> skip DATABASE_URL
// (the app falls back to SQLite).
// The compose plane (compose_on=true) sidecar PG uses password auth; a missing
// password = configuration error, stay fail-closed (no silent degradation to a
// trust string hitting the wrong database).
pub fn native_env_for(
    svc: &str,
    env_file: &std::path::Path,
    compose_on: bool,
) -> Result<HashMap<String, String>> {
    let qdrant_up = qdrant_reachable();
    let pg_up = pg_reachable();
    native_env_for_opts(svc, env_file, compose_on, pg_up, qdrant_up)
}

// issue #78: testable variant — pg_up/qdrant_up injected by tests (no real
// network probing).
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
    // Common fragments the PG connection string depends on
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
                    // issue #78 addendum: compose-plane sidecar PG uses password
                    // auth; a missing password = configuration error, stay
                    // fail-closed (second line of defense beyond check_env).
                    (None, _) if compose_on => {
                        bail!(
                            "FUSION_PG_PASSWORD missing — cannot build DATABASE_URL for {svc} (compose plane fail-closed)"
                        );
                    }
                    // issue #78 addendum: native-plane brew trust auth has no
                    // password — reachable PG -> inject the passwordless string
                    // (missing FUSION_PG_USER omits the user segment; libpq/
                    // asyncpg default to the OS user); unreachable -> skip and
                    // let the app fall back.
                    (None, _) if pg_up => {
                        tracing::info!(
                            svc,
                            "FUSION_PG_PASSWORD unset but 5432 reachable (trust auth), injecting passwordless DATABASE_URL"
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
                            "FUSION_PG_PASSWORD unset and 5432 unreachable, skipping DATABASE_URL injection"
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
                        "qdrant 127.0.0.1:6333 unreachable, skipping QDRANT_URL injection (rag falls back to :memory:)"
                    );
                    continue;
                }
                "http://localhost:6333".to_string()
            }
            other => bail!("env_map: unknown connection key {other} for {svc}"),
        };
        out.insert((*k).to_string(), val);
    }
    // Skipped keys (qdrant down / pg down) allow an empty map (the app has a
    // fallback); any other empty map = resolution anomaly, fail-closed.
    if out.is_empty() && !skipped {
        bail!("env_map: {svc} required connection variables resolved to empty (fail-closed)");
    }
    tracing::info!(svc, keys = ?keys, "native env injection resolved");
    Ok(out)
}

// issue #78: local qdrant (127.0.0.1:6333) reachability probe, 300ms timeout.
// A failed probe does not block injection (caller skips QDRANT_URL; rag falls
// back upstream).
fn qdrant_reachable() -> bool {
    tcp_reachable(6333, "qdrant")
}

// issue #78 addendum: local PG (127.0.0.1:5432) reachability probe (corroborates
// trust auth).
fn pg_reachable() -> bool {
    tcp_reachable(5432, "postgres")
}

fn tcp_reachable(port: u16, what: &str) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)) {
        Ok(_) => true,
        Err(e) => {
            tracing::info!(err = %e, what, port, "probe failed, the matching connection env will be skipped");
            false
        }
    }
}

// issue #29: resolve a service's shell registration env.
// Resolve order (mirrors trainer-bridge.sh reading gateway config itself):
//   process env -> env_file -> fallback
// - FUSION_SHELL_GATEWAY_URL / FUSION_GATEWAY_BASE_URL: fallback
//   http://127.0.0.1:11432 (local gateway fixed port; since issue #54 the old
//   vs new URL name is split per consumer)
// - FUSION_ADMIN_KEY: fallback reads fusion-gateway config.yaml
//   web_shell.admin_key (read-only, never rewrites upstream), path overridden
//   by FUSION_GATEWAY_CONFIG
// - FUSION_TRAINER_BRIDGE_KEY: no fallback, neither env nor env_file ->
//   fail-closed Err (the bridge requires this key to start; fail early, alert
//   early)
// Non-shell-module services -> empty map.
pub fn shell_env_for(svc: &str, env_file: &Path) -> Result<HashMap<String, String>> {
    // issue #101: fusion-health audit-key persistence, injected additively
    // before the shell_vars gate (fusion-health is excluded from FUSION_CLIENT_KEY
    // injection per #85/#88 but must still receive its audit key). Non-fatal.
    let mut out = HashMap::new();
    if svc == "fusion-health"
        && let Some(v) = health_audit_key()
    {
        out.insert("FUSION_HEALTH_AUDIT_HMAC_KEY".to_string(), v);
    }
    let keys = shell_vars(svc);
    if keys.is_empty() {
        return Ok(out);
    }
    for k in &keys {
        match *k {
            "FUSION_SHELL_GATEWAY_URL" => {
                let v = resolve(k, env_file).unwrap_or_else(|| "http://127.0.0.1:11432".into());
                out.insert(k.to_string(), v);
            }
            // issue #54: admin key published under the new name FUSION_ADMIN_KEY
            // (conventions §1); the fallback still reads gateway config
            // web_shell.admin_key read-only, never rewrites upstream.
            "FUSION_ADMIN_KEY" => {
                let v = match resolve(k, env_file) {
                    Some(v) => v,
                    None => read_gateway_admin_key().with_context(|| {
                        format!(
                            "FUSION_ADMIN_KEY missing — neither env nor .env has it and gateway config is unreadable, \
                                 cannot register shell for {svc} (fail-closed)"
                        )
                    })?,
                };
                out.insert(k.to_string(), v);
            }
            // issue #54: the new URL variable read by plugins console v0.4.7,
            // same fallback as the local gateway.
            "FUSION_GATEWAY_BASE_URL" => {
                let v = resolve(k, env_file).unwrap_or_else(|| "http://127.0.0.1:11432".into());
                out.insert(k.to_string(), v);
            }
            "FUSION_TRAINER_BRIDGE_KEY" => {
                let v = resolve(k, env_file).with_context(|| {
                    format!(
                        "FUSION_TRAINER_BRIDGE_KEY missing — bridge refuses to start (fail-closed, svc {svc})"
                    )
                })?;
                out.insert(k.to_string(), v);
            }
            // issue #45: business-plane gateway client key — resolve order same
            // as shell registration: process env -> env_file -> fallback reading
            // gateway config auth.api_keys (prefer the name=admin all-model key
            // whose allowed_models includes *; else the first entry). All three
            // services' start.sh hard-require it with `:?`; all missing ->
            // fail-closed Err (fail early, alert early).
            "FUSION_CLIENT_KEY" => {
                let v = match resolve(k, env_file) {
                    Some(v) => v,
                    None => read_gateway_client_key().with_context(|| {
                        format!(
                            "FUSION_CLIENT_KEY missing — neither env nor .env has it and gateway config is unreadable, \
                             cannot start {svc} (fail-closed)"
                        )
                    })?,
                };
                out.insert(k.to_string(), v);
            }
            // issue #105: the workspace root is optional hardening config, not a
            // startup dependency — when both env and env_file lack it, skip
            // injection (upstream web_api.py falls back to Path.home(), same as
            // pre-fix behavior), not fail-closed, honoring the boot contract
            // (absent optional config must not block startup).
            "FUSION_COWORK_WORKSPACE_ROOT" => {
                if let Some(v) = resolve(k, env_file) {
                    out.insert(k.to_string(), v);
                }
            }
            other => bail!("env_map: unknown shell registration key {other} for {svc}"),
        }
    }
    tracing::info!(svc, keys = ?keys, "shell registration env resolved");
    Ok(out)
}

// issue #101: fusion-health signs its tamper-evident audit log with
// FUSION_HEALTH_AUDIT_HMAC_KEY (fallback FUSION_HEALTH_API_KEY), resolved per
// request — a key change mid-log makes every pre-change line unverifiable
// (`tampered`). The key's persisted home is ~/.fusion-health/env (upstream
// v1.2.6; FUSION_HEALTH_ENV_FILE overrides), which upstream start.sh sources —
// but the supervisor must not depend on that: its daemon env snapshot may
// carry a stale-era key or none. Inject the persisted key into the child env
// (same value start.sh would set; file absent -> no injection, the empty-key
// fallback then stays self-consistent). Never log the value.
pub fn health_audit_key() -> Option<String> {
    let path = match std::env::var("FUSION_HEALTH_ENV_FILE") {
        Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".fusion-health/env")
        }
    };
    let text = std::fs::read_to_string(&path).ok()?;
    let key = parse_health_env_key(&text);
    if key.is_some() {
        tracing::info!(
            path = %path.display(),
            "fusion-health audit key injected from persisted env file"
        );
    } else {
        tracing::warn!(
            path = %path.display(),
            "fusion-health env file present but no FUSION_HEALTH_AUDIT_HMAC_KEY — audit key not injected"
        );
    }
    key
}

fn parse_health_env_key(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == "FUSION_HEALTH_AUDIT_HMAC_KEY"
            && !v.trim().is_empty()
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

// Read fusion-gateway config.yaml's web_shell.admin_key, read-only.
// Path: FUSION_GATEWAY_CONFIG overrides, default ~/fusion/fusion-gateway/config.yaml.
fn read_gateway_admin_key() -> Result<String> {
    let path = std::env::var("FUSION_GATEWAY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join("fusion/fusion-gateway/config.yaml")
        });
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read gateway config: {}", path.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse gateway config: {}", path.display()))?;
    let key = cfg
        .get("web_shell")
        .and_then(|w| w.get("admin_key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .with_context(|| {
            format!(
                "gateway config has no web_shell.admin_key: {}",
                path.display()
            )
        })?;
    Ok(key.to_string())
}

// issue #45: read fusion-gateway config.yaml's auth.api_keys, take the
// name=admin key (all-model wildcard, needed by business backends hitting the
// gateway's LLM channel); if no admin entry, take the first.
// Path: FUSION_GATEWAY_CONFIG overrides, default ~/fusion/fusion-gateway/config.yaml.
fn read_gateway_client_key() -> Result<String> {
    let path = std::env::var("FUSION_GATEWAY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join("fusion/fusion-gateway/config.yaml")
        });
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read gateway config: {}", path.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse gateway config: {}", path.display()))?;
    let keys = cfg
        .get("auth")
        .and_then(|a| a.get("api_keys"))
        .and_then(|k| k.as_sequence())
        .with_context(|| format!("gateway config has no auth.api_keys: {}", path.display()))?;
    let admin = keys
        .iter()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("admin"));
    let entry = admin
        .or_else(|| keys.first())
        .with_context(|| format!("gateway config auth.api_keys is empty: {}", path.display()))?;
    let key = entry
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .with_context(|| {
            format!(
                "gateway config api_keys entry has no key: {}",
                path.display()
            )
        })?;
    Ok(key.to_string())
}

// Derive the PG database name from the service name: modelhub/cowork use their
// own env, identity/rag use the service name.
// issue #78 addendum: the db-name fallback splits per plane — compose sidecar
// pg-init creates short prefix-less names (cowork/identity/rag, see
// docker-compose.sidecar.yml FUSION_PG_DATABASES), native brew PG creates
// underscored full names (fusion_cowork/fusion_identity, see
// fusion-db-conventions-1030). FUSION_*_DB env overrides take priority on
// both planes.
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

// issue #125: per-service env overlay (`~/.fusion-sv/env/<svc>.env`).
// Operators no longer need to detour through manual start.sh to change env
// vars (the daemon env snapshot is fixed and existing injections only cover
// known names). Every exec path (up / restart / rollout / crash auto-recovery)
// re-reads the file on the spot, layered after shell/sidecar injections (same
// key overrides the old value), so auto-recovery automatically carries the
// operator's variables too. Priority: process env -> per-service file ->
// shell/sidecar injection -> strip_env_for stripping (stripping is always
// last, the security guard wins).
// File missing/unreadable -> empty vector (optional enhancement, never blocks
// startup); malformed lines are WARNed and skipped; values are never logged.
pub fn overlay_env_for(svc: &str) -> Vec<(String, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home).join(".fusion-sv/env");
    overlay_env_for_in(&dir, svc)
}

pub fn overlay_env_for_in(dir: &std::path::Path, svc: &str) -> Vec<(String, String)> {
    let path = dir.join(format!("{svc}.env"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let out = parse_env_overlay(&text);
    tracing::info!(svc = %svc, path = %path.display(), entries = out.len(), "env overlay applied");
    out
}

// Pure parser: KEY=VALUE lines, ignoring blank lines/# comments/malformed
// lines missing '='. KEY whitespace is trimmed, VALUE kept verbatim (embedded
// '=' and quotes included, no shell expansion — determinism first).
pub fn parse_env_overlay(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            tracing::warn!(line = %line, "env overlay: malformed line skipped");
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            tracing::warn!(line = %line, "env overlay: empty key skipped");
            continue;
        }
        out.push((k.to_string(), v.to_string()));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::NamedTempFile;

    // issue #101: audit-key parser — key line wins, comments/blank/foreign
    // vars ignored, empty value treated as absent.
    #[test]
    fn test_parse_health_env_key() {
        assert_eq!(
            parse_health_env_key("FUSION_HEALTH_AUDIT_HMAC_KEY=abc123\n"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_health_env_key("# comment\nOTHER=1\nFUSION_HEALTH_AUDIT_HMAC_KEY=k2\n"),
            Some("k2".to_string())
        );
        assert_eq!(
            parse_health_env_key("FUSION_HEALTH_AUDIT_HMAC_KEY=\n"),
            None
        );
        assert_eq!(parse_health_env_key("FUSION_HEALTH_API_KEY=x\n"), None);
        assert_eq!(parse_health_env_key(""), None);
    }

    // Process env is globally shared; parallel tests pollute each other. This
    // lock forces env tests to run serially.
    // pub(crate): shared by the env tests in supervisor/config/backup.
    // Reentrant within a thread (thread_local depth counter + the outermost
    // holder takes the std Mutex): EnvGuard itself also acquires this lock
    // (#134 CI flake — bare set/remove racing lock-holding tests); nested sets
    // on the same thread do not self-deadlock, cross-thread stays mutually
    // exclusive.
    thread_local! {
        static ENV_LOCK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    pub(crate) struct EnvLockGuard {
        _inner: Option<std::sync::MutexGuard<'static, ()>>,
    }
    impl Drop for EnvLockGuard {
        fn drop(&mut self) {
            ENV_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
    pub(crate) fn env_lock() -> EnvLockGuard {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        let outermost = ENV_LOCK_DEPTH.with(|d| {
            let v = d.get();
            d.set(v + 1);
            v == 0
        });
        EnvLockGuard {
            _inner: outermost.then(|| M.get_or_init(|| Mutex::new(())).lock().unwrap()),
        }
    }

    fn env_file(content: &str) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), content).unwrap();
        f
    }

    // identity + .env with PG password -> DATABASE_URL resolved
    #[test]
    fn test_native_env_injected_for_identity() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=secret123\nFUSION_PG_USER=appuser\n");
        let env = native_env_for("fusion-identity", f.path(), false).unwrap();
        let url = env.get("DATABASE_URL").unwrap();
        assert!(
            url.contains("appuser:secret123@localhost:5432/fusion_identity"),
            "got {url}"
        );
    }

    // issue #78 addendum: FUSION_PG_PASSWORD missing + PG reachable (brew trust
    // auth) -> passwordless connection string
    #[test]
    fn test_native_env_trust_auth_when_pg_up() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_USER=fusion\n");
        let env = native_env_for_opts("fusion-identity", f.path(), false, true, true).unwrap();
        assert_eq!(
            env.get("DATABASE_URL").unwrap(),
            "postgresql://fusion@localhost:5432/fusion_identity"
        );
    }

    // issue #78 addendum: both password and FUSION_PG_USER missing + PG
    // reachable -> user segment omitted (libpq defaults to the OS user)
    #[test]
    fn test_native_env_trust_auth_no_user() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        unsafe { std::env::remove_var("FUSION_PG_USER") };
        let f = env_file("");
        let env = native_env_for_opts("fusion-identity", f.path(), false, true, true).unwrap();
        assert_eq!(
            env.get("DATABASE_URL").unwrap(),
            "postgresql://localhost:5432/fusion_identity"
        );
    }

    // issue #78 addendum: password missing + PG unreachable -> skip DATABASE_URL
    // (the app falls back to SQLite), not failing the whole injection;
    // REDIS_URL still injected unconditionally
    #[test]
    fn test_native_env_pg_down_skips_db_keeps_redis() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("");
        let env = native_env_for_opts("fusion-cowork", f.path(), false, false, true).unwrap();
        assert!(
            !env.contains_key("DATABASE_URL"),
            "unreachable PG with no password should not inject DATABASE_URL"
        );
        assert!(
            env.contains_key("REDIS_URL"),
            "REDIS_URL unaffected by PG state"
        );
    }

    // issue #78 addendum: compose plane with missing password -> still
    // fail-closed Err (sidecar PG uses password auth; silently degrading to a
    // trust string would hit the wrong database)
    #[test]
    fn test_compose_plane_missing_password_fail_closed() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_USER=fusion\n");
        let r = native_env_for_opts("fusion-identity", f.path(), true, true, true);
        assert!(
            r.is_err(),
            "compose plane with missing password should fail-closed"
        );
        assert!(
            r.unwrap_err().to_string().contains("FUSION_PG_PASSWORD"),
            "err should name the missing key"
        );
    }

    // Non-dependent service -> empty injection (unchanged)
    #[test]
    fn test_native_env_unknown_service_no_injection() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=x\n");
        let env = native_env_for("fusion-mlx", f.path(), false).unwrap();
        assert!(
            env.is_empty(),
            "non-dependent service should not get env injected"
        );
    }

    // Process env takes precedence over env_file
    #[test]
    fn test_process_env_precedence() {
        let _g = env_lock();
        // edition 2024: set_var/remove_var are unsafe (see CLAUDE.md EnvGuard)
        unsafe { std::env::set_var("FUSION_PG_PASSWORD", "fromproc") };
        // RAII: clear the process env even on panic, avoid polluting other tests
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
        assert!(url.contains("fromproc"), "process env should win: {url}");
    }

    // cowork needs both DATABASE_URL + REDIS_URL
    #[test]
    fn test_cowork_gets_db_and_redis() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let f = env_file("FUSION_PG_PASSWORD=pw\n");
        let env = native_env_for("fusion-cowork", f.path(), false).unwrap();
        assert!(env.contains_key("DATABASE_URL"));
        assert!(env.contains_key("REDIS_URL"));
        assert!(env["REDIS_URL"].contains("6379"));
    }

    // rag needs QDRANT_URL (issue #78: use the opts testable variant, no real
    // network probing)
    #[test]
    fn test_rag_gets_qdrant() {
        let f = env_file("");
        let env = native_env_for_opts("fusion-rag", f.path(), false, true, true).unwrap();
        assert_eq!(env.get("QDRANT_URL").unwrap(), "http://localhost:6333");
    }

    // issue #78: qdrant unreachable -> skip QDRANT_URL injection, rag allowed an
    // empty map (:memory: fallback)
    #[test]
    fn test_rag_qdrant_down_skips_key() {
        let f = env_file("");
        let env = native_env_for_opts("fusion-rag", f.path(), false, true, false).unwrap();
        assert!(
            !env.contains_key("QDRANT_URL"),
            "unreachable qdrant should not inject QDRANT_URL"
        );
        assert!(
            env.is_empty(),
            "rag allowed an empty map when its only key is skipped"
        );
    }

    // issue #78 addendum: db-name fallback splits per plane — compose=short name
    // (pg-init), native=underscored full name (brew)
    #[test]
    fn test_pg_db_name_per_plane() {
        let _g = env_lock();
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

    // read_env_file skips comments and blank lines, parses key=value
    #[test]
    fn test_read_env_file_skips_comments() {
        let f = env_file("# comment\n\nKEY=val\nOTHER=2\n");
        let map = read_env_file(f.path());
        assert_eq!(map.get("KEY").unwrap(), "val");
        assert_eq!(map.get("OTHER").unwrap(), "2");
        assert!(!map.contains_key("# comment"));
    }

    // ---- issue #29: shell registration env ----

    // Process env provides the admin key directly -> injection succeeds, gateway
    // config not read.
    // issue #54: injection name moved to FUSION_ADMIN_KEY; cowork still reads the
    // old URL name.
    #[test]
    fn test_shell_env_from_process_env() {
        let _g = env_lock();
        // issue #85: after the default inversion cowork-web also gets
        // FUSION_CLIENT_KEY injected; the test supplies the key explicitly
        let _ck = EnvGuard::set("FUSION_CLIENT_KEY", "ck-proc");
        let _c = EnvGuard::set("FUSION_ADMIN_KEY", "fromproc");
        let f = env_file("");
        let env = shell_env_for("fusion-cowork-web", f.path()).unwrap();
        assert_eq!(env.get("FUSION_ADMIN_KEY").unwrap(), "fromproc");
        assert_eq!(
            env.get("FUSION_SHELL_GATEWAY_URL").unwrap(),
            "http://127.0.0.1:11432",
            "unconfigured URL should fall back to the local gateway"
        );
    }

    // .env provides the admin key -> resolved from the file when the process env
    // lacks it; plugins console reads the new URL name FUSION_GATEWAY_BASE_URL
    // (issue #54)
    #[test]
    fn test_shell_env_from_env_file() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_ADMIN_KEY") };
        unsafe { std::env::remove_var("FUSION_GATEWAY_BASE_URL") };
        let f = env_file("FUSION_ADMIN_KEY=fromfile\nFUSION_CLIENT_KEY=ck-file\n");
        let env = shell_env_for("fusion-plugins-ecosystem-console", f.path()).unwrap();
        assert_eq!(env.get("FUSION_ADMIN_KEY").unwrap(), "fromfile");
        assert_eq!(
            env.get("FUSION_GATEWAY_BASE_URL").unwrap(),
            "http://127.0.0.1:11432",
            "console URL unconfigured should fall back to the local gateway"
        );
    }

    // issue #105: workspace root injected from env_file; when absent, neither
    // injected nor failed (optional hardening)
    #[test]
    fn test_shell_env_workspace_root_optional() {
        let _g = env_lock();
        let _ck = EnvGuard::set("FUSION_CLIENT_KEY", "ck-105");
        let _ak = EnvGuard::set("FUSION_ADMIN_KEY", "ak-105");
        let _wr = EnvGuard::set("FUSION_COWORK_WORKSPACE_ROOT", "");
        let f = env_file("FUSION_COWORK_WORKSPACE_ROOT=/srv/cowork-workspaces\n");
        let env = shell_env_for("fusion-cowork-web", f.path()).unwrap();
        assert_eq!(
            env.get("FUSION_COWORK_WORKSPACE_ROOT").unwrap(),
            "/srv/cowork-workspaces"
        );
        // Both process env and env_file lack it -> not injected, no error
        // (upstream falls back to Path.home())
        let f2 = env_file("");
        let env2 = shell_env_for("fusion-cowork-web", f2.path()).unwrap();
        assert!(!env2.contains_key("FUSION_COWORK_WORKSPACE_ROOT"));
    }

    // Admin key entirely missing + gateway config unreadable (pointed at a
    // nonexistent path) -> fail-closed Err
    #[test]
    fn test_shell_env_fail_closed_no_admin_key() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_ADMIN_KEY") };
        let _c = EnvGuard::set(
            "FUSION_GATEWAY_CONFIG",
            "/nonexistent/fusion-gateway-config-test.yaml",
        );
        let f = env_file("FUSION_CLIENT_KEY=ck-file\n");
        let r = shell_env_for("fusion-cowork-web", f.path());
        assert!(r.is_err(), "admin key entirely missing should fail-closed");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("FUSION_ADMIN_KEY"),
            "err should name the missing key: {msg}"
        );
    }

    // trainer-bridge key has no fallback: neither env nor .env -> fail-closed Err
    #[test]
    fn test_shell_env_bridge_key_fail_closed() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_TRAINER_BRIDGE_KEY") };
        // issue #85 inverts FUSION_CLIENT_KEY injection onto every managed
        // service, trainer-bridge included. Without an explicit value the
        // resolve chain falls through to the gateway config file, which does
        // not exist on CI runners — the resulting Err names FUSION_CLIENT_KEY,
        // not the key this test is about. Pin it so the only missing required
        // var is FUSION_TRAINER_BRIDGE_KEY.
        let _ck = EnvGuard::set("FUSION_CLIENT_KEY", "ck-bridge-fail-closed");
        let f = env_file("");
        let r = shell_env_for("fusion-trainer-bridge", f.path());
        assert!(r.is_err(), "missing bridge key should fail-closed");
        assert!(
            r.unwrap_err()
                .to_string()
                .contains("FUSION_TRAINER_BRIDGE_KEY")
        );
    }

    // trainer-bridge key resolved successfully from .env
    #[test]
    fn test_shell_env_bridge_key_from_file() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_TRAINER_BRIDGE_KEY") };
        // Same #85 reason as test_shell_env_bridge_key_fail_closed: without a
        // pinned FUSION_CLIENT_KEY the gateway-config fallback fails first on
        // CI (no config file) and shell_env_for errs before reaching the
        // trainer key.
        let _ck = EnvGuard::set("FUSION_CLIENT_KEY", "ck-bridge-from-file");
        let f = env_file("FUSION_TRAINER_BRIDGE_KEY=bridge-k\n");
        let env = shell_env_for("fusion-trainer-bridge", f.path()).unwrap();
        assert_eq!(env.get("FUSION_TRAINER_BRIDGE_KEY").unwrap(), "bridge-k");
        assert!(
            !env.contains_key("FUSION_SHELL_GATEWAY_URL"),
            "bridge needs only its own key"
        );
    }

    // Non-shell-module service -> empty map (unchanged)
    #[test]
    fn test_shell_env_unknown_service_empty() {
        let f = env_file("");
        let env = shell_env_for("fusion-mlx", f.path()).unwrap();
        assert!(env.is_empty());
        assert!(!needs_shell_env("fusion-mlx"));
        assert!(needs_shell_env("fusion-cowork-web"));
        assert!(needs_shell_env("fusion-trainer-bridge"));
    }

    // issue #39/#40: fusion-mlx child processes strip FUSION_MLX_API_KEY
    // (prevents old-key pinning); other services do not strip. issue #45:
    // fusion-health strips too (config.py env outranks settings.json; the old
    // key makes the backend probe 401). issue #88: fusion-health also strips
    // FUSION_CLIENT_KEY (v1.2.3 _mlx_key() is env-first; direct mlx connection
    // with a client key is a permanent 401).
    #[test]
    fn test_strip_env_for_mlx_only() {
        assert_eq!(strip_env_for("fusion-mlx"), &["FUSION_MLX_API_KEY"]);
        assert_eq!(
            strip_env_for("fusion-health"),
            &["FUSION_MLX_API_KEY", "FUSION_CLIENT_KEY"]
        );
        assert!(strip_env_for("fusion-gateway").is_empty());
        assert!(strip_env_for("fusion-rag").is_empty());
    }

    // issue #88: fusion-health is not a gateway consumer — no FUSION_CLIENT_KEY
    // injection (shell_vars exclusion), but still part of the shell ecosystem
    // (needs_shell_env goes through the match branch). Other services such as
    // trainer-bridge are unaffected.
    #[test]
    fn test_issue88_health_excluded_from_client_key() {
        assert!(!shell_vars("fusion-health").contains(&"FUSION_CLIENT_KEY"));
        assert!(shell_vars("fusion-rag").contains(&"FUSION_CLIENT_KEY"));
        assert!(shell_vars("ai-trysuite").contains(&"FUSION_CLIENT_KEY"));
    }

    // ---- issue #45: business-plane FUSION_CLIENT_KEY injection ----

    // Process env provides the key directly -> injection succeeds, gateway
    // config not read
    #[test]
    fn test_client_key_from_process_env() {
        let _g = env_lock();
        let _c = EnvGuard::set("FUSION_CLIENT_KEY", "fromproc");
        let f = env_file("");
        let env = shell_env_for("ai-fit", f.path()).unwrap();
        assert_eq!(env.get("FUSION_CLIENT_KEY").unwrap(), "fromproc");
        assert!(needs_shell_env("ai-fit"));
        assert!(needs_shell_env("ai-speech-exam"));
        assert!(needs_shell_env("ai-apparelmockup"));
        // issue #55: batch-registered FastAPI entries get injected too
        assert!(needs_shell_env("ai-ads-videobuilder"));
        assert!(needs_shell_env("ai-charactervoice"));
        // issue #85 default inversion: everything outside the exclusion table is
        // injected — except static hosting/gateway/mlx
        assert!(!needs_shell_env("customer-insight"));
        assert!(!needs_shell_env("customer-insight-api"));
        assert!(!needs_shell_env("ai-apparelmockup-web"));
        assert!(!needs_shell_env("fusion-gateway"));
        assert!(!needs_shell_env("fusion-mlx"));
        // Unknown/unregistered services are injected as well (ignored if unread)
        assert!(needs_shell_env("ai-family"));
        assert!(needs_shell_env("fusion-rag"));
        assert!(needs_shell_env("ai-trysuite"));
        // issue #85 directly triggered service: /health gateway check 401
        assert!(needs_shell_env("fusion-code-modenization"));
        assert!(needs_shell_env("fusion-unknown-new-service"));
    }

    // .env provides the key -> resolved from the file when the process env lacks it
    #[test]
    fn test_client_key_from_env_file() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_CLIENT_KEY") };
        let f = env_file("FUSION_CLIENT_KEY=fromfile\n");
        let env = shell_env_for("ai-speech-exam", f.path()).unwrap();
        assert_eq!(env.get("FUSION_CLIENT_KEY").unwrap(), "fromfile");
    }

    // Entirely missing + gateway config unreadable -> fail-closed Err naming the
    // missing key
    #[test]
    fn test_client_key_fail_closed_no_source() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_CLIENT_KEY") };
        let _c = EnvGuard::set(
            "FUSION_GATEWAY_CONFIG",
            "/nonexistent/fusion-gateway-config-test.yaml",
        );
        let f = env_file("");
        let r = shell_env_for("ai-apparelmockup", f.path());
        assert!(r.is_err(), "client key entirely missing should fail-closed");
        assert!(
            r.unwrap_err().to_string().contains("FUSION_CLIENT_KEY"),
            "err should name the missing key"
        );
    }

    // Fallback reads gateway config auth.api_keys — prefers the name=admin entry
    #[test]
    fn test_client_key_gateway_fallback_prefers_admin() {
        let _g = env_lock();
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

    // Env guard (RAII restore of process env), isomorphic to config.rs EnvGuard
    pub(crate) struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        // Holds the env test lock: set/remove/restore are mutually exclusive with
        // other env tests throughout (#134 CI flake).
        _lock: EnvLockGuard,
    }
    impl EnvGuard {
        pub(crate) fn set(key: &'static str, val: &str) -> Self {
            let lock = env_lock();
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
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

    // issue #125: overlay parsing — KEY=VALUE / comments / blank lines / malformed
    // lines skipped
    #[test]
    fn test_issue125_parse_env_overlay() {
        let out = parse_env_overlay(
            "# comment\n\nFUSION_LLM_TIMEOUT=120\nSPACED  KEY = keep-inner=eq\nbad line no eq\nX=\n",
        );
        assert_eq!(
            out,
            vec![
                ("FUSION_LLM_TIMEOUT".to_string(), "120".to_string()),
                ("SPACED  KEY".to_string(), " keep-inner=eq".to_string()),
                ("X".to_string(), String::new()),
            ]
        );
    }

    // issue #125: overlay file reading — existing file parsed, missing file empty
    // vector
    #[test]
    fn test_issue125_overlay_env_for_in() {
        let dir = std::env::temp_dir().join(format!("fusion-sv-ovl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let svc = "fusion-test-ovl";
        // Missing -> empty
        assert!(overlay_env_for_in(&dir, svc).is_empty());
        // Existing -> parsed
        std::fs::write(dir.join(format!("{svc}.env")), "A=1\n# c\nB=two\n").unwrap();
        assert_eq!(
            overlay_env_for_in(&dir, svc),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string())
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
