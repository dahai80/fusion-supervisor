// issue #29: start-method override table for shell-ecosystem module services.
// The default start path is bash <repo>/start.sh <cmd>; the services below start differently:
//   - fusion-trainer-bridge: ships its own scripts/trainer-bridge.sh (start|stop, nohup+pidfile)
//   - fusion-cowork-web: `fusion-cowork web serve --port 11461` (foreground uvicorn, needs daemonizing)
//   - fusion-plugins-ecosystem-console: `fusion-plugin-console --port 8795` (same as above)
// Upstream repos' start.sh belongs to each project; supervisor only wraps it, never rewrites (governance boundary).

use std::collections::HashMap;
use std::path::PathBuf;

// Start method
#[derive(Debug, Clone, PartialEq)]
pub enum StartSpec {
    // bash <script> <cmd> — a script with its own lifecycle (start/stop subcommands)
    Script(PathBuf),
    // nohup <bin> <args> >> log 2>&1 & echo $! > pidfile — daemonize a foreground process
    // (stop kills via pidfile; restart degrades to stop+start via restart_cmd_for)
    // issue #45: cwd is relative to FUSION_ROOT (None=inherit daemon cwd);
    // env holds app-internal fixed env (e.g. config paths), not secrets — secrets go through env_map injection.
    // issue #49: env_file is a dotenv file relative to FUSION_ROOT, sourced with set -a before start
    // (the service's own .env is the single source of truth, overriding stale values in the daemon process env snapshot);
    // a missing file fails closed and refuses to start (bash exit 1).
    Daemon {
        bin: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        env_file: Option<String>,
    },
    // issue #145: systemd-managed service (declared in the config systemd_units table, VPS deployment) —
    // lifecycle driven via systemctl start/stop; the unit itself is the process authority (start.sh
    // pidfiles do not work for systemd-spawned processes). restart = stop+start flow unchanged.
    Systemd {
        unit: String,
    },
}

// issue #145: systemd-managed table injected from config (OnceLock, same pattern as PORT_MAP —
// filled once at daemon start, read-only at runtime).
static SYSTEMD_UNITS: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();

// Registered at daemon start (Config::load validation already guarantees the unit-name character whitelist).
pub fn register_systemd_units(map: HashMap<String, String>) {
    let _ = SYSTEMD_UNITS.set(map);
}

fn systemd_unit_for(name: &str) -> Option<String> {
    SYSTEMD_UNITS.get()?.get(name).cloned()
}

// issue #145: systemctl command construction (pure function, testable). start/stop only — restart is
// carried by restart_one's stop+start flow, same semantics as the Daemon spec.
pub fn systemd_shell_cmd(unit: &str, verb: &str) -> anyhow::Result<String> {
    match verb {
        "start" | "stop" => Ok(format!("systemctl {verb} {unit}")),
        other => anyhow::bail!(
            "systemd spec does not support {other} (restart should be stop+start): {unit}"
        ),
    }
}

// Convenience constructor without cwd/env (all existing override services use this form)
fn daemon(bin: &str, args: &[&str]) -> StartSpec {
    StartSpec::Daemon {
        bin: bin.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: Vec::new(),
        env_file: None,
    }
}

// Script/binary resolution root for repo-relative paths: FUSION_ROOT first, else the current directory.
fn fusion_root() -> PathBuf {
    std::env::var("FUSION_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

// Service name → start method. Not in the table → None (default start.sh path).
pub fn start_spec_for(name: &str) -> Option<StartSpec> {
    // issue #145: config-declared systemd management wins first (on VPS deployments systemd is authoritative)
    if let Some(unit) = systemd_unit_for(name) {
        return Some(StartSpec::Systemd { unit });
    }
    match name {
        "fusion-trainer-bridge" => Some(StartSpec::Script(
            fusion_root().join("fusion-trainer/scripts/trainer-bridge.sh"),
        )),
        // issue #81: the fusion-office root start.sh only manages the 11485 shell (shell-serve.mjs);
        // the api (registry :11492) lifecycle script lives under the nested services/office-api/
        // (start|stop|restart|status|log, with its own pidfile + /healthz health wait).
        // Orchestrate the script as-is via Script, no upstream edits (only this project may be modified; upstream problems get issues).
        "fusion-office-api" => Some(StartSpec::Script(
            fusion_root().join("fusion-office/services/office-api/start.sh"),
        )),
        // issue #100: cowork-web needs FUSION_COWORK_WEBHOOK_SECRET from the root .env
        // (decision webhook signature verification; when unset every callback 503s). The daemon env
        // snapshot cannot get it (changing the plist requires a full daemon reload) — env_file is
        // sourced with set -a before start; a single-service restart takes effect. A missing key does
        // not fail (the endpoint 503s with a self-describing message).
        "fusion-cowork-web" => Some(StartSpec::Daemon {
            bin: "fusion-cowork".into(),
            args: vec![
                "web".into(),
                "serve".into(),
                "--port".into(),
                "11461".into(),
            ],
            cwd: None,
            env: Vec::new(),
            env_file: Some(".env".into()),
        }),
        "fusion-plugins-ecosystem-console" => {
            Some(daemon("fusion-plugin-console", &["--port", "8795"]))
        }
        // issue #31: MCP server main entry (registry :8765), foreground sse process daemonized
        "fusion-plugins-ecosystem" => Some(daemon(
            "fusion-plugin-server",
            &["--transport", "sse", "--port", "8765"],
        )),
        // issue #33: fusion-store repo root has no start.sh; deploy/start.sh has an upstream bug
        // (BINARY points to a nonexistent target/release/fusion-store; the actual artifact is fs-serve,
        //  upstream issue dahai80/fusion-store#5) — daemonize the fs-serve binary directly
        // (has default port/loopback listen, see probe_map for the probe)
        "fusion-store" => Some(daemon("fusion-store/target/release/fs-serve", &[])),
        // issue #33: fusion-smallbusiness has no start.sh; the CLI entry runs uvicorn directly
        // (port decided by FSB env, see probe_map for the probe)
        "fusion-smallbusiness" => Some(daemon("fusion-smallbusiness", &[])),
        // osagent issue#6: after serve-metrics moved 9100→11571, supervisor takes over —
        // fusion-osagent has no start.sh; serve-metrics is a foreground CLI process (proctitle
        // `fusion-osagent serve-metrics :11571`), daemonized takeover
        "fusion-osagent-metrics" => Some(daemon(
            "fusion-osagent",
            &["serve-metrics", "--port", "11571"],
        )),
        // issue #33: fusion-simulation has no start.sh; `fusion-sim service start` is a foreground
        // blocking process (gRPC :11447 + metrics :11456, --gui starts the Web GUI :11455);
        // the registry's -gui/-metrics entries are served embedded by this process, already supervise:false
        "fusion-simulation" => Some(daemon(
            "fusion-sim",
            &["service", "start", "--host", "127.0.0.1", "--gui"],
        )),
        // issue #34: fusion-cowork-ws = `fusion-cowork remote serve` foreground WebSocket
        // service (registry :11439); cowork start.sh only manages the main desk RPC, not this
        "fusion-cowork-ws" => Some(daemon("fusion-cowork", &["remote", "serve"])),
        // issue #34: fusion-bench start.sh references the removed fusion_bench.api.server
        // module (upstream moved to api.app, upstream issue dahai80/fusion-bench#27) —
        // daemonize `fusion-bench serve` directly (foreground uvicorn, default port 11467)
        "fusion-bench" => Some(daemon("fusion-bench", &["serve"])),
        // issue #34/#49: fusion-k12-teacher start.sh `start` subcommand foreground execs (upstream
        // dahai80/fusion-k12-teacher#31 fixed it but the script path still blocks in the foreground) — daemonize
        // `fusion-k12 serve` (default port 11448). issue #49: a bare serve does not load the project's own
        // .env (FUSION_MLX_API_KEY etc.), inheriting stale keys from the daemon env snapshot → gateway
        // 401 → /api/health 503 backend down → probe restart loop. env_file points at the project's
        // .env (single source of truth), sourced with set -a before start, overriding the stale values.
        "fusion-k12-teacher" => Some(StartSpec::Daemon {
            bin: "fusion-k12".into(),
            args: vec!["serve".into()],
            cwd: None,
            env: Vec::new(),
            env_file: Some("fusion-k12-teacher/.env".into()),
        }),
        // issue #34: fusion-code-modenization start.sh uses `python -m ...server.runner`
        // — runner has no __main__ entry, imports and exits 0 (the entry moved to server/__main__.py,
        // upstream issue dahai80/fusion-code-modelization#21) — daemonize the correct entry directly
        "fusion-code-modenization" => Some(daemon(
            ".venv/bin/python",
            &["-m", "fusion_code_modelization.server"],
        )),
        // issue #45: three business-platform arg-less start.sh scripts (exec foreground processes, they
        // ignore start/stop args) — `bash start.sh stop` would start the service in reverse; the script
        // path is unusable, same daemonize as bench/k12/modenization. FUSION_CLIENT_KEY (secret) is not
        // in this table — it goes through env_map injection (process env → .env → gateway config fallback).
        // issue #55: registry key + dir both renamed to ai-fit; start.sh foreground execs
        // uvicorn server.main:app (cwd=repo root, no --app-dir — the old fit-ai spec's
        // --app-dir server was wrong; previously masked by a leftover process holding the port).
        "ai-fit" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "server.main:app".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11510".into(),
            ],
            cwd: Some("business/ai-fit".into()),
            env: Vec::new(),
            env_file: None,
        }),
        "ai-speech-exam" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "app.main:app".into(),
                "--app-dir".into(),
                "backend".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11560".into(),
            ],
            cwd: Some("business/ai-speech-exam".into()),
            // start.sh resolves config with $PWD before cd backend → relative to cwd (=repo root) is exactly equivalent
            env: vec![(
                "FUSION_AI_SPEECH_EXAM_CONFIG".into(),
                "config/config.yaml".into(),
            )],
            env_file: None,
        }),
        // issue #55: upstream start.sh changed from a static http.server to a FastAPI backend
        // (uvicorn server.app:app, 11520); web static serving split off to ai-apparelmockup-web.
        "ai-apparelmockup" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "server.app:app".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11520".into(),
            ],
            cwd: Some("business/ai-apparelmockup".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // ---- issue #55: business entries registered in the 2026-10-27/28 port governance batch ----
        // Convention 1: standard start.sh all foreground exec → same daemonize; ports pinned one by one
        // (upstream config defaults are mostly pre-migration stale ports, TASK-PORT-PROC-2026-10-27 not landed).
        // FUSION_CLIENT_KEY always goes through env_map injection, never in this table.
        "ai-ads-videobuilder" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "ads_vb.cli".into(),
                "serve".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11531".into(),
            ],
            cwd: Some("business/ai-ads-videobuilder".into()),
            env: Vec::new(),
            env_file: None,
        }),
        "ai-doc-automation" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "backend.cli".into(),
                "serve".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11550".into(),
            ],
            cwd: Some("business/ai-doc-automation".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // backend.app takes no CLI args; the port reads FUSION_AI_INFORMATION_SEARCH_PORT
        "ai-information-search" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec!["-m".into(), "backend.app".into()],
            cwd: Some("business/ai-information-search".into()),
            env: vec![("FUSION_AI_INFORMATION_SEARCH_PORT".into(), "11595".into())],
            env_file: None,
        }),
        // fusion_ai_operabase.main port reads FUSION_AI_OPERABASE_PORT
        "ai-OperaBase" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec!["-m".into(), "fusion_ai_operabase.main".into()],
            cwd: Some("business/ai-OperaBase".into()),
            env: vec![("FUSION_AI_OPERABASE_PORT".into(), "11504".into())],
            env_file: None,
        }),
        // charactervoice has no start.sh; config default ports are still the pre-migration 11480/11481 → pin explicitly
        "ai-charactervoice" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "svc_api.app:app".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11505".into(),
            ],
            cwd: Some("business/ai-charactervoice".into()),
            env: Vec::new(),
            env_file: None,
        }),
        "ai-charactervoice-session" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "svc_session.app:app".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11506".into(),
            ],
            cwd: Some("business/ai-charactervoice".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // customer-insight dual entries share one repo: web static serving (11594) +
        // reserved API port (11501, hosts static until the API lands, then switch the entry point)
        "customer-insight" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "http.server".into(),
                "11594".into(),
                "--bind".into(),
                "127.0.0.1".into(),
                "--directory".into(),
                "web/dist".into(),
            ],
            cwd: Some("business/ai-customer-insight".into()),
            env: Vec::new(),
            env_file: None,
        }),
        "customer-insight-api" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "http.server".into(),
                "11501".into(),
                "--bind".into(),
                "127.0.0.1".into(),
                "--directory".into(),
                "web/dist".into(),
            ],
            cwd: Some("business/ai-customer-insight".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // web static serving split out of apparelmockup (no /health → TCP probe)
        "ai-apparelmockup-web" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "http.server".into(),
                "11593".into(),
                "--bind".into(),
                "127.0.0.1".into(),
                "--directory".into(),
                "web/dist".into(),
            ],
            cwd: Some("business/ai-apparelmockup".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // issue #88: two more entries found by the post-venv-rebuild fleet audit — start.sh foreground
        // execs and ignores subcommands (bash start.sh stop would START), same Daemon takeover.
        // Ports pinned to registry values (start.sh defaults are the same, guarding against upstream config drift)
        // issue #143: the ai-assistant-agent entry was removed — the service's own scheduler deliberately
        // self-heal restarts periodically (~5m10s graceful exit and PID change, upstream
        // dahai80-business/ai-assistant-agent#5); supervisor management would treat each as a crash,
        // auto-revive and create a Down/Back alert storm (proven in #142).
        // registry is marked supervise: false; its lifecycle returns to its own tooling.
        // issue #94: same as #88 — start.sh `start` case foreground execs uvicorn,
        // the default script path would hang exec; Daemon takeover, port pinned to registry 11516
        "ai-booking-agent" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "booking_agent.main:app".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11516".into(),
                "--app-dir".into(),
                "backend".into(),
            ],
            cwd: Some("business/ai-booking-agent".into()),
            env: Vec::new(),
            env_file: None,
        }),
        "ai-lead-qualifier" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec![
                "-m".into(),
                "ai_lead_qualifier.cli".into(),
                "serve".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "11552".into(),
            ],
            cwd: Some("business/ai-lead-qualifier".into()),
            env: Vec::new(),
            env_file: None,
        }),
        // issue #131: fusion-site repo has no start.sh — Next standalone foreground node
        // process (cwd .next/standalone, server.js reads PORT/HOSTNAME env). Previously only
        // manual nohup with injected env; any external restart/takeover lost the env (auth callback 401 observed).
        // Port pinned to registry :11481 (3000 is a reserved port, upstream fusion-site#54 migration).
        // NEXT_PUBLIC_SITE_URL varies by deployment (public origin / local port) and the key's semantics
        // belong to the deployment — routed through the ~/.fusion-sv/env/fusion-site.env overlay (#125), not pinned.
        "fusion-site" => Some(StartSpec::Daemon {
            bin: "node".into(),
            args: vec!["server.js".into()],
            cwd: Some("fusion-site/.next/standalone".into()),
            env: vec![
                ("PORT".into(), "11481".into()),
                ("HOSTNAME".into(), "127.0.0.1".into()),
                ("NODE_ENV".into(), "production".into()),
            ],
            env_file: None,
        }),
        // issue #140: fusion-ops Mac-side registration removed (service moved to VPS systemd, fusion-ops#5) —
        // no longer managed here, avoiding a permanent Stopped facade in the status table.
        _ => None,
    }
}

// CLI entries inside the shared venv preferred (the daemon usually does not run inside a venv shell);
// fall back to the bare name for PATH resolution when absent.
// A bin containing a path separator (e.g. fusion-store/target/release/fs-serve) resolves relative to FUSION_ROOT.
pub fn resolve_bin(bin: &str) -> String {
    if bin.contains('/') {
        let candidate = fusion_root().join(bin);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
        return bin.to_string();
    }
    let candidate = fusion_root().join(".venv/bin").join(bin);
    if candidate.exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        bin.to_string()
    }
}

// Supervisor-managed pidfile directory (~/.fusion-sv/pids/)
pub fn pid_path(svc: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".fusion-sv/pids")
        .join(format!("{svc}.pid"))
}

// Supervisor-managed service log (~/.fusion-sv/logs/<svc>.log)
pub fn log_path(svc: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".fusion-sv/logs")
        .join(format!("{svc}.log"))
}

// Idle-sleep prevention companion: after the launchd→supervisor migration, the original plist's
// caffeinate -i was lost in the move (2026-10-30). fusion-gupiao's 01:00 daily_suggest long scan
// gets interrupted by system idle sleep. After a successful start/restart, attach a system assertion
// with `caffeinate -i -w <service pid>` — when the service exits or is stopped, caffeinate exits
// automatically and releases the assertion; no stop-path cooperation needed.
// The upstream proper fix is a caffeinate option in gupiao start.sh (fusion-gupiao#6);
// this table is a supervisor-side transitional patch.
pub fn caffeinate_pidfile(svc: &str) -> Option<&'static str> {
    match svc {
        "fusion-gupiao" => Some("~/.fusion-gupiao/run.pid"),
        _ => None,
    }
}

// Daemonized start command (run via bash -c): nohup background + log redirect + pidfile.
// Note mkdir/cd/export are joined with `;` rather than `&&`: `A && B &` backgrounds the whole list as
// a subshell, so $! gets the subshell pid, not the real process pid — stop would kill the shell,
// leaving the service process orphaned and still holding the port. With `;` separators nohup
// backgrounds alone, and $! is the real pid after nohup exec.
// issue #45: cd is guarded with `|| exit 1` (refuse to start when cwd does not exist, never run bare in a wrong directory).
// issue #49: env_file is sourced with set -a after cd (dotenv overrides stale daemon env snapshot values);
// a missing file fails closed with `|| exit 1`.
pub fn daemon_start_cmd(spec: &StartSpec, svc: &str) -> Option<String> {
    let StartSpec::Daemon {
        bin,
        args,
        cwd,
        env,
        env_file,
    } = spec
    else {
        return None;
    };
    let bin = resolve_bin(bin);
    let log = log_path(svc);
    let pid = pid_path(svc);
    let mut pre = format!("mkdir -p '{}'; ", pid.parent().unwrap().display());
    if let Some(cwd) = cwd {
        let dir = fusion_root().join(cwd);
        pre.push_str(&format!("cd '{}' || exit 1; ", dir.display()));
    }
    if let Some(env_file) = env_file {
        let path = fusion_root().join(env_file);
        pre.push_str(&format!(
            "set -a; [ -f '{path}' ] || exit 1; source '{path}'; set +a; ",
            path = path.display(),
        ));
    }
    for (k, v) in env {
        pre.push_str(&format!("export {k}='{v}'; "));
    }
    // issue #110: set -m (job control) makes the backgrounded service process its own process group
    // (pgid == its own pid, proven on macOS bash) — only then can the stop side kill the whole tree by
    // process group, so uvicorn workers and other descendants are not left behind. nohup already blocks
    // HUP; set -m does not change that.
    Some(format!(
        "{pre}set -m; nohup '{bin}' {args} >> '{log}' 2>&1 & echo $! > '{pid}'",
        args = args.join(" "),
        log = log.display(),
        pid = pid.display(),
    ))
}

// Daemonized stop command: pidfile kill + grace wait + SIGKILL escalation + cleanup.
// Some foreground-converted services (e.g. fusion-k12-teacher) ignore SIGTERM (graceful shutdown hangs);
// TERM alone yields a fake stop — down returns ok / pidfile removed / process still alive — so a -9
// fallback is mandatory.
// Grace is 5s (25×0.2s); a normal process exiting <1s breaks out early; an endless loop equals a 5s
// block, acceptable serially per service.
// issue #96: port-anchored fallback — orphan listeners whose pidfile is lost (PID changed/missing) are
// killed by registered port via lsof -sTCP:LISTEN; otherwise restart/rollout new processes die on port
// collision and the probe hits the old listener → "ok" but the code was never replaced. Only Daemon spec
// services take this path (the supervisor owns that port outright); start.sh services are unaffected.
// issue #110 (t3code kill-safety): two killing disciplines —
// 1) the pidfile branch checks pgid first; when pgid == pid (guaranteed by start-side set -m) kill the
//    whole tree by process group with `kill -TERM -<pid>` (descendant workers are not left behind); when
//    not found/unequal (legacy processes), fall back to single-pid kill, matching the old behavior.
//    ESRCH is covered by the kill -0 polling.
// 2) the port fallback must verify ownership before killing: lsof reads the candidate pid's cwd + ps
//    reads cmdline, kill only when the service's repo directory name matches; unrelated listeners are
//    skipped with a trace, never blindly killed by port
//    (t3code lesson: argv/port pattern matching kills unrelated processes).
//    This repo has no pkill/grep pattern-kill paths — pgrep is used for read-only detection only (runner.rs).
pub fn daemon_stop_cmd(svc: &str, port: Option<u16>) -> String {
    let pid = pid_path(svc);
    let repo_dir = crate::manifest::name_to_dir(svc);
    let mut cmd = format!(
        "p=$(cat '{pid}' 2>/dev/null || true); \
         if [ -n \"$p\" ]; then \
           g=$(ps -o pgid= -p \"$p\" 2>/dev/null | tr -d ' ' || true); \
           if [ -n \"$g\" ] && [ \"$g\" = \"$p\" ]; then \
             kill -TERM -\"$p\" 2>/dev/null || true; \
             i=0; while kill -0 \"$p\" 2>/dev/null && [ $i -lt 25 ]; do sleep 0.2; i=$((i+1)); done; \
             kill -0 \"$p\" 2>/dev/null && kill -9 -\"$p\" 2>/dev/null || true; \
           else \
             kill \"$p\" 2>/dev/null || true; \
             i=0; while kill -0 \"$p\" 2>/dev/null && [ $i -lt 25 ]; do sleep 0.2; i=$((i+1)); done; \
             kill -0 \"$p\" 2>/dev/null && kill -9 \"$p\" 2>/dev/null || true; \
           fi; \
         fi; \
         rm -f '{pid}'",
        pid = pid.display()
    );
    if let Some(port) = port {
        cmd.push_str(&format!(
            "; \
             p2=$(lsof -ti tcp:{port} -sTCP:LISTEN 2>/dev/null || true); \
             if [ -n \"$p2\" ]; then \
               for q in $p2; do \
                 cw=$(lsof -a -p \"$q\" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p'); \
                 cl=$(ps -o command= -p \"$q\" 2>/dev/null || true); \
                 case \"$cw|$cl\" in \
                   *'{repo}'*) \
                     kill \"$q\" 2>/dev/null || true; \
                     i=0; while kill -0 \"$q\" 2>/dev/null && [ $i -lt 25 ]; do sleep 0.2; i=$((i+1)); done; \
                     kill -0 \"$q\" 2>/dev/null && kill -9 \"$q\" 2>/dev/null || true; \
                     ;; \
                   *) \
                     echo \"skip port {port} listener $q: not owned by {repo}\" >&2; \
                     ;; \
                 esac; \
               done; \
             fi",
            port = port,
            repo = repo_dir
        ));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    // caffeinate companion table: gupiao has it, other services do not
    #[test]
    fn test_caffeinate_pidfile_table() {
        assert_eq!(
            caffeinate_pidfile("fusion-gupiao"),
            Some("~/.fusion-gupiao/run.pid")
        );
        assert_eq!(caffeinate_pidfile("fusion-mlx"), None);
        assert_eq!(caffeinate_pidfile("unknown-svc"), None);
    }

    // issue #145: config systemd-managed table — after registration start_spec_for returns Systemd,
    // command construction is start/stop only, restart semantics left to the stop+start flow
    #[test]
    fn test_issue145_systemd_spec_and_cmd() {
        assert!(start_spec_for("svc-not-registered").is_none());
        let mut m = std::collections::HashMap::new();
        m.insert(
            "fusion-identity".to_string(),
            "fusion-identity.service".to_string(),
        );
        register_systemd_units(m);
        match start_spec_for("fusion-identity") {
            Some(StartSpec::Systemd { unit }) => {
                assert_eq!(unit, "fusion-identity.service");
            }
            other => panic!("expected a Systemd spec: {other:?}"),
        }
        assert_eq!(
            systemd_shell_cmd("fusion-identity.service", "start").unwrap(),
            "systemctl start fusion-identity.service"
        );
        assert_eq!(
            systemd_shell_cmd("fusion-identity.service", "stop").unwrap(),
            "systemctl stop fusion-identity.service"
        );
        assert!(systemd_shell_cmd("u", "restart").is_err());
    }

    #[test]
    fn test_trainer_bridge_uses_script() {
        let spec = start_spec_for("fusion-trainer-bridge").unwrap();
        assert!(matches!(spec, StartSpec::Script(_)));
    }

    // issue #81: office-api uses the nested services/office-api/start.sh (not the repo-root script)
    #[test]
    fn test_office_api_uses_script() {
        let spec = start_spec_for("fusion-office-api").unwrap();
        match spec {
            StartSpec::Script(p) => {
                assert!(
                    p.ends_with("fusion-office/services/office-api/start.sh"),
                    "office-api script path wrong: {:?}",
                    p
                );
            }
            _ => panic!("office-api should be a Script spec"),
        }
    }

    // issue #34: cowork-ws daemonizes the remote serve foreground process
    #[test]
    fn test_cowork_ws_daemon_spec() {
        let spec = start_spec_for("fusion-cowork-ws").unwrap();
        match spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-cowork");
                assert_eq!(args, vec!["remote".to_string(), "serve".to_string()]);
            }
            _ => panic!("cowork-ws should be a Daemon spec"),
        }
    }

    // issue #34: bench/k12/modenization start.sh upstream-broken, Daemon specs run the correct entries directly
    #[test]
    fn test_issue34_broken_start_sh_daemon_specs() {
        let spec = start_spec_for("fusion-bench").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-bench");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-bench should be a Daemon spec"),
        }

        let spec = start_spec_for("fusion-k12-teacher").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-k12");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-k12-teacher should be a Daemon spec"),
        }

        // issue #100: cowork-web must carry env_file pointing at root .env so a
        // plain `restart fusion-cowork-web` delivers FUSION_COWORK_WEBHOOK_SECRET
        let spec = start_spec_for("fusion-cowork-web").unwrap();
        match &spec {
            StartSpec::Daemon {
                bin,
                args,
                env_file,
                ..
            } => {
                assert_eq!(bin, "fusion-cowork");
                assert_eq!(
                    args,
                    &vec![
                        "web".to_string(),
                        "serve".to_string(),
                        "--port".to_string(),
                        "11461".to_string()
                    ]
                );
                assert_eq!(
                    env_file.as_deref(),
                    Some(".env"),
                    "issue #100: env_file must point at root .env"
                );
            }
            _ => panic!("fusion-cowork-web should be a Daemon spec"),
        }

        let spec = start_spec_for("fusion-code-modenization").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, ".venv/bin/python");
                assert_eq!(
                    args,
                    &vec![
                        "-m".to_string(),
                        "fusion_code_modelization.server".to_string()
                    ]
                );
            }
            _ => panic!("fusion-code-modenization should be a Daemon spec"),
        }
        // resolve_bin path form: .venv/bin/python must resolve under FUSION_ROOT
        let r = resolve_bin(".venv/bin/python");
        assert!(
            r.contains(".venv/bin/python"),
            "unexpected resolution result: {r}"
        );
    }

    #[test]
    fn test_cowork_web_daemon_spec() {
        let spec = start_spec_for("fusion-cowork-web").unwrap();
        match spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-cowork");
                assert!(args.contains(&"11461".to_string()));
            }
            _ => panic!("cowork-web should be a Daemon spec"),
        }
    }

    #[test]
    fn test_unknown_service_none() {
        assert!(start_spec_for("fusion-mlx").is_none());
        assert!(start_spec_for("fusion-fake").is_none());
    }

    #[test]
    fn test_daemon_start_cmd_shape() {
        let Some(spec) = start_spec_for("fusion-cowork-web") else {
            panic!("cowork-web should have a spec");
        };
        let cmd = daemon_start_cmd(&spec, "fusion-cowork-web").unwrap();
        assert!(
            cmd.contains("nohup"),
            "should run in background via nohup: {cmd}"
        );
        assert!(
            cmd.contains("web serve --port 11461"),
            "should carry full args: {cmd}"
        );
        assert!(
            cmd.contains(".fusion-sv/pids/fusion-cowork-web.pid"),
            "should write the pidfile: {cmd}"
        );
        assert!(
            cmd.contains("echo $!"),
            "should record the background pid: {cmd}"
        );
    }

    #[test]
    fn test_daemon_start_cmd_none_for_script_spec() {
        let spec = start_spec_for("fusion-trainer-bridge").unwrap();
        assert!(daemon_start_cmd(&spec, "fusion-trainer-bridge").is_none());
    }

    // issue #31: the pidfile must record the real process pid — mkdir must not be joined with nohup via &&
    // (`&& ... &` backgrounds the whole list, $! = subshell pid; stop kills the shell and leaves an orphan)
    #[test]
    fn test_daemon_start_cmd_no_subshell_pid() {
        let Some(spec) = start_spec_for("fusion-cowork-web") else {
            panic!("cowork-web should have a spec");
        };
        let cmd = daemon_start_cmd(&spec, "fusion-cowork-web").unwrap();
        assert!(
            !cmd.contains("&& nohup"),
            "mkdir and nohup must not be joined with && (the pidfile would record a subshell pid): {cmd}"
        );
        assert!(
            cmd.contains("; nohup"),
            "mkdir should be joined with ; and nohup backgrounded alone: {cmd}"
        );
    }

    // issue #31: plugins-ecosystem MCP server (:8765) Daemon spec
    #[test]
    fn test_plugins_ecosystem_mcp_daemon_spec() {
        let spec = start_spec_for("fusion-plugins-ecosystem").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-plugin-server");
                assert!(args.contains(&"--transport".to_string()));
                assert!(args.contains(&"sse".to_string()));
                assert!(args.contains(&"8765".to_string()));
            }
            _ => panic!("plugins-ecosystem should be a Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-plugins-ecosystem").unwrap();
        assert!(
            cmd.contains("--transport sse --port 8765"),
            "should carry MCP server args: {cmd}"
        );
    }

    // issue #33: fusion-store daemonizes the fs-serve binary directly (deploy/start.sh upstream
    // BINARY name mismatch, script path unusable); a bin with a path resolves relative to FUSION_ROOT
    #[test]
    fn test_fusion_store_daemon_spec() {
        let spec = start_spec_for("fusion-store").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-store/target/release/fs-serve");
                assert!(
                    args.is_empty(),
                    "zero-arg start, port uses the fs-serve built-in default"
                );
            }
            _ => panic!("fusion-store should be a Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-store").unwrap();
        assert!(
            cmd.contains("fs-serve"),
            "should start the fs-serve binary: {cmd}"
        );
    }

    // A bin with a path separator resolves relative to FUSION_ROOT; a bare name goes through .venv/bin
    #[test]
    fn test_resolve_bin_path_form() {
        // must never be joined into .venv/bin whether or not it exists (joining a path with / is semantically wrong)
        let r = resolve_bin("fusion-store/target/release/fs-serve");
        assert!(
            !r.contains(".venv"),
            "path-form bin must not resolve through .venv: {r}"
        );
        assert!(r.contains("fs-serve"));
    }

    // issue #33: fusion-smallbusiness CLI runs uvicorn directly (:11490)
    #[test]
    fn test_fusion_smallbusiness_daemon_spec() {
        let spec = start_spec_for("fusion-smallbusiness").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-smallbusiness");
                assert!(
                    args.is_empty(),
                    "zero-arg start, port uses the FSB env default"
                );
            }
            _ => panic!("fusion-smallbusiness should be a Daemon spec"),
        }
    }

    // issue #33: fusion-sim service start blocks in the foreground; daemonize + --gui
    // (gui :11455 / metrics :11456 embedded in the same process; registry already supervise:false)
    #[test]
    fn test_fusion_simulation_daemon_spec() {
        let spec = start_spec_for("fusion-simulation").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-sim");
                assert!(args.contains(&"service".to_string()));
                assert!(args.contains(&"start".to_string()));
                assert!(args.contains(&"--gui".to_string()));
            }
            _ => panic!("fusion-simulation should be a Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-simulation").unwrap();
        assert!(
            cmd.contains("service start"),
            "should carry service start: {cmd}"
        );
    }

    // issue #45: business platform's three arg-less start.sh → daemonize + cwd + env
    #[test]
    fn test_issue42_business_daemon_specs() {
        let spec = start_spec_for("ai-fit").unwrap();
        match &spec {
            StartSpec::Daemon {
                bin,
                args,
                cwd,
                env,
                ..
            } => {
                assert_eq!(bin, ".venv/bin/python");
                assert!(args.contains(&"server.main:app".to_string()));
                assert!(
                    !args.contains(&"--app-dir".to_string()),
                    "uvicorn should import from the repo root"
                );
                assert_eq!(cwd.as_deref(), Some("business/ai-fit"));
                assert!(
                    env.is_empty(),
                    "ai-fit has no fixed env (key goes through env_map)"
                );
            }
            _ => panic!("ai-fit should be a Daemon spec"),
        }

        let spec = start_spec_for("ai-speech-exam").unwrap();
        match &spec {
            StartSpec::Daemon { args, cwd, env, .. } => {
                assert!(args.contains(&"app.main:app".to_string()));
                assert_eq!(cwd.as_deref(), Some("business/ai-speech-exam"));
                assert!(
                    env.iter()
                        .any(|(k, v)| k == "FUSION_AI_SPEECH_EXAM_CONFIG"
                            && v == "config/config.yaml"),
                    "should carry the config relative-path env: {env:?}"
                );
            }
            _ => panic!("ai-speech-exam should be a Daemon spec"),
        }

        // issue #55: apparelmockup upstream switched to FastAPI — no longer http.server
        let spec = start_spec_for("ai-apparelmockup").unwrap();
        match &spec {
            StartSpec::Daemon { args, cwd, .. } => {
                assert!(args.contains(&"server.app:app".to_string()));
                assert!(args.contains(&"11520".to_string()));
                assert!(!args.contains(&"http.server".to_string()));
                assert_eq!(cwd.as_deref(), Some("business/ai-apparelmockup"));
            }
            _ => panic!("ai-apparelmockup should be a Daemon spec"),
        }
    }

    // issue #55: batch-registered entries — ports pinned (upstream config default ports not migrated),
    // entries without CLI port args use env, shared-repo dual entries share cwd
    #[test]
    fn test_issue55_business_batch_specs() {
        for (name, module, port) in [
            ("ai-charactervoice", "svc_api.app:app", "11505"),
            ("ai-charactervoice-session", "svc_session.app:app", "11506"),
        ] {
            let spec = start_spec_for(name).unwrap();
            match &spec {
                StartSpec::Daemon { args, cwd, .. } => {
                    assert!(args.contains(&module.to_string()), "{name}: {args:?}");
                    assert!(
                        args.contains(&"--port".to_string()) && args.contains(&port.to_string()),
                        "{name} port must be pinned to {port}: {args:?}"
                    );
                    assert_eq!(cwd.as_deref(), Some("business/ai-charactervoice"));
                }
                _ => panic!("{name} should be a Daemon spec"),
            }
        }

        let spec = start_spec_for("ai-information-search").unwrap();
        match &spec {
            StartSpec::Daemon { env, cwd, .. } => {
                assert!(
                    env.iter()
                        .any(|(k, v)| k == "FUSION_AI_INFORMATION_SEARCH_PORT" && v == "11595")
                );
                assert_eq!(cwd.as_deref(), Some("business/ai-information-search"));
            }
            _ => panic!("ai-information-search should be a Daemon spec"),
        }

        let spec = start_spec_for("ai-OperaBase").unwrap();
        match &spec {
            StartSpec::Daemon { env, cwd, .. } => {
                assert!(
                    env.iter()
                        .any(|(k, v)| k == "FUSION_AI_OPERABASE_PORT" && v == "11504")
                );
                assert_eq!(cwd.as_deref(), Some("business/ai-OperaBase"));
            }
            _ => panic!("ai-operabase should be a Daemon spec"),
        }

        for (name, port) in [
            ("customer-insight", "11594"),
            ("customer-insight-api", "11501"),
            ("ai-apparelmockup-web", "11593"),
        ] {
            let spec = start_spec_for(name).unwrap();
            match &spec {
                StartSpec::Daemon { args, cwd, .. } => {
                    assert!(args.contains(&"http.server".to_string()));
                    assert!(args.contains(&port.to_string()), "{name}: {args:?}");
                    assert!(args.contains(&"web/dist".to_string()));
                    assert!(cwd.is_some());
                }
                _ => panic!("{name} should be a Daemon spec"),
            }
        }

        for name in ["ai-ads-videobuilder", "ai-doc-automation"] {
            let spec = start_spec_for(name).unwrap();
            match &spec {
                StartSpec::Daemon { args, .. } => {
                    assert!(args.contains(&"serve".to_string()), "{name}: {args:?}");
                    assert!(args.contains(&"--port".to_string()));
                }
                _ => panic!("{name} should be a Daemon spec"),
            }
        }

        // issue #88: two more business entries absorbed by the fleet audit (start.sh foreground exec
        // and ignores subcommands), ports pinned to registry values
        // issue #143: ai-assistant-agent removed (self-healing service no longer managed) — assert deregistration
        assert!(start_spec_for("ai-assistant-agent").is_none());
        let spec = start_spec_for("ai-lead-qualifier").unwrap();
        match &spec {
            StartSpec::Daemon { args, cwd, .. } => {
                assert_eq!(
                    args,
                    &[
                        "-m".to_string(),
                        "ai_lead_qualifier.cli".to_string(),
                        "serve".to_string(),
                        "--host".to_string(),
                        "127.0.0.1".to_string(),
                        "--port".to_string(),
                        "11552".to_string(),
                    ]
                );
                assert_eq!(cwd.as_deref(), Some("business/ai-lead-qualifier"));
            }
            _ => panic!("ai-lead-qualifier should be a Daemon spec"),
        }

        // osagent issue#6: serve-metrics Daemon spec (takeover after the 9100→11571 migration)
        let spec = start_spec_for("fusion-osagent-metrics").unwrap();
        match &spec {
            StartSpec::Daemon {
                bin,
                args,
                cwd,
                env,
                env_file,
            } => {
                assert_eq!(bin, "fusion-osagent");
                assert_eq!(args, &["serve-metrics", "--port", "11571"]);
                assert!(cwd.is_none() && env.is_empty() && env_file.is_none());
            }
            _ => panic!("fusion-osagent-metrics should be a Daemon spec"),
        }
    }

    // issue #45: cwd/env enter the start command — cd guarded with || exit 1, export prefixed,
    // without breaking the `; nohup` pid discipline (the pidfile records the real process pid)
    #[test]
    fn test_issue42_daemon_cmd_cwd_env_shape() {
        let spec = start_spec_for("ai-speech-exam").unwrap();
        let cmd = daemon_start_cmd(&spec, "ai-speech-exam").unwrap();
        assert!(
            cmd.contains("business/ai-speech-exam"),
            "should cd into the repo: {cmd}"
        );
        assert!(
            cmd.contains("|| exit 1;"),
            "cd failure should refuse to start: {cmd}"
        );
        assert!(
            cmd.contains("export FUSION_AI_SPEECH_EXAM_CONFIG='config/config.yaml';"),
            "should export the config path: {cmd}"
        );
        assert!(
            !cmd.contains("&& nohup"),
            "must not break the pid discipline: {cmd}"
        );
        assert!(cmd.contains("; nohup"), "nohup must be ; separated: {cmd}");
    }

    // issue #131: fusion-site Next standalone — node server.js @ .next/standalone,
    // port/host/NODE_ENV pinned to registry values, NEXT_PUBLIC_SITE_URL via overlay not in this table
    #[test]
    fn test_issue131_fusion_site_daemon_spec() {
        let spec = start_spec_for("fusion-site").expect("fusion-site spec missing");
        let StartSpec::Daemon {
            bin,
            args,
            cwd,
            env,
            env_file,
        } = &spec
        else {
            panic!("expected a Daemon spec");
        };
        assert_eq!(bin, "node");
        assert_eq!(args, &vec!["server.js".to_string()]);
        assert_eq!(cwd.as_deref(), Some("fusion-site/.next/standalone"));
        assert!(env.contains(&("PORT".to_string(), "11481".to_string())));
        assert!(env.contains(&("HOSTNAME".to_string(), "127.0.0.1".to_string())));
        assert!(env.contains(&("NODE_ENV".to_string(), "production".to_string())));
        assert_eq!(*env_file, None);
        let cmd = daemon_start_cmd(&spec, "fusion-site").unwrap();
        assert!(
            cmd.contains("fusion-site/.next/standalone"),
            "should cd into standalone: {cmd}"
        );
        assert!(
            cmd.contains("|| exit 1;"),
            "cd failure should refuse to start: {cmd}"
        );
        assert!(
            cmd.contains("export PORT='11481';"),
            "should export the port: {cmd}"
        );
        assert!(cmd.contains("; nohup"), "nohup must be ; separated: {cmd}");
        assert_eq!(
            crate::probe_map::restart_cmd_for("fusion-site"),
            "stop+start"
        );
    }

    #[test]
    fn test_daemon_stop_cmd_kills_pidfile() {
        let cmd = daemon_stop_cmd("fusion-cowork-web", Some(11461));
        assert!(cmd.contains("kill"), "stop should kill: {cmd}");
        assert!(
            cmd.contains("fusion-cowork-web.pid"),
            "stop should read the pidfile: {cmd}"
        );
        assert!(
            cmd.contains("kill -9"),
            "stop should escalate to SIGKILL: {cmd}"
        );
        assert!(
            cmd.contains("rm -f"),
            "stop should clean the pidfile: {cmd}"
        );
    }

    // Services that ignore SIGTERM must be killed with escalation (production bug: k12 still alive after down)
    #[test]
    fn test_daemon_stop_cmd_sigterm_escalation() {
        let cmd = daemon_stop_cmd("fusion-k12-teacher", None);
        assert!(
            cmd.contains("kill -0 \"$p\""),
            "should poll process liveness: {cmd}"
        );
        assert!(
            cmd.contains("kill -9 \"$p\""),
            "still alive after TERM should -9: {cmd}"
        );
        assert!(
            cmd.contains("rm -f"),
            "clean the pidfile after killing: {cmd}"
        );
    }

    // issue #96: port-anchored orphan-listener fallback — with a port, append the lsof LISTEN kill segment
    // issue #110: ownership must be verified before the kill (cwd/cmdline matches the repo directory name), no more blind kills
    #[test]
    fn test_daemon_stop_cmd_port_fallback() {
        let cmd = daemon_stop_cmd("fusion-cowork-web", Some(11461));
        assert!(
            cmd.contains("lsof -ti tcp:11461 -sTCP:LISTEN"),
            "should have the port-anchored kill segment: {cmd}"
        );
        assert!(
            cmd.contains("*'fusion-cowork'*"),
            "ownership check should match the repo directory name: {cmd}"
        );
        assert!(
            cmd.contains("not owned by fusion-cowork"),
            "unrelated listeners should be skipped with a trace: {cmd}"
        );
        assert!(
            cmd.contains("kill -9 \"$q\""),
            "a matched owner still gets the -9 fallback: {cmd}"
        );
        let no_port = daemon_stop_cmd("fusion-cowork-web", None);
        assert!(
            !no_port.contains("lsof"),
            "without a port the lsof segment must be absent: {no_port}"
        );
    }

    // issue #110: the start command carries set -m — service process pgid == its own pid so the stop side can group-kill
    #[test]
    fn test_daemon_start_cmd_sets_process_group() {
        let spec = start_spec_for("fusion-cowork-web").unwrap();
        let cmd = daemon_start_cmd(&spec, "fusion-cowork-web").unwrap();
        assert!(
            cmd.contains("set -m; nohup"),
            "the start command should carry set -m so the background process forms its own group: {cmd}"
        );
    }

    // issue #110: the pidfile branch checks pgid first, group-kills the whole tree when pgid==pid (TERM → poll → -9);
    // when pgid differs (legacy processes) falls back to single-pid kill
    #[test]
    fn test_daemon_stop_cmd_group_kill() {
        let cmd = daemon_stop_cmd("fusion-cowork-web", None);
        assert!(
            cmd.contains("g=$(ps -o pgid= -p \"$p\""),
            "should check the process group first: {cmd}"
        );
        assert!(
            cmd.contains("kill -TERM -\"$p\""),
            "should group-kill TERM when pgid==pid: {cmd}"
        );
        assert!(
            cmd.contains("kill -9 -\"$p\""),
            "group kill should also escalate to -9: {cmd}"
        );
        assert!(
            cmd.contains("kill \"$p\" 2>/dev/null || true"),
            "fall back to single-pid kill when pgid differs: {cmd}"
        );
    }

    // issue #49: k12 must source the project's own .env before daemonizing (single source of truth,
    // overriding the daemon env snapshot's stale keys); a missing file fails closed; other Daemon
    // services do not carry this segment
    #[test]
    fn test_issue49_k12_env_file_source_segment() {
        // fusion_root() reads the FUSION_ROOT env — mutually exclusive with the config/runner tests'
        // EnvGuard (#134 CI flake: seg paths drift when FUSION_ROOT is concurrently rewritten).
        let _env = crate::env_map::tests::env_lock();
        let spec = start_spec_for("fusion-k12-teacher").unwrap();
        let StartSpec::Daemon {
            env_file: Some(env_file),
            ..
        } = &spec
        else {
            panic!("fusion-k12-teacher should be a Daemon spec with env_file");
        };
        assert_eq!(env_file, "fusion-k12-teacher/.env");

        let cmd = daemon_start_cmd(&spec, "fusion-k12-teacher").unwrap();
        let seg = format!(
            "set -a; [ -f '{p}' ] || exit 1; source '{p}'; set +a; ",
            p = fusion_root().join(env_file).display(),
        );
        assert!(
            cmd.contains(&seg),
            "should carry the fail-closed source segment: {cmd}"
        );
        // the source segment must come before nohup (env injection precedes process start)
        assert!(
            cmd.find(&seg).unwrap() < cmd.find("nohup").unwrap(),
            "the source segment should come before nohup: {cmd}"
        );
        assert!(
            !cmd.contains("&& nohup"),
            "must not break the pid discipline: {cmd}"
        );

        // Daemon services without env_file must not contain the source segment
        let plain =
            daemon_start_cmd(&start_spec_for("fusion-bench").unwrap(), "fusion-bench").unwrap();
        assert!(
            !plain.contains("set -a;"),
            "services without env_file should not source: {plain}"
        );
    }
}
