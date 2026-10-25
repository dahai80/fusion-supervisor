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
        // issue #33: fusion-store 仓库根无 start.sh; deploy/start.sh 有上游 bug
        // (BINARY 指向不存在的 target/release/fusion-store, 实际产物是 fs-serve,
        //  上游 issue dahai80/fusion-store#5) — 直接 daemonize fs-serve 二进制
        // (自带默认口/环回监听, 探活见 probe_map)
        "fusion-store" => Some(StartSpec::Daemon {
            bin: "fusion-store/target/release/fs-serve".into(),
            args: vec![],
        }),
        // issue #33: fusion-smallbusiness 无 start.sh, CLI 入口直接跑 uvicorn
        // (口由 FSB env 决定, 探活见 probe_map)
        "fusion-smallbusiness" => Some(StartSpec::Daemon {
            bin: "fusion-smallbusiness".into(),
            args: vec![],
        }),
        // issue #33: fusion-simulation 无 start.sh, `fusion-sim service start`
        // 前台阻塞进程 (gRPC :11447 + metrics :11456, --gui 起 Web GUI :11455);
        // registry 的 -gui/-metrics 条目由该进程内嵌服务, 已 supervise:false
        "fusion-simulation" => Some(StartSpec::Daemon {
            bin: "fusion-sim".into(),
            args: vec![
                "service".into(),
                "start".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--gui".into(),
            ],
        }),
        // issue #34: fusion-cowork-ws = `fusion-cowork remote serve` 前台 WebSocket
        // 服务 (registry :11439), cowork start.sh 只管主 desk RPC 不含它
        "fusion-cowork-ws" => Some(StartSpec::Daemon {
            bin: "fusion-cowork".into(),
            args: vec!["remote".into(), "serve".into()],
        }),
        // issue #34: fusion-bench start.sh 引用已删除的 fusion_bench.api.server
        // 模块 (上游已迁 api.app, 上游 issue dahai80/fusion-bench#27) —
        // 直接 daemonize `fusion-bench serve` (前台 uvicorn, 默认口 11467)
        "fusion-bench" => Some(StartSpec::Daemon {
            bin: "fusion-bench".into(),
            args: vec!["serve".into()],
        }),
        // issue #34: fusion-k12-teacher start.sh 不收 start 子命令 (bare/-d/stop,
        // 上游 issue dahai80/fusion-k12-teacher#31) — 直接 daemonize
        // `fusion-k12 serve` (前台, 默认口 11448)
        "fusion-k12-teacher" => Some(StartSpec::Daemon {
            bin: "fusion-k12".into(),
            args: vec!["serve".into()],
        }),
        // issue #34: fusion-code-modenization start.sh 用 `python -m ...server.runner`
        // — runner 无 __main__ 入口, 导入即退出 0 (入口已迁 server/__main__.py,
        // 上游 issue dahai80/fusion-code-modelization#21) — 直接 daemonize 正确入口
        "fusion-code-modenization" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec!["-m".into(), "fusion_code_modelization.server".into()],
        }),
        _ => None,
    }
}

// 共享 venv 内的 CLI 入口优先 (daemon 通常不在 venv shell 里跑);
// 不存在则退回裸名交给 PATH。
// 含路径分隔符的 bin (如 fusion-store/target/release/fs-serve) 相对 FUSION_ROOT 解析。
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

    // issue #34: cowork-ws 用 remote serve 前台进程 daemonize
    #[test]
    fn test_cowork_ws_daemon_spec() {
        let spec = start_spec_for("fusion-cowork-ws").unwrap();
        match spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-cowork");
                assert_eq!(args, vec!["remote".to_string(), "serve".to_string()]);
            }
            _ => panic!("cowork-ws 应为 Daemon spec"),
        }
    }

    // issue #34: bench/k12/modenization start.sh 上游破损, Daemon spec 直跑正确入口
    #[test]
    fn test_issue34_broken_start_sh_daemon_specs() {
        let spec = start_spec_for("fusion-bench").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-bench");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-bench 应为 Daemon spec"),
        }

        let spec = start_spec_for("fusion-k12-teacher").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-k12");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-k12-teacher 应为 Daemon spec"),
        }

        let spec = start_spec_for("fusion-code-modenization").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, ".venv/bin/python");
                assert_eq!(
                    args,
                    &vec![
                        "-m".to_string(),
                        "fusion_code_modelization.server".to_string()
                    ]
                );
            }
            _ => panic!("fusion-code-modenization 应为 Daemon spec"),
        }
        // resolve_bin 路径形式: .venv/bin/python 必须解析到 FUSION_ROOT 下
        let r = resolve_bin(".venv/bin/python");
        assert!(r.contains(".venv/bin/python"), "解析结果异常: {r}");
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

    // issue #33: fusion-store 直接 daemonize fs-serve 二进制 (deploy/start.sh 上游
    // BINARY 名不匹配, 走不了); 含路径的 bin 相对 FUSION_ROOT 解析
    #[test]
    fn test_fusion_store_daemon_spec() {
        let spec = start_spec_for("fusion-store").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-store/target/release/fs-serve");
                assert!(args.is_empty(), "零参启动, 口走 fs-serve 内置默认");
            }
            _ => panic!("fusion-store 应为 Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-store").unwrap();
        assert!(cmd.contains("fs-serve"), "应启动 fs-serve 二进制: {cmd}");
    }

    // 含路径分隔符的 bin 相对 FUSION_ROOT 解析; 纯名走 .venv/bin
    #[test]
    fn test_resolve_bin_path_form() {
        // 存在与否都不得拼进 .venv/bin (路径带 / 拼接语义错误)
        let r = resolve_bin("fusion-store/target/release/fs-serve");
        assert!(!r.contains(".venv"), "路径形式 bin 不得走 .venv 解析: {r}");
        assert!(r.contains("fs-serve"));
    }

    // issue #33: fusion-smallbusiness CLI 直接跑 uvicorn (:11490)
    #[test]
    fn test_fusion_smallbusiness_daemon_spec() {
        let spec = start_spec_for("fusion-smallbusiness").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-smallbusiness");
                assert!(args.is_empty(), "零参启动, 口走 FSB env 默认值");
            }
            _ => panic!("fusion-smallbusiness 应为 Daemon spec"),
        }
    }

    // issue #33: fusion-sim service start 前台阻塞, daemonize + --gui
    // (gui :11455 / metrics :11456 由同一进程内嵌, registry 已 supervise:false)
    #[test]
    fn test_fusion_simulation_daemon_spec() {
        let spec = start_spec_for("fusion-simulation").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args } => {
                assert_eq!(bin, "fusion-sim");
                assert!(args.contains(&"service".to_string()));
                assert!(args.contains(&"start".to_string()));
                assert!(args.contains(&"--gui".to_string()));
            }
            _ => panic!("fusion-simulation 应为 Daemon spec"),
        }
        let cmd = daemon_start_cmd(&spec, "fusion-simulation").unwrap();
        assert!(cmd.contains("service start"), "应带 service start: {cmd}");
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
