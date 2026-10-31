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
    // issue #45: cwd 相对 FUSION_ROOT (None=继承 daemon cwd);
    // env 为 app 内部固定 env (如 config 路径), 非 secret — secret 走 env_map 注入。
    // issue #49: env_file 相对 FUSION_ROOT 的 dotenv 文件, start 前 set -a source
    // 注入 (服务自身 .env 是唯一真源, 覆盖 daemon 进程 env 快照里的旧值);
    // 文件缺失 fail-closed 拒启 (bash exit 1)。
    Daemon {
        bin: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        env_file: Option<String>,
    },
}

// 无 cwd/env 的便捷构造 (既有覆盖服务全用此形态)
fn daemon(bin: &str, args: &[&str]) -> StartSpec {
    StartSpec::Daemon {
        bin: bin.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: Vec::new(),
        env_file: None,
    }
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
        // issue #81: fusion-office 根 start.sh 只管 11485 shell (shell-serve.mjs),
        // api (registry :11492) 的生命周期脚本在嵌套 services/office-api/ 下
        // (start|stop|restart|status|log, 自带 pidfile + /healthz 健康等待)。
        // 走 Script 编排脚本原样, 不改上游 (只能修改本工程, 上游问题提 issue)。
        "fusion-office-api" => Some(StartSpec::Script(
            fusion_root().join("fusion-office/services/office-api/start.sh"),
        )),
        "fusion-cowork-web" => Some(daemon(
            "fusion-cowork",
            &["web", "serve", "--port", "11461"],
        )),
        "fusion-plugins-ecosystem-console" => {
            Some(daemon("fusion-plugin-console", &["--port", "8795"]))
        }
        // issue #31: MCP server 主入口 (registry :8765), 前台 sse 进程 daemonize
        "fusion-plugins-ecosystem" => Some(daemon(
            "fusion-plugin-server",
            &["--transport", "sse", "--port", "8765"],
        )),
        // issue #33: fusion-store 仓库根无 start.sh; deploy/start.sh 有上游 bug
        // (BINARY 指向不存在的 target/release/fusion-store, 实际产物是 fs-serve,
        //  上游 issue dahai80/fusion-store#5) — 直接 daemonize fs-serve 二进制
        // (自带默认口/环回监听, 探活见 probe_map)
        "fusion-store" => Some(daemon("fusion-store/target/release/fs-serve", &[])),
        // issue #33: fusion-smallbusiness 无 start.sh, CLI 入口直接跑 uvicorn
        // (口由 FSB env 决定, 探活见 probe_map)
        "fusion-smallbusiness" => Some(daemon("fusion-smallbusiness", &[])),
        // osagent issue#6: serve-metrics 迁 9100→11571 后由 supervisor 接管 —
        // fusion-osagent 无 start.sh, serve-metrics 是前台 CLI 进程 (proctitle
        // `fusion-osagent serve-metrics :11571`), daemonize 接管
        "fusion-osagent-metrics" => Some(daemon(
            "fusion-osagent",
            &["serve-metrics", "--port", "11571"],
        )),
        // issue #33: fusion-simulation 无 start.sh, `fusion-sim service start`
        // 前台阻塞进程 (gRPC :11447 + metrics :11456, --gui 起 Web GUI :11455);
        // registry 的 -gui/-metrics 条目由该进程内嵌服务, 已 supervise:false
        "fusion-simulation" => Some(daemon(
            "fusion-sim",
            &["service", "start", "--host", "127.0.0.1", "--gui"],
        )),
        // issue #34: fusion-cowork-ws = `fusion-cowork remote serve` 前台 WebSocket
        // 服务 (registry :11439), cowork start.sh 只管主 desk RPC 不含它
        "fusion-cowork-ws" => Some(daemon("fusion-cowork", &["remote", "serve"])),
        // issue #34: fusion-bench start.sh 引用已删除的 fusion_bench.api.server
        // 模块 (上游已迁 api.app, 上游 issue dahai80/fusion-bench#27) —
        // 直接 daemonize `fusion-bench serve` (前台 uvicorn, 默认口 11467)
        "fusion-bench" => Some(daemon("fusion-bench", &["serve"])),
        // issue #34/#49: fusion-k12-teacher start.sh `start` 子命令前台 exec (上游
        // dahai80/fusion-k12-teacher#31 已修但脚本路径仍前台阻塞) — daemonize
        // `fusion-k12 serve` (默认口 11448)。issue #49: bare serve 不加载项目自身
        // .env (FUSION_MLX_API_KEY 等), 继承 daemon env 快照里的旧 key → gateway
        // 401 → /api/health 503 backend down → 探活重启循环。env_file 指向项目
        // .env (唯一真源), start 前 set -a source 注入, 覆盖旧值。
        "fusion-k12-teacher" => Some(StartSpec::Daemon {
            bin: "fusion-k12".into(),
            args: vec!["serve".into()],
            cwd: None,
            env: Vec::new(),
            env_file: Some("fusion-k12-teacher/.env".into()),
        }),
        // issue #34: fusion-code-modenization start.sh 用 `python -m ...server.runner`
        // — runner 无 __main__ 入口, 导入即退出 0 (入口已迁 server/__main__.py,
        // 上游 issue dahai80/fusion-code-modelization#21) — 直接 daemonize 正确入口
        "fusion-code-modenization" => Some(daemon(
            ".venv/bin/python",
            &["-m", "fusion_code_modelization.server"],
        )),
        // issue #45: business 平台三个 arg-less start.sh (exec 前台进程, 不解析
        // start/stop 参数) — `bash start.sh stop` 会反向启动服务, 走不了脚本路径,
        // 与 bench/k12/modenization 同款 daemonize。FUSION_CLIENT_KEY (secret) 不在
        // 此表 — 走 env_map 注入 (进程 env → .env → gateway config 兜底)。
        // issue #55: registry key + dir 均已改名 ai-fit; start.sh 前台 exec
        // uvicorn server.main:app (cwd=repo 根, 无 --app-dir — 旧 fit-ai spec 的
        // --app-dir server 是错的, 此前靠端口上残留进程探活掩盖)。
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
            // start.sh 在 cd backend 前以 $PWD 解析 config → 相对 cwd (=repo 根) 恰好等价
            env: vec![(
                "FUSION_AI_SPEECH_EXAM_CONFIG".into(),
                "config/config.yaml".into(),
            )],
            env_file: None,
        }),
        // issue #55: 上游 start.sh 已从静态 http.server 改为 FastAPI 后端
        // (uvicorn server.app:app, 11520); web 静态分发拆到 ai-apparelmockup-web。
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
        // ---- issue #55: 2026-10-27/28 端口治理批量登记的 business 条目 ----
        // 约定 1 标准 start.sh 全部前台 exec → 同款 daemonize; 端口逐一钉死
        // (上游 config 默认值多为迁移前旧口, TASK-PORT-PROC-2026-10-27 未落地)。
        // FUSION_CLIENT_KEY 一律走 env_map 注入, 不在此表。
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
        // backend.app 无 CLI 参数, 端口读 FUSION_AI_INFORMATION_SEARCH_PORT
        "ai-information-search" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec!["-m".into(), "backend.app".into()],
            cwd: Some("business/ai-information-search".into()),
            env: vec![("FUSION_AI_INFORMATION_SEARCH_PORT".into(), "11595".into())],
            env_file: None,
        }),
        // fusion_ai_operabase.main 端口读 FUSION_AI_OPERABASE_PORT
        "ai-OperaBase" => Some(StartSpec::Daemon {
            bin: ".venv/bin/python".into(),
            args: vec!["-m".into(), "fusion_ai_operabase.main".into()],
            cwd: Some("business/ai-OperaBase".into()),
            env: vec![("FUSION_AI_OPERABASE_PORT".into(), "11504".into())],
            env_file: None,
        }),
        // charactervoice 无 start.sh; config 默认口仍是迁移前 11480/11481 → 显式钉口
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
        // customer-insight 双条目共享同一 repo: web 静态分发 (11594) +
        // API 预留口 (11501, API 未实现前先托管静态, API 落地后换入口)
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
        // apparelmockup 拆出的 web 静态分发 (无 /health → TCP 探活)
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

// 防 idle sleep 陪护: launchd 迁 supervisor 后, 原 plist 的 caffeinate -i 随迁
// 丢失 (2026-10-30)。fusion-gupiao 凌晨 01:00 daily_suggest 长扫描会被系统
// idle sleep 打断。start/restart 成功后以 `caffeinate -i -w <服务 pid>` 挂
// 系统断言 — 服务退出/被停时 caffeinate 自动退出释放断言, 无需 stop 路径配合。
// 上游正解是 gupiao start.sh 自带 caffeinate 选项 (fusion-gupiao#6),
// 此表为 supervisor 侧过渡补丁。
pub fn caffeinate_pidfile(svc: &str) -> Option<&'static str> {
    match svc {
        "fusion-gupiao" => Some("~/.fusion-gupiao/run.pid"),
        _ => None,
    }
}

// daemonize start 命令 (bash -c 执行): nohup 后台 + 日志重定向 + pidfile。
// 注意 mkdir/cd/export 用 `;` 分隔而非 `&&`: `A && B &` 会把整个列表后台化为
// 子 shell, $! 拿到的是子 shell pid 而非真实进程 pid — stop 时 kill 掉的是壳,
// 服务进程变孤儿继续占口。`;` 分隔后 nohup 单独后台, $! 即 nohup exec 后的真实 pid。
// issue #45: cd 用 `|| exit 1` 守护 (cwd 不存在时拒启, 不在错误目录裸跑)。
// issue #49: env_file 在 cd 之后 set -a source (dotenv 覆盖 daemon env 快照旧值),
// 文件缺失 `|| exit 1` fail-closed 拒启。
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
    Some(format!(
        "{pre}nohup '{bin}' {args} >> '{log}' 2>&1 & echo $! > '{pid}'",
        args = args.join(" "),
        log = log.display(),
        pid = pid.display(),
    ))
}

// daemonize stop 命令: pidfile kill + 宽限等待 + 升级 SIGKILL + 清理。
// 部分前台化服务 (如 fusion-k12-teacher) 忽略 SIGTERM (优雅关闭卡死), 只 TERM
// 会出现 down 返回 ok / pidfile 已删 / 进程仍存活的假停止 — 必须 -9 兜底。
// 宽限 5s (25×0.2s), 正常进程 <1s 退出即提前跳出; 死循环等价 5s 阻塞, 逐服务串行可接受。
pub fn daemon_stop_cmd(svc: &str) -> String {
    let pid = pid_path(svc);
    format!(
        "p=$(cat '{pid}' 2>/dev/null || true); \
         if [ -n \"$p\" ]; then \
           kill \"$p\" 2>/dev/null || true; \
           i=0; while kill -0 \"$p\" 2>/dev/null && [ $i -lt 25 ]; do sleep 0.2; i=$((i+1)); done; \
           kill -0 \"$p\" 2>/dev/null && kill -9 \"$p\" 2>/dev/null || true; \
         fi; \
         rm -f '{pid}'",
        pid = pid.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // caffeinate 陪护表: gupiao 有, 其他服务无
    #[test]
    fn test_caffeinate_pidfile_table() {
        assert_eq!(
            caffeinate_pidfile("fusion-gupiao"),
            Some("~/.fusion-gupiao/run.pid")
        );
        assert_eq!(caffeinate_pidfile("fusion-mlx"), None);
        assert_eq!(caffeinate_pidfile("unknown-svc"), None);
    }

    #[test]
    fn test_trainer_bridge_uses_script() {
        let spec = start_spec_for("fusion-trainer-bridge").unwrap();
        assert!(matches!(spec, StartSpec::Script(_)));
    }

    // issue #81: office-api 走嵌套 services/office-api/start.sh (非仓库根脚本)
    #[test]
    fn test_office_api_uses_script() {
        let spec = start_spec_for("fusion-office-api").unwrap();
        match spec {
            StartSpec::Script(p) => {
                assert!(
                    p.ends_with("fusion-office/services/office-api/start.sh"),
                    "office-api 脚本路径错误: {:?}",
                    p
                );
            }
            _ => panic!("office-api 应为 Script spec"),
        }
    }

    // issue #34: cowork-ws 用 remote serve 前台进程 daemonize
    #[test]
    fn test_cowork_ws_daemon_spec() {
        let spec = start_spec_for("fusion-cowork-ws").unwrap();
        match spec {
            StartSpec::Daemon { bin, args, .. } => {
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
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-bench");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-bench 应为 Daemon spec"),
        }

        let spec = start_spec_for("fusion-k12-teacher").unwrap();
        match &spec {
            StartSpec::Daemon { bin, args, .. } => {
                assert_eq!(bin, "fusion-k12");
                assert_eq!(args, &vec!["serve".to_string()]);
            }
            _ => panic!("fusion-k12-teacher 应为 Daemon spec"),
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
            StartSpec::Daemon { bin, args, .. } => {
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
            StartSpec::Daemon { bin, args, .. } => {
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
            StartSpec::Daemon { bin, args, .. } => {
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
            StartSpec::Daemon { bin, args, .. } => {
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
            StartSpec::Daemon { bin, args, .. } => {
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

    // issue #45: business 平台三个 arg-less start.sh → daemonize + cwd + env
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
                    "uvicorn 应从 repo 根导入"
                );
                assert_eq!(cwd.as_deref(), Some("business/ai-fit"));
                assert!(env.is_empty(), "ai-fit 无固定 env (key 走 env_map)");
            }
            _ => panic!("ai-fit 应为 Daemon spec"),
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
                    "应带 config 相对路径 env: {env:?}"
                );
            }
            _ => panic!("ai-speech-exam 应为 Daemon spec"),
        }

        // issue #55: apparelmockup 上游改 FastAPI — 不再是 http.server
        let spec = start_spec_for("ai-apparelmockup").unwrap();
        match &spec {
            StartSpec::Daemon { args, cwd, .. } => {
                assert!(args.contains(&"server.app:app".to_string()));
                assert!(args.contains(&"11520".to_string()));
                assert!(!args.contains(&"http.server".to_string()));
                assert_eq!(cwd.as_deref(), Some("business/ai-apparelmockup"));
            }
            _ => panic!("ai-apparelmockup 应为 Daemon spec"),
        }
    }

    // issue #55: 批量登记条目 — 端口钉死 (上游 config 默认口未迁移),
    // 无 CLI 端口参数的走 env, 共享 repo 双条目 cwd 一致
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
                        "{name} 端口须钉死 {port}: {args:?}"
                    );
                    assert_eq!(cwd.as_deref(), Some("business/ai-charactervoice"));
                }
                _ => panic!("{name} 应为 Daemon spec"),
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
            _ => panic!("ai-information-search 应为 Daemon spec"),
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
            _ => panic!("ai-operabase 应为 Daemon spec"),
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
                _ => panic!("{name} 应为 Daemon spec"),
            }
        }

        for name in ["ai-ads-videobuilder", "ai-doc-automation"] {
            let spec = start_spec_for(name).unwrap();
            match &spec {
                StartSpec::Daemon { args, .. } => {
                    assert!(args.contains(&"serve".to_string()), "{name}: {args:?}");
                    assert!(args.contains(&"--port".to_string()));
                }
                _ => panic!("{name} 应为 Daemon spec"),
            }
        }

        // osagent issue#6: serve-metrics Daemon spec (9100→11571 迁移后接管)
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
            _ => panic!("fusion-osagent-metrics 应为 Daemon spec"),
        }
    }

    // issue #45: cwd/env 进 start 命令 — cd 带 || exit 1 守护, export 前置,
    // 且不破坏 `; nohup` pid 纪律 (pidfile 记真实进程 pid)
    #[test]
    fn test_issue42_daemon_cmd_cwd_env_shape() {
        let spec = start_spec_for("ai-speech-exam").unwrap();
        let cmd = daemon_start_cmd(&spec, "ai-speech-exam").unwrap();
        assert!(
            cmd.contains("business/ai-speech-exam"),
            "应 cd 到 repo: {cmd}"
        );
        assert!(cmd.contains("|| exit 1;"), "cd 失败应拒启: {cmd}");
        assert!(
            cmd.contains("export FUSION_AI_SPEECH_EXAM_CONFIG='config/config.yaml';"),
            "应 export config 路径: {cmd}"
        );
        assert!(!cmd.contains("&& nohup"), "不得破坏 pid 纪律: {cmd}");
        assert!(cmd.contains("; nohup"), "nohup 须 ; 分隔: {cmd}");
    }

    #[test]
    fn test_daemon_stop_cmd_kills_pidfile() {
        let cmd = daemon_stop_cmd("fusion-cowork-web");
        assert!(cmd.contains("kill"), "stop 应 kill: {cmd}");
        assert!(
            cmd.contains("fusion-cowork-web.pid"),
            "stop 应读 pidfile: {cmd}"
        );
        assert!(cmd.contains("kill -9"), "stop 应升级 SIGKILL: {cmd}");
        assert!(cmd.contains("rm -f"), "stop 应清 pidfile: {cmd}");
    }

    // 忽略 SIGTERM 的服务必须被升级击杀 (生产 bug: k12 down 后进程仍存活)
    #[test]
    fn test_daemon_stop_cmd_sigterm_escalation() {
        let cmd = daemon_stop_cmd("fusion-k12-teacher");
        assert!(cmd.contains("kill -0 \"$p\""), "应轮询进程存活: {cmd}");
        assert!(cmd.contains("kill -9 \"$p\""), "TERM 后仍存活应 -9: {cmd}");
        assert!(cmd.contains("rm -f"), "击杀后清 pidfile: {cmd}");
    }

    // issue #49: k12 daemonize 前必须 source 项目自身 .env (唯一真源, 覆盖 daemon
    // env 快照旧 key); 文件缺失 fail-closed 拒启; 其他 Daemon 服务不带该段
    #[test]
    fn test_issue49_k12_env_file_source_segment() {
        let spec = start_spec_for("fusion-k12-teacher").unwrap();
        let StartSpec::Daemon {
            env_file: Some(env_file),
            ..
        } = &spec
        else {
            panic!("fusion-k12-teacher 应为带 env_file 的 Daemon spec");
        };
        assert_eq!(env_file, "fusion-k12-teacher/.env");

        let cmd = daemon_start_cmd(&spec, "fusion-k12-teacher").unwrap();
        let seg = format!(
            "set -a; [ -f '{p}' ] || exit 1; source '{p}'; set +a; ",
            p = fusion_root().join(env_file).display(),
        );
        assert!(cmd.contains(&seg), "应带 fail-closed source 段: {cmd}");
        // source 段须在 nohup 之前 (env 注入先于进程启动)
        assert!(
            cmd.find(&seg).unwrap() < cmd.find("nohup").unwrap(),
            "source 段应在 nohup 前: {cmd}"
        );
        assert!(!cmd.contains("&& nohup"), "不得破坏 pid 纪律: {cmd}");

        // 无 env_file 的 Daemon 服务命令不含 source 段
        let plain =
            daemon_start_cmd(&start_spec_for("fusion-bench").unwrap(), "fusion-bench").unwrap();
        assert!(
            !plain.contains("set -a;"),
            "无 env_file 服务不应 source: {plain}"
        );
    }
}
