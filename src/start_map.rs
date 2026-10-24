// issue #29: shell-ecosystem 模块服务的启动方式覆盖表。
// 默认启动路径是 bash <repo>/start.sh <cmd>; 下列服务的启动命令不同:
//   - fusion-trainer-bridge: 自带 scripts/trainer-bridge.sh (start|stop, nohup+pidfile)
//   - fusion-cowork-web: `fusion-cowork web serve --port 11461` (前台 uvicorn, 需 daemonize)
//   - fusion-plugins-ecosystem-console: `fusion-plugin-console --port 8795` (同上)
// 上游 repo 的 start.sh 归属各自项目, supervisor 只封装不改写 (治理边界)。

use std::path::PathBuf;

// 启动方式
#[derive(Debug, Clone, PartialEq)]
pub enum StartSpec {
    // bash <script> <cmd> — 自带生命周期的脚本 (start/stop 子命令)
    Script(PathBuf),
    // nohup <bin> <args> >> log 2>&1 & echo $! > pidfile — 前台进程 daemonize
    // (stop 用 pidfile kill; restart 由 restart_cmd_for 退化为 stop+start)
    Daemon { bin: String, args: Vec<String> },
}

// repo 相对路径下的脚本/二进制解析根: FUSION_ROOT 优先, 否则当前目录。
fn fusion_root() -> PathBuf {
    std::env::var("FUSION_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

// 服务名 → 启动方式。不在表内 → None (走默认 start.sh 路径)。
pub fn start_spec_for(name: &str) -> Option<StartSpec> {
    match name {
        "fusion-trainer-bridge" => Some(StartSpec::Script(
            fusion_root().join("fusion-trainer/scripts/trainer-bridge.sh"),
        )),
        "fusion-cowork-web" => Some(StartSpec::Daemon {
            bin: "fusion-cowork".into(),
            args: vec![
                "web".into(),
                "serve".into(),
                "--port".into(),
                "11461".into(),
            ],
        }),
        "fusion-plugins-ecosystem-console" => Some(StartSpec::Daemon {
            bin: "fusion-plugin-console".into(),
            args: vec!["--port".into(), "8795".into()],
        }),
        // issue #31: MCP server 主入口 (registry :8765), 前台 sse 进程 daemonize
        "fusion-plugins-ecosystem" => Some(StartSpec::Daemon {
            bin: "fusion-plugin-server".into(),
            args: vec![
                "--transport".into(),
                "sse".into(),
                "--port".into(),
                "8765".into(),
            ],
        }),
        _ => None,
    }
}

// 共享 venv 内的 CLI 入口优先 (daemon 通常不在 venv shell 里跑);
// 不存在则退回裸名交给 PATH。
pub fn resolve_bin(bin: &str) -> String {
    let candidate = fusion_root().join(".venv/bin").join(bin);
    if candidate.exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        bin.to_string()
    }
}

// supervisor 托管 pidfile 目录 (~/.fusion-sv/pids/)
pub fn pid_path(svc: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".fusion-sv/pids")
        .join(format!("{svc}.pid"))
}

// supervisor 托管服务日志 (~/.fusion-sv/logs/<svc>.log)
pub fn log_path(svc: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".fusion-sv/logs")
        .join(format!("{svc}.log"))
}

// daemonize start 命令 (bash -c 执行): nohup 后台 + 日志重定向 + pidfile。
// 注意 mkdir 用 `;` 分隔而非 `&&`: `A && B &` 会把整个列表后台化为子 shell,
// $! 拿到的是子 shell pid 而非真实进程 pid — stop 时 kill 掉的是壳, 服务进程
// 变孤儿继续占口。`;` 分隔后 nohup 单独后台, $! 即 nohup exec 后的真实 pid。
pub fn daemon_start_cmd(spec: &StartSpec, svc: &str) -> Option<String> {
    let StartSpec::Daemon { bin, args } = spec else {
        return None;
    };
    let bin = resolve_bin(bin);
    let log = log_path(svc);
    let pid = pid_path(svc);
    Some(format!(
        "mkdir -p '{dir}'; nohup '{bin}' {args} >> '{log}' 2>&1 & echo $! > '{pid}'",
        dir = pid.parent().unwrap().display(),
        args = args.join(" "),
        log = log.display(),
        pid = pid.display(),
    ))
}

// daemonize stop 命令: pidfile kill + 清理
pub fn daemon_stop_cmd(svc: &str) -> String {
    let pid = pid_path(svc);
    format!(
        "kill \"$(cat '{pid}')\" 2>/dev/null; rm -f '{pid}'",
        pid = pid.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trainer_bridge_uses_script() {
        let spec = start_spec_for("fusion-trainer-bridge").unwrap();
        assert!(matches!(spec, StartSpec::Script(_)));
    }

    #[test]
    fn test_cowork_web_daemon_spec() {
        let spec = start_spec_for("fusion-cowork-web").unwrap();
        match spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-cowork");
                assert!(args.contains(&"11461".to_string()));
            }
            _ => panic!("cowork-web 应为 Daemon spec"),
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
            panic!("cowork-web 应有 spec");
        };
        let cmd = daemon_start_cmd(&spec, "fusion-cowork-web").unwrap();
        assert!(cmd.contains("nohup"), "应 nohup 后台: {cmd}");
        assert!(
            cmd.contains("web serve --port 11461"),
            "应带完整参数: {cmd}"
        );
        assert!(
            cmd.contains(".fusion-sv/pids/fusion-cowork-web.pid"),
            "应写 pidfile: {cmd}"
        );
        assert!(cmd.contains("echo $!"), "应记录后台 pid: {cmd}");
    }

    #[test]
    fn test_daemon_start_cmd_none_for_script_spec() {
        let spec = start_spec_for("fusion-trainer-bridge").unwrap();
        assert!(daemon_start_cmd(&spec, "fusion-trainer-bridge").is_none());
    }

    // issue #31: pidfile 必须记真实进程 pid — mkdir 不得与 nohup 用 && 连接
    // (`&& ... &` 整列表后台化, $! = 子 shell pid, stop kill 掉壳留孤儿进程)
    #[test]
    fn test_daemon_start_cmd_no_subshell_pid() {
        let Some(spec) = start_spec_for("fusion-cowork-web") else {
            panic!("cowork-web 应有 spec");
        };
        let cmd = daemon_start_cmd(&spec, "fusion-cowork-web").unwrap();
        assert!(
            !cmd.contains("&& nohup"),
            "mkdir 与 nohup 不得用 && 连接 (pidfile 会记子 shell pid): {cmd}"
        );
        assert!(
            cmd.contains("; nohup"),
            "mkdir 应以 ; 分隔, nohup 单独后台: {cmd}"
        );
    }

    // issue #31: plugins-ecosystem MCP server (:8765) Daemon spec
    #[test]
    fn test_plugins_ecosystem_mcp_daemon_spec() {
        let spec = start_spec_for("fusion-plugins-ecosystem").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-plugin-server");
                assert!(args.contains(&"--transport".to_string()));
                assert!(args.contains(&"sse".to_string()));
                assert!(args.contains(&"8765".to_string()));
            }
            _ => panic!("plugins-ecosystem 应为 Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-plugins-ecosystem").unwrap();
        assert!(
            cmd.contains("--transport sse --port 8765"),
            "应带 MCP server 参数: {cmd}"
        );
    }

    #[test]
    fn test_daemon_stop_cmd_kills_pidfile() {
        let cmd = daemon_stop_cmd("fusion-cowork-web");
        assert!(cmd.contains("kill"), "stop 应 kill: {cmd}");
        assert!(
            cmd.contains("fusion-cowork-web.pid"),
            "stop 应读 pidfile: {cmd}"
        );
        assert!(cmd.contains("rm -f"), "stop 应清 pidfile: {cmd}");
    }
}
