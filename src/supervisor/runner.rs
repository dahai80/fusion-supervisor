use crate::alert::{Alert, AlertKind, Alerter};
use crate::manifest::ServiceDef;
use crate::probe::ProbeResult;
use crate::store::HistoryStore;
use crate::supervisor::policy::RestartPolicy;
use anyhow::Result;

pub type ProbeFn = Box<dyn Fn() -> ProbeResult + Send + Sync>;
pub type StartFn = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

// issue #47: 配置致命退出码 (sysexits.h EX_CONFIG)。fusion-gateway 契约:
// 78 = 配置坏且自愈失败 (重启无救, 需人工修配置), 其它退出码 = 普通 crash。
pub(crate) const EXIT_CONFIG: i32 = 78;
// issue #47: 快速失败窗口 (秒) — start 成功后窗口内又探活不可达 = fast-exit
const FAST_FAIL_WINDOW_SEC: u64 = 10;
// issue #47: 连续快速失败熔断阈值 (达阈值 → 告警 + 退避升级)
const FAST_FAIL_STREAK: u32 = 3;
// issue #47: config-fatal 封顶次数 (达限标 Failed, 人工 fusion-sv restart 复位)
const CONFIG_FAIL_CAP: u32 = 5;
// issue #79: 连续失败重启阈值 — 探活连续 N 次失败才进重启流。
// 繁忙服务 (fusion-mlx GIL 高负载) /healthz 单次超时抖动不得升级重启;
// 中间任一次成功探活即复位计数, 抖动永不升级。真宕机 3 连败
// (poll_unhealthy_sec=2 ≈ 6s) 后照常重启, 仍然及时。
const RESTART_FAIL_STREAK: u32 = 3;

// issue #47: 熔断退避序列 10s → 60s → 300s (封顶)。区别于普通 crash 的 1→2→4→…:
// config-fatal / 快速失败时快速重试只会空烧进程启动, 拉长间隔等人工修复。
fn escalated_delay(n: u32) -> u64 {
    match n {
        0 | 1 => 10,
        2 => 60,
        _ => 300,
    }
}

// issue #47: 从 exec_* 失败信息提取退出码 (约定格式 "exit code=<n>",
// 见 supervisor/mod.rs 的 bail! 格式 — 改一处必须同步另一处)。
fn exit_code_of(err_msg: &str) -> Option<i32> {
    let rest = err_msg.split("exit code=").nth(1)?;
    let digits: &str = rest
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .next()
        .unwrap_or("");
    digits.parse().ok()
}

// issue #86: 检测操作员是否正在对该服务跑 start.sh (任意子命令)。
// 端口探活无法区分 "操作员 do_restart 的 stop 窗口" 与 "真宕机" — 两者都
// ConnectionRefused。supervisor 若在操作员 restart 期间抢跑自动重启:
// 1) 与操作员的 do_start 竞态双拉进程; 2) supervisor 的 exec 只带 daemon
// env 快照 (env_n=0), 操作员行内 env 覆盖被静默丢弃 (2026-10-31 fusion-mlx
// FUSION_MLX_HEAVY_ADMISSION_LINE=0.99 实证丢失)。
// 检测两阶段:
//   阶段 1: pgrep -f "<repo_dir>/start[.]sh" — cmdline 含带 repo 前缀的路径
//   (绝对路径调用 / `bash fusion-mlx/start.sh` 相对 monorepo 根调用)。
//   阶段 2 (2026-10-31 生产补): 操作员 cd 进 repo 后 `./start.sh restart` —
//   cmdline 只有 "./start.sh", 无 repo 前缀, 阶段 1 漏检 (fusion-mlx
//   env_n=0 抢跑实证)。按 basename 捞候选进程, 逐个用 lsof 读 cwd, 与
//   FUSION_ROOT/<repo_dir> 比对。
// 自匹配防护: 模式用 [.] 括号写法, 同 repo 并发 pgrep cmdline 含字面
// "start[.]sh", 正则只匹配 "start.sh" 不匹配 "start[.]sh", 杜绝 pgrep 互配
// (测试并行跑出过 7 个假阳性); supervisor 自身 exec 在核心锁内同步执行,
// tick 不会并发看到自己的 start.sh 进程。pgrep/lsof 不可用 → false
// (pass-open, 行为回落到修复前)。
fn pgrep_pids(pat: &str) -> Option<Vec<String>> {
    match std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pat)
        .output()
    {
        Ok(o) if o.status.success() => Some(
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
        ),
        Ok(_) => Some(Vec::new()),
        Err(e) => {
            tracing::warn!(err = %e, "pgrep 不可用, 视为无操作员脚本在跑 (pass-open)");
            None
        }
    }
}

// lsof 读进程 cwd: `lsof -a -p <pid> -d cwd -Fn` 输出 p<pid> / n<path> 两行。
// 返回 canonicalize 后路径 (/var 系 symlink 目录在 macOS 上解析为 /private/...)。
fn proc_cwd(pid: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", pid, "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('n') {
            return std::fs::canonicalize(p).ok().or_else(|| Some(p.into()));
        }
    }
    None
}

fn start_sh_process_alive(repo_dir: &str) -> bool {
    // 阶段 1: cmdline 带路径前缀
    let pat = format!("{repo_dir}/start[.]sh");
    if pgrep_pids(&pat).is_some_and(|p| !p.is_empty()) {
        return true;
    }
    // 阶段 2: cwd 相对调用 (./start.sh), 按 cwd 鉴别归属
    let Some(pids) = pgrep_pids("start[.]sh") else {
        return false;
    };
    let root = std::env::var("FUSION_ROOT").unwrap_or_else(|_| ".".to_string());
    let want = std::path::Path::new(&root).join(repo_dir);
    let want = std::fs::canonicalize(&want).unwrap_or(want);
    for pid in pids {
        if let Some(cwd) = proc_cwd(&pid)
            && cwd == want
        {
            return true;
        }
    }
    false
}

// 状态机: Stopped→Starting→Healthy→(Unhealthy)→Restarting→Healthy|Failed
//         Healthy→Draining→Stopped (drain 完成) | Healthy→Draining→Healthy (drain 取消)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    Restarting,
    Failed,
    // issue #9: 摘流量中。探活仍跑, 但 tick 不重启 (draining 服务退出是预期)。
    // grace 到期或探活不可达 (in-flight 完成/服务停止响应) → 转 Stopped。
    Draining,
}

pub struct ServiceRunner {
    pub def: ServiceDef,
    pub state: State,
    pub policy: RestartPolicy,
    pub restart_cmd: String, // "restart" | "stop+start"
    // issue #9: drain 宽限期截止 (unix sec)。None=非 draining 或无 grace。
    pub drain_grace_until: Option<u64>,
    // issue #31: 退避重启截止 (unix sec)。None=无待执行的重启。
    // 此前退避是持锁 sleep — 多个故障服务会把 SupervisorCore Mutex 扣住
    // 数十秒, 饿死 RPC/HTTP status。改为记截止时间本 tick 直接返回,
    // 后续 tick 到点再真正 invoke_start (poll_unhealthy_sec=2s, 误差 ≤2s)。
    pub restart_after: Option<u64>,
    // issue #47: 上次 invoke_start 成功时刻 (unix sec)。None=从未成功 start。
    last_start_ts: Option<u64>,
    // issue #47: 连续快速失败计数 — start 成功后 FAST_FAIL_WINDOW_SEC 内又探活不可达。
    fast_fail_streak: u32,
    // issue #47: 快速失败熔断告警 latch (达阈值告警一次, 恢复 Healthy 复位, 不刷屏)
    fast_fail_alerted: bool,
    // issue #47: config-fatal (exit 78) 连续计数 — 达 CONFIG_FAIL_CAP 标 Failed。
    config_fail_streak: u32,
    // issue #79: 探活连续失败计数 — 达 RESTART_FAIL_STREAK 才进重启流。
    // 任一次成功探活即复位 (抖动不升级)。pub 供集成测试预置。
    pub fail_streak: u32,
    // issue #86: 操作员 start.sh 竞态告警 latch — 首次跳过自动重启时 WARN
    // 一次 (含行内 env 丢失提示), 持续跳过期间静默; 恢复 Healthy 复位。
    operator_race_notified: bool,
    probe_fn: ProbeFn,
    start_fn: StartFn,
}

impl ServiceRunner {
    pub fn new(
        def: ServiceDef,
        restart_cmd: &str,
        probe_fn: impl Fn() -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str) -> Result<()> + Send + Sync + 'static,
        policy: RestartPolicy,
    ) -> Self {
        Self {
            def,
            state: State::Stopped,
            policy,
            restart_cmd: restart_cmd.to_string(),
            drain_grace_until: None,
            restart_after: None,
            last_start_ts: None,
            operator_race_notified: false,
            fast_fail_streak: 0,
            fast_fail_alerted: false,
            config_fail_streak: 0,
            fail_streak: 0,
            probe_fn: Box::new(probe_fn),
            start_fn: Box::new(start_fn),
        }
    }

    // 单步监督。now_ts=unix sec (注入, 确定性)。
    pub async fn tick(
        &mut self,
        now_ts: u64,
        store: &HistoryStore,
        alerter: &mut Alerter,
    ) -> Result<()> {
        let result = (self.probe_fn)();
        self.persist_health(&result, now_ts, store);
        // issue #9: draining 服务不进重启流。探活仍记录健康。
        // 探活不可达 (in-flight 完成/服务停响应) 或 grace 到期 → Stopped + 告警。
        if self.state == State::Draining {
            let grace_expired = self.drain_grace_until.map(|t| now_ts >= t).unwrap_or(true);
            let drained = !matches!(result, ProbeResult::Healthy { .. }) || grace_expired;
            if drained {
                tracing::info!(svc = %self.def.name, grace_expired, "drain 完成 → Stopped");
                self.state = State::Stopped;
                self.drain_grace_until = None;
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::ServiceDrained,
                        detail: format!("drain 完成 (grace_expired={grace_expired})",),
                        ts: now_ts,
                    })
                    .await;
            }
            // draining 期间保持 Draining, 不重启, 不告警 ServiceDown
            return Ok(());
        }
        match result {
            ProbeResult::Healthy { latency_ms } => {
                let was_down = matches!(self.state, State::Unhealthy | State::Restarting);
                self.state = State::Healthy;
                // issue #79: 任一次成功探活即复位连续失败计数 (抖动不升级)。
                self.fail_streak = 0;
                if was_down {
                    // issue #79: 恢复时清掉上一轮失败留下的陈旧排期,
                    // 否则下次进 Unhealthy 会直接命中过期 due 立即重启。
                    self.restart_after = None;
                    tracing::info!(svc = %self.def.name, "recovered");
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::ServiceBack,
                            detail: format!("latency {}ms", latency_ms),
                            ts: now_ts,
                        })
                        .await;
                    self.policy.reset();
                    // issue #47: 恢复即复位熔断计数 (快速失败 / config-fatal)
                    self.fast_fail_streak = 0;
                    self.fast_fail_alerted = false;
                    self.config_fail_streak = 0;
                    // issue #86: 恢复复位操作员竞态告警 latch
                    self.operator_race_notified = false;
                }
            }
            ProbeResult::Unhealthy { reason } => {
                self.handle_unhealthy(reason, now_ts, alerter).await;
            }
        }
        Ok(())
    }

    fn persist_health(&self, result: &ProbeResult, now_ts: u64, store: &HistoryStore) {
        let (status, latency) = match result {
            ProbeResult::Healthy { latency_ms } => ("healthy", *latency_ms),
            ProbeResult::Unhealthy { .. } => ("unhealthy", 0),
        };
        if let Err(e) = store.record_health(&self.def.name, status, latency, now_ts, "native") {
            tracing::warn!(svc = %self.def.name, err = %e, "health write fail (非致命, 继续监督)");
        }
    }

    async fn handle_unhealthy(
        &mut self,
        reason: crate::probe::UnhealthyReason,
        now_ts: u64,
        alerter: &mut Alerter,
    ) {
        // 防风暴: BadPath 不重启不记 crash, 只告警路径错
        if !reason.restart_eligible() {
            tracing::warn!(
                svc = %self.def.name,
                reason = ?reason,
                "探活不可达但非宕机信号, 不重启 (查 probe_map 路径)"
            );
            // issue #49: 同一根因每 tick (2s) 重复告警刷爆日志 — 只在状态转移进
            // Unhealthy 的首次告警, 持续 Unhealthy 期间静默; 恢复 Healthy 后
            // 再失败会重新告警 (状态机复位即解除静默)。
            // Stopped/Failed/Draining 探活失败不改状态 — 手动停止的服务
            // (down_one) 不能被下一 tick 翻回 Unhealthy 变红。
            if matches!(self.state, State::Stopped | State::Failed | State::Draining) {
                return;
            }
            let first_transition = !matches!(self.state, State::Unhealthy);
            self.state = State::Unhealthy;
            if first_transition {
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::RestartFailed,
                        detail: format!("probe path suspect: {:?}", reason),
                        ts: now_ts,
                    })
                    .await;
            }
            return;
        }
        // 真宕机信号 (ConnectionRefused/Timeout)
        // issue #79: 人工态 (Failed/Stopped) 不累计; 其余连续失败计数 +1,
        // 达 RESTART_FAIL_STREAK 才进重启流。
        if !matches!(self.state, State::Failed | State::Stopped) {
            self.fail_streak += 1;
        }
        match self.state {
            State::Healthy | State::Starting => {
                tracing::warn!(svc = %self.def.name, reason = ?reason, "服务宕机");
                self.state = State::Unhealthy;
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::ServiceDown,
                        detail: format!("{:?}", reason),
                        ts: now_ts,
                    })
                    .await;
                // 首次宕机不立即重启, 等下一 tick (避免抖动误判)
            }
            State::Unhealthy | State::Restarting => {
                // issue #79: 连续失败未达阈值不排重启 — 繁忙服务 (fusion-mlx
                // GIL 高负载) 探活抖动中间夹成功探活即复位, 永不升级;
                // 真宕机 3 连败 (poll 2s ≈ 6s) 后排程, 仍然及时。
                if self.fail_streak < RESTART_FAIL_STREAK {
                    tracing::info!(
                        svc = %self.def.name,
                        streak = self.fail_streak,
                        "连续失败未达阈值, 暂不排重启"
                    );
                    return;
                }
                // issue #31: 退避不持锁 sleep (旧实现扣锁数十秒饿死 status/metrics),
                // 改为记截止时间本 tick 返回, 到点 tick 再真正重启。
                // crash 记账语义与旧版一致: 每次实际重启尝试记 1 次。
                match self.restart_after {
                    None => {
                        // issue #47: 熔断生效时退避走 10→60→300 序列,
                        // 不走普通 crash 的 1→2→4 快速序列 (空烧进程启动)。
                        let delay = if self.config_fail_streak > 0 {
                            escalated_delay(self.config_fail_streak)
                        } else if self.fast_fail_streak >= FAST_FAIL_STREAK {
                            escalated_delay(self.fast_fail_streak)
                        } else {
                            self.policy.next_delay()
                        };
                        self.restart_after = Some(now_ts + delay);
                        tracing::info!(svc = %self.def.name, delay, "退避后重启 (scheduled)");
                    }
                    Some(due) if now_ts < due => {
                        // 未到点, 等后续 tick
                    }
                    Some(_) => {
                        // issue #86: 操作员 start.sh 进行中 → 跳过本轮自动重启。
                        // 端口探活分不清操作员 do_stop 窗口与真宕机; 抢跑会双拉
                        // 进程且 exec 只带 daemon env 快照 (env_n=0), 操作员行内
                        // env 覆盖被静默丢弃。保留 restart_after (下一 tick 重查),
                        // 不记 crash — 操作员动作不是崩溃。脚本退出后探活恢复则
                        // 自愈, 仍宕机则下一轮正常重启。
                        if start_sh_process_alive(&self.def.repo_dir) {
                            if !self.operator_race_notified {
                                self.operator_race_notified = true;
                                tracing::warn!(
                                    svc = %self.def.name,
                                    repo = %self.def.repo_dir,
                                    "操作员 start.sh 进行中, 跳过自动重启 (避免竞态双拉 + \\
                                     行内 env 被 daemon env 快照覆盖; 操作员 restart 期间 \\
                                     行内 env 覆盖仅操作员自己的 exec 生效)"
                                );
                            }
                            return;
                        }
                        self.restart_after = None;
                        let capped = self.policy.record_crash(now_ts);
                        if capped {
                            tracing::error!(svc = %self.def.name, "crash-window 封顶, 标 Failed 停重启");
                            self.state = State::Failed;
                            let _ = alerter
                                .alert(Alert {
                                    service: self.def.name.clone(),
                                    kind: AlertKind::RestartCapHit,
                                    detail: format!("cap {} hit", self.policy.crash_count()),
                                    ts: now_ts,
                                })
                                .await;
                            return;
                        }
                        // issue #47: 快速失败断路 — 上次 start 成功后窗口内又探活不可达。
                        // 连续达阈值 → 熔断告警 (latch, 恢复复位), 后续退避升级 10→60→300。
                        if let Some(t0) = self.last_start_ts
                            && now_ts.saturating_sub(t0) < FAST_FAIL_WINDOW_SEC
                        {
                            self.fast_fail_streak += 1;
                            if self.fast_fail_streak == FAST_FAIL_STREAK && !self.fast_fail_alerted
                            {
                                self.fast_fail_alerted = true;
                                tracing::error!(
                                    svc = %self.def.name,
                                    streak = self.fast_fail_streak,
                                    window_sec = FAST_FAIL_WINDOW_SEC,
                                    "快速失败熔断 (start 后秒级又宕), 退避升级"
                                );
                                let _ = alerter
                                    .alert(Alert {
                                        service: self.def.name.clone(),
                                        kind: AlertKind::FastFailLoop,
                                        detail: format!(
                                            "{} consecutive fast-exits (<{}s) after start",
                                            self.fast_fail_streak, FAST_FAIL_WINDOW_SEC
                                        ),
                                        ts: now_ts,
                                    })
                                    .await;
                            }
                        }
                        match self.invoke_start() {
                            Err(e) => {
                                let detail = e.to_string();
                                // issue #47: exit 78 (EX_CONFIG) = 配置致命, 重启无救 —
                                // 不烧快速重试, 退避走 10→60→300; 连续达 cap 标 Failed。
                                if exit_code_of(&detail) == Some(EXIT_CONFIG) {
                                    self.config_fail_streak += 1;
                                    self.last_start_ts = None;
                                    tracing::error!(
                                        svc = %self.def.name,
                                        streak = self.config_fail_streak,
                                        "config-fatal (exit 78), 重启无救需修配置"
                                    );
                                    let _ = alerter
                                        .alert(Alert {
                                            service: self.def.name.clone(),
                                            kind: AlertKind::ServiceConfigFatal,
                                            detail,
                                            ts: now_ts,
                                        })
                                        .await;
                                    if self.config_fail_streak >= CONFIG_FAIL_CAP {
                                        self.state = State::Failed;
                                        tracing::error!(svc = %self.def.name, "config-fatal 封顶, 标 Failed 停重启");
                                        return;
                                    }
                                    self.restart_after =
                                        Some(now_ts + escalated_delay(self.config_fail_streak));
                                } else {
                                    tracing::warn!(svc = %self.def.name, err = %detail, "start.sh fail");
                                    let _ = alerter
                                        .alert(Alert {
                                            service: self.def.name.clone(),
                                            kind: AlertKind::RestartFailed,
                                            detail,
                                            ts: now_ts,
                                        })
                                        .await;
                                }
                            }
                            Ok(()) => {
                                self.last_start_ts = Some(now_ts);
                                self.state = State::Restarting;
                            }
                        }
                    }
                }
            }
            State::Failed | State::Stopped => {
                // 不动作: Failed 需人工 fusion-sv restart <svc> 复位
            }
            State::Draining => {
                // 不可达: tick() 在 handle_unhealthy 前对 Draining 提前返回。
                // 防御性兜底 (编译器穷尽匹配), 理论上不执行。
                tracing::warn!(
                    svc = %self.def.name,
                    "handle_unhealthy 命中 Draining (不应发生, 已被 tick 拦截)"
                );
            }
        }
    }

    pub(crate) fn invoke_start(&self) -> Result<()> {
        if self.restart_cmd == "restart" {
            (self.start_fn)("restart")
        } else {
            (self.start_fn)("stop")?;
            (self.start_fn)("start")
        }
    }

    // issue #9: 单次探活 (prewarm 用)。复用注入的 probe_fn。
    pub fn probe_once(&self) -> ProbeResult {
        (self.probe_fn)()
    }

    // issue #9: 进入摘流量。grace_sec=宽限期 (到期强制 Stopped)。
    // 仅 Healthy/Starting 可 drain; 其它状态返回 Err (避免误标)。
    pub fn begin_drain(&mut self, grace_sec: u64, now_ts: u64) -> Result<()> {
        if !matches!(
            self.state,
            State::Healthy | State::Starting | State::Unhealthy
        ) {
            anyhow::bail!(
                "drain 需 Healthy/Starting/Unhealthy, 当前 {:?} (svc {})",
                self.state,
                self.def.name
            );
        }
        self.state = State::Draining;
        self.drain_grace_until = Some(now_ts + grace_sec);
        tracing::info!(svc = %self.def.name, grace_sec, "drain 开始 → Draining");
        Ok(())
    }

    // issue #9: 取消 drain (回流量)。仅 Draining 可取消。
    pub fn cancel_drain(&mut self) -> Result<()> {
        if self.state != State::Draining {
            anyhow::bail!(
                "cancel_drain 需 Draining, 当前 {:?} (svc {})",
                self.state,
                self.def.name
            );
        }
        self.state = State::Healthy;
        self.drain_grace_until = None;
        tracing::info!(svc = %self.def.name, "drain 取消 → Healthy");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Layer;
    use crate::probe::{ProbeResult, UnhealthyReason};
    use std::sync::{Arc, Mutex};

    fn def() -> ServiceDef {
        ServiceDef {
            name: "fusion-rag".into(),
            port: 11436,
            repo_dir: "fusion-rag".into(),
            purpose: "kb".into(),
            fixed: false,
            managed: true,
            layer: Layer::App,
            boot_tier: crate::manifest::BootTier::T2,
        }
    }

    // 可变探活结果 + 记录 start.sh 调用序列, 供断言
    #[allow(clippy::type_complexity)]
    fn harness(
        probe_results: ProbeResult,
    ) -> (
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<ProbeResult>>,
        impl Fn() -> ProbeResult + Send + Sync,
        impl Fn(&str) -> Result<()> + Send + Sync,
    ) {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let probe_cell: Arc<Mutex<ProbeResult>> = Arc::new(Mutex::new(probe_results));
        let calls_p = calls.clone();
        let probe_p = probe_cell.clone();
        let probe_fn = move || probe_p.lock().unwrap().clone();
        let start_fn = move |cmd: &str| -> Result<()> {
            calls_p.lock().unwrap().push(cmd.to_string());
            Ok(())
        };
        (calls, probe_cell, probe_fn, start_fn)
    }

    fn store() -> HistoryStore {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        HistoryStore::open(tmp.path()).unwrap()
    }

    #[tokio::test]
    async fn test_healthy_stays_healthy() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(calls.lock().unwrap().is_empty()); // 不调 start.sh
    }

    #[tokio::test]
    async fn test_healthy_to_unhealthy_emits_down_no_restart_yet() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        // 首次宕机: 标 Unhealthy + 告警, 但尚未重启 (重启在下一 tick)
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    #[tokio::test]
    async fn test_stopped_stays_stopped_on_probe_fail() {
        // 手动 down 的服务 (Stopped) 探活失败不得翻回 Unhealthy (否则 tray 变红)
        let (_, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsConnectFailed,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Stopped;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1002, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
    }

    #[tokio::test]
    async fn test_unhealthy_records_crash_and_restarts() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2; // 已标 down, 这次进重启流
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: 退避不再持锁 sleep — 第一 tick 排期, 未到点不重启
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy), "排期 tick 不应重启");
        assert_eq!(r.policy.crash_count(), 0);
        assert!(calls.lock().unwrap().is_empty());
        // 到点 tick (首退避 delay=1s): 真正重启 + 记 1 次 crash
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Restarting));
        assert_eq!(r.policy.crash_count(), 1);
        assert_eq!(*calls.lock().unwrap(), vec!["restart".to_string()]);
    }

    #[tokio::test]
    async fn test_cap_hit_marks_failed_no_restart() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        // 预填 5 次 crash, 这次第 6 次封顶 (R10: policy is_capped 用 > max, max=5 → 第 6 次封顶)
        for i in 0..5 {
            r.policy.record_crash(900 + i);
        }
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: 第一 tick 排期 (5 crash → 退避 30s), 到点 tick 封顶 → Failed 不重启
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy), "排期 tick 不应重启");
        r.tick(1031, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Failed));
        assert!(calls.lock().unwrap().is_empty()); // 封顶不重启
    }

    #[tokio::test]
    async fn test_badpath_no_restart_no_crash() {
        // 防风暴核心: BadPath(404) 不重启, 不记 crash
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadPath(404),
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty()); // 不调 start.sh
        assert_eq!(r.policy.crash_count(), 0); // 不记 crash
        // 再 tick 一次 (state=Unhealthy): BadPath 仍不重启
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    // issue #49: 非重启型探活失败 (BadPath/BadStatus) 的 RestartFailed 告警只在
    // 转移进 Unhealthy 的首次发 — 持续 Unhealthy 期间每 tick (2s) 不得刷屏
    #[tokio::test]
    async fn test_badpath_alert_only_on_transition() {
        let (_, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadStatus(500),
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(0, String::new());
        r.tick(1000, &s, &mut al).await.unwrap(); // 转移进 Unhealthy → 告警 1 次
        assert_eq!(al.alerts_emitted(), 1);
        r.tick(1002, &s, &mut al).await.unwrap(); // 持续 Unhealthy → 静默
        r.tick(1004, &s, &mut al).await.unwrap();
        assert_eq!(al.alerts_emitted(), 1, "持续 Unhealthy 不得重复告警");
    }

    #[tokio::test]
    async fn test_recovery_emits_back() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        // 改探活为健康, tick 应恢复
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 8 };
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_stop_plus_start_when_no_restart_cmd() {
        // restart_cmd="stop+start" (fusion-store 无 restart)
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "stop+start", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: 第一 tick 排期 (delay=1s), 到点 tick 执行 stop+start
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(calls.lock().unwrap().is_empty(), "排期 tick 不应重启");
        r.tick(1001, &s, &mut al).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["stop".to_string(), "start".to_string()]
        );
    }

    // issue #9: drain → Draining → grace 到期后 tick → Stopped, 不重启
    #[tokio::test]
    async fn test_drain_transitions_to_draining_then_stopped() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        // 进入 drain, grace=10s
        r.begin_drain(10, 1000).unwrap();
        assert!(matches!(r.state, State::Draining));
        assert_eq!(r.drain_grace_until, Some(1010));
        // grace 未到期 + 探活健康 → 保持 Draining, 不重启
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1005, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Draining));
        assert!(calls.lock().unwrap().is_empty());
        // grace 到期 → Stopped, 仍不重启
        r.tick(1011, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
        assert!(r.drain_grace_until.is_none());
        assert!(calls.lock().unwrap().is_empty());
    }

    // issue #9: draining 服务探活不可达 → tick 不重启, 转 Stopped
    #[tokio::test]
    async fn test_tick_skips_restart_for_draining() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        r.begin_drain(10, 1000).unwrap();
        // 探活不可达 + grace 未到期 → 仍转 Stopped (in-flight 完成, 服务已停响应)
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
        // 关键: 不调 start.sh (draining 不进重启流)
        assert!(calls.lock().unwrap().is_empty());
    }

    // issue #9: 非 Healthy/Starting/Unhealthy 状态调 begin_drain → Err
    #[tokio::test]
    async fn test_begin_drain_rejects_invalid_state() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Failed;
        assert!(r.begin_drain(10, 1000).is_err());
        assert!(matches!(r.state, State::Failed));
        let _ = calls;
    }

    // issue #9: cancel_drain 仅 Draining 可调, 否则 Err
    #[tokio::test]
    async fn test_cancel_drain_only_when_draining() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        // 非 Draining → Err
        assert!(r.cancel_drain().is_err());
        // 进 drain 再 cancel → Healthy
        r.begin_drain(10, 1000).unwrap();
        assert!(matches!(r.state, State::Draining));
        r.cancel_drain().unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(r.drain_grace_until.is_none());
        let _ = (calls, probe_cell);
    }

    // issue #47: exec 失败信息退出码解析 (约定格式 "exit code=<n>")
    #[test]
    fn test_exit_code_of() {
        assert_eq!(
            exit_code_of("script restart exit code=78: yaml: bad config"),
            Some(78)
        );
        assert_eq!(exit_code_of("daemon cmd start exit code=1: boom"), Some(1));
        assert_eq!(exit_code_of("script restart killed by signal: x"), None);
        assert_eq!(exit_code_of("spawn fail: no such file"), None);
        assert_eq!(exit_code_of("exit code=78"), Some(78));
        assert_eq!(exit_code_of("exit code=-1: segv"), Some(-1));
    }

    // issue #47: config-fatal (exit 78) 退避升级 10→60→300, 不走 1→2→4 快速序列
    #[tokio::test]
    async fn test_config_fatal_exit_78_escalates_backoff() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> {
            Err(anyhow::anyhow!(
                "script restart exit code=78: yaml: bad config"
            ))
        };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // 排期 tick (streak=0 → 走普通退避 1s)
        r.tick(1000, &s, &mut al).await.unwrap();
        assert_eq!(r.restart_after, Some(1001));
        // 到点: start 失败 exit 78 → streak=1, 退避 10s
        r.tick(1001, &s, &mut al).await.unwrap();
        assert_eq!(r.config_fail_streak, 1);
        assert_eq!(r.restart_after, Some(1011));
        // 到点: streak=2, 退避 60s
        r.tick(1011, &s, &mut al).await.unwrap();
        assert_eq!(r.config_fail_streak, 2);
        assert_eq!(r.restart_after, Some(1071));
        // 到点: streak=3, 退避 300s (封顶)
        r.tick(1071, &s, &mut al).await.unwrap();
        assert_eq!(r.config_fail_streak, 3);
        assert_eq!(r.restart_after, Some(1371));
        // 状态仍 Unhealthy (未封顶不标 Failed)
        assert!(matches!(r.state, State::Unhealthy));
    }

    // issue #47: 连续 5 次 exit 78 → 标 Failed 停重启
    #[tokio::test]
    async fn test_config_fatal_cap_marks_failed() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> {
            Err(anyhow::anyhow!(
                "script restart exit code=78: yaml: bad config"
            ))
        };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        let mut now = 1000u64;
        // 排期 tick, 之后循环 "到点 → 失败 streak+1 → 排期 escalated" 直到 Failed
        r.tick(now, &s, &mut al).await.unwrap(); // 排期 1s
        now += 1;
        loop {
            r.tick(now, &s, &mut al).await.unwrap(); // 失败 → streak+1
            if matches!(r.state, State::Failed) {
                break;
            }
            now = r.restart_after.unwrap(); // 跳到退避截止
        }
        assert_eq!(r.config_fail_streak, 5);
        assert!(
            matches!(r.state, State::Failed),
            "连续 5 次 exit 78 应标 Failed"
        );
    }

    // issue #47: 快速失败断路 — start 成功后窗口内又宕, 连续 3 次熔断 + 退避升级
    #[tokio::test]
    async fn test_fast_fail_circuit_breaker() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> { Ok(()) };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // 排期 1s → 到点 start 成功 (last_start_ts=1001)
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1001, &s, &mut al).await.unwrap();
        assert_eq!(r.last_start_ts, Some(1001));
        assert_eq!(r.fast_fail_streak, 0);
        // 轮 1: 排期 (退避 2s) → 到点 1004, 距上次 start 3s < 10s → streak=1
        r.tick(1002, &s, &mut al).await.unwrap();
        r.tick(1004, &s, &mut al).await.unwrap();
        assert_eq!(r.fast_fail_streak, 1);
        // 轮 2: 退避 4s → 到点 1009, 距上次 start 5s → streak=2
        r.tick(1005, &s, &mut al).await.unwrap();
        r.tick(1009, &s, &mut al).await.unwrap();
        assert_eq!(r.fast_fail_streak, 2);
        // 轮 3: 退避 8s → 到点 1018, 距上次 start 9s → streak=3 熔断 latch
        r.tick(1010, &s, &mut al).await.unwrap();
        r.tick(1018, &s, &mut al).await.unwrap();
        assert_eq!(r.fast_fail_streak, 3);
        assert!(r.fast_fail_alerted, "达阈值应置熔断 latch");
        // 下一轮排期退避升级: escalated_delay(3)=300s
        r.tick(1019, &s, &mut al).await.unwrap();
        assert_eq!(r.restart_after, Some(1319), "熔断后退避应走 300s 封顶");
    }

    // issue #47: 恢复 Healthy 复位熔断计数
    #[tokio::test]
    async fn test_recovery_resets_circuit_breakers() {
        let (_, probe_cell, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: 预置连续失败计数, 模拟已失败的中间态 (直接从 Unhealthy 起测)
        r.fail_streak = 2;
        r.fast_fail_streak = 3;
        r.fast_fail_alerted = true;
        r.config_fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert_eq!(r.fast_fail_streak, 0);
        assert!(!r.fast_fail_alerted);
        assert_eq!(r.config_fail_streak, 0);
        assert_eq!(r.fail_streak, 0, "恢复应复位连续失败计数");
        let _ = probe_cell;
    }

    // issue #79: 连续失败未达阈值 (3) 不排重启 — 繁忙服务探活抖动不升级
    #[tokio::test]
    async fn test_issue79_streak_gate_no_restart_below_threshold() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // tick 1: streak=1 → Unhealthy + ServiceDown, 不排期
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        assert!(r.restart_after.is_none(), "streak=1 不得排期");
        assert_eq!(al.alerts_emitted(), 1);
        // tick 2: streak=2 → 仍不排期
        r.tick(1002, &s, &mut al).await.unwrap();
        assert!(r.restart_after.is_none(), "streak=2 不得排期");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0, "未重启不得记 crash");
        // tick 3: streak=3 达阈值 → 排期 (首退避 1s)
        r.tick(1004, &s, &mut al).await.unwrap();
        assert_eq!(r.restart_after, Some(1005), "3 连败应排期重启");
        assert!(calls.lock().unwrap().is_empty(), "排期 tick 不应重启");
    }

    // issue #86: 操作员 start.sh 进程检测 — 真实 pgrep 路径
    #[test]
    fn test_start_sh_process_alive_detection() {
        // 无进程 → false
        assert!(
            !start_sh_process_alive("no-such-repo-xyz-987"),
            "无操作员脚本应不检出"
        );
        // 模拟操作员脚本: bash 进程 cmdline 含 "<repo>/start.sh"
        // (bash -c 的 $0 参数进 cmdline, pgrep -f 可匹配)
        // 复合命令防 bash exec 优化 (单命令会 exec 成 sleep, argv 丢失)
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3; true")
            .arg("race-probe-repo-xyz/start.sh")
            .spawn()
            .unwrap();
        let mut seen = false;
        for _ in 0..40 {
            if start_sh_process_alive("race-probe-repo-xyz") {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen, "操作员脚本进程存活应被检出");
        // 进程退出后不再检出 (pgrep 有毫秒级延迟, 轮询等待清零)
        let mut cleared = false;
        for _ in 0..60 {
            if !start_sh_process_alive("race-probe-repo-xyz") {
                cleared = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(cleared, "脚本退出后应不再检出");
    }

    // issue #86 生产补: 操作员 cd 进 repo 目录用 ./start.sh (cmdline 无 repo
    // 前缀) 也要检出 — 阶段 2 cwd 鉴别路径
    #[test]
    fn test_start_sh_alive_relative_cwd_invocation() {
        struct EnvGuard {
            key: &'static str,
            prev: Option<String>,
        }
        impl EnvGuard {
            fn set(key: &'static str, val: &str) -> Self {
                let prev = std::env::var(key).ok();
                // SAFETY: 仅本测试读写此 key; 恢复在 Drop, panic 不泄漏。
                unsafe { std::env::set_var(key, val) };
                Self { key, prev }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: 同上。
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var(self.key, v) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }
        let base = std::env::temp_dir().join("race-cwd-repo-base-xyz");
        let repo = base.join("fake-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let _guard = EnvGuard::set("FUSION_ROOT", base.to_str().unwrap());
        // 模拟操作员在 repo 目录用 ./start.sh — cmdline 无 repo 前缀,
        // 复合命令防 bash exec 优化丢 argv
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3; true")
            .arg("./start.sh")
            .current_dir(&repo)
            .spawn()
            .unwrap();
        let mut seen = false;
        for _ in 0..60 {
            if start_sh_process_alive("fake-repo") {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        // 清理临时目录 (只留测试产物外的最终输出件)
        let _ = std::fs::remove_dir_all(&base);
        assert!(seen, "cwd 相对调用 ./start.sh 应被阶段 2 检出");
    }

    // issue #86: 到点重启时操作员 start.sh 在跑 → 跳过本轮 (不 invoke_start,
    // 不记 crash, restart_after 保留); 脚本退出后下一轮正常重启。
    #[tokio::test]
    async fn test_operator_start_sh_running_skips_auto_restart() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut d = def();
        d.repo_dir = "race-flow-repo-xyz".into();
        let mut r = ServiceRunner::new(d, "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 2; true")
            .arg("race-flow-repo-xyz/start.sh")
            .spawn()
            .unwrap();
        // 等 pgrep 可见 (bash spawn 毫秒级)
        for _ in 0..40 {
            if start_sh_process_alive("race-flow-repo-xyz") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // 预置 streak: tick1 (Healthy→Unhealthy 记宕机) → tick2 (streak 达阈值排期,
        // 首退避 1s, due=1002) → tick3 到点, 应被操作员脚本闸门拦下
        r.fail_streak = 2;
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        r.tick(1001, &s, &mut al).await.unwrap();
        assert_eq!(r.restart_after, Some(1002), "3 连败应排期重启");
        r.tick(1002, &s, &mut al).await.unwrap();
        // 关键: 未 invoke_start, 未记 crash, 排期保留 (下 tick 重查)
        assert!(
            calls.lock().unwrap().is_empty(),
            "操作员脚本在跑不得抢跑自动重启"
        );
        assert_eq!(
            r.policy.crash_count(),
            0,
            "操作员动作不是崩溃, 不得记 crash"
        );
        assert!(r.restart_after.is_some(), "排期应保留, 下 tick 重查");
        assert!(r.operator_race_notified, "首次跳过应置告警 latch");
        // 等操作员脚本退出 (sleep 2 到期) → 下一轮排期到点正常重启
        let _ = child.wait();
        let mut gone = false;
        for _ in 0..60 {
            if !start_sh_process_alive("race-flow-repo-xyz") {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone, "操作员脚本应已退出");
        r.tick(1002, &s, &mut al).await.unwrap();
        r.tick(1003, &s, &mut al).await.unwrap();
        assert!(
            !calls.lock().unwrap().is_empty(),
            "脚本退出后应恢复正常自动重启"
        );
    }

    // issue #79: 抖动 (失败-失败-恢复-失败-失败) 中间夹成功探活即复位, 永不升级
    #[tokio::test]
    async fn test_issue79_flap_recovery_resets_streak() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // 两次失败 (streak 1→2)
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1002, &s, &mut al).await.unwrap();
        assert_eq!(r.fail_streak, 2);
        // 恢复 — 复位计数 + 清陈旧排期
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 8 };
        r.tick(1004, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert_eq!(r.fail_streak, 0);
        // 再两次失败 — streak 从 0 重新数, 不排期
        *probe_cell.lock().unwrap() = ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        };
        r.tick(1006, &s, &mut al).await.unwrap();
        r.tick(1008, &s, &mut al).await.unwrap();
        assert_eq!(r.fail_streak, 2);
        assert!(r.restart_after.is_none(), "抖动不得升级排期");
        assert!(calls.lock().unwrap().is_empty(), "抖动不得重启");
        assert_eq!(r.policy.crash_count(), 0);
    }
}
