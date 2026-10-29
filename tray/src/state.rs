// PRD §4.2 数据模型 (纯逻辑, 无 GUI 依赖 — 单测覆盖)。
// Svc 一行 + Model 聚合: 悬浮点色 / 扇区聚合 / 排序 / 过滤 / 轮询间隔。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Core,
    Platform,
    App,
}

impl Tier {
    pub fn parse(s: &str) -> Tier {
        match s {
            "T0" => Tier::Core,
            "T1" => Tier::Platform,
            _ => Tier::App,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tier::Core => "CORE · 核心",
            Tier::Platform => "PLATFORM · 常用",
            Tier::App => "APP · 按需",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcState {
    Healthy,
    Starting,
    Unhealthy,
    Stopped,
    Failed,
}

impl SvcState {
    pub fn parse(s: &str) -> SvcState {
        match s {
            "Healthy" => SvcState::Healthy,
            "Starting" => SvcState::Starting,
            "Unhealthy" | "Restarting" | "Draining" => SvcState::Unhealthy,
            "Failed" => SvcState::Failed,
            _ => SvcState::Stopped,
        }
    }

    pub fn is_running(self) -> bool {
        matches!(
            self,
            SvcState::Healthy | SvcState::Starting | SvcState::Unhealthy
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotColor {
    Green,
    Yellow,
    Red,
    Gray,
}

#[derive(Debug, Clone)]
pub struct Svc {
    pub name: String,
    pub port: u16,
    pub tier: Tier,
    pub state: SvcState,
    // pin 状态: Some("auto")/Some("manual") = 有个人覆盖; None = 走注册表。
    // boot_auto = 会被开机自启 (pin=auto, 或无覆盖且注册表 T0)。
    pub pinned: Option<String>,
    pub boot_auto: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub services: Vec<Svc>,
    pub reachable: bool,
}

impl Model {
    pub fn dot(&self) -> DotColor {
        if !self.reachable {
            return DotColor::Gray;
        }
        let t0: Vec<_> = self
            .services
            .iter()
            .filter(|s| s.tier == Tier::Core)
            .collect();
        if t0.is_empty() {
            return DotColor::Gray;
        }
        let down = t0
            .iter()
            .any(|s| matches!(s.state, SvcState::Failed | SvcState::Stopped));
        let degraded = t0.iter().any(|s| s.state != SvcState::Healthy);
        if down {
            DotColor::Red
        } else if degraded {
            DotColor::Yellow
        } else {
            DotColor::Green
        }
    }

    pub fn tier_svcs(&self, t: Tier) -> Vec<&Svc> {
        self.services.iter().filter(|s| s.tier == t).collect()
    }

    // PRD §4.2: T2 列表默认「运行中在前 + 注册表序」(稳定排序)。
    pub fn sorted(tier_svcs: Vec<&Svc>) -> Vec<&Svc> {
        let mut v = tier_svcs;
        v.sort_by_key(|s| !s.state.is_running());
        v
    }

    pub fn filtered<'a>(tier_svcs: Vec<&'a Svc>, q: &str) -> Vec<&'a Svc> {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return tier_svcs;
        }
        tier_svcs
            .into_iter()
            .filter(|s| s.name.to_lowercase().contains(&q))
            .collect()
    }

    // 乐观更新: 本地点击立即改状态, 探活回填纠错 (poll 覆盖全量)。
    pub fn set_state(&mut self, name: &str, state: SvcState) {
        for s in &mut self.services {
            if s.name == name {
                s.state = state;
            }
        }
    }

    pub fn set_pin(&mut self, name: &str, pin: Option<String>) {
        for s in &mut self.services {
            if s.name == name {
                s.pinned = pin.clone();
                s.boot_auto = match &pin {
                    Some(p) => p == "auto",
                    None => s.tier == Tier::Core,
                };
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SectorAgg {
    pub running: usize,
    pub total: usize,
    // 行内点阵: 绿=运行健康 / 黄=中间态 / 红=异常 / 灰=停止
    pub dots: Vec<DotColor>,
    pub all_green: bool,
    pub any_red: bool,
}

pub fn sector_color(a: &SectorAgg) -> DotColor {
    if a.any_red {
        DotColor::Red
    } else if a.all_green {
        DotColor::Green
    } else if a.running > 0 {
        DotColor::Yellow
    } else {
        DotColor::Gray
    }
}

pub fn sector(svcs: &[&Svc]) -> SectorAgg {
    let mut dots = Vec::with_capacity(svcs.len());
    let mut running = 0;
    let mut all_green = !svcs.is_empty();
    let mut any_red = false;
    for s in svcs {
        let c = match s.state {
            SvcState::Healthy => {
                running += 1;
                DotColor::Green
            }
            SvcState::Starting => {
                running += 1;
                all_green = false;
                DotColor::Yellow
            }
            SvcState::Unhealthy | SvcState::Failed => {
                running += s.state.is_running() as usize;
                all_green = false;
                any_red = true;
                DotColor::Red
            }
            SvcState::Stopped => {
                all_green = false;
                DotColor::Gray
            }
        };
        dots.push(c);
    }
    SectorAgg {
        running,
        total: svcs.len(),
        dots,
        all_green,
        any_red,
    }
}

// PRD §4.2 轮询节奏: L2/L3 打开 5s / 回 dot 30s / daemon 不可达 60s。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiState {
    Dot,
    Fan,
    List(Tier),
}

pub fn poll_interval_secs(ui: UiState, reachable: bool) -> u64 {
    if !reachable {
        60
    } else {
        match ui {
            UiState::Dot => 30,
            UiState::Fan | UiState::List(_) => 5,
        }
    }
}

// /api/v1/boot 响应行 (supervisor BootEntry)。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BootRow {
    pub name: String,
    pub tier: String,
    pub pinned: Option<String>,
    pub state: String,
    pub port: u16,
}

impl BootRow {
    pub fn into_svc(self) -> Svc {
        let tier = Tier::parse(&self.tier);
        let state = SvcState::parse(&self.state);
        // boot_auto 与 supervisor boot_auto 谓词一致: pin 优先, 否则 T0。
        let boot_auto = match &self.pinned {
            Some(p) => p == "auto",
            None => tier == Tier::Core,
        };
        Svc {
            name: self.name,
            port: self.port,
            tier,
            state,
            pinned: self.pinned,
            boot_auto,
        }
    }
}

pub fn model_from_boot(rows: Vec<BootRow>) -> Model {
    Model {
        services: rows.into_iter().map(|r| r.into_svc()).collect(),
        reachable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, tier: Tier, state: SvcState, pinned: Option<&str>) -> Svc {
        Svc {
            name: name.into(),
            port: 11434,
            tier,
            state,
            pinned: pinned.map(String::from),
            boot_auto: match pinned {
                Some("auto") => true,
                Some(_) => false,
                None => tier == Tier::Core,
            },
        }
    }

    fn rows(list: &[(&str, Tier, SvcState, Option<&str>)]) -> Vec<Svc> {
        list.iter().map(|(n, t, s, p)| svc(n, *t, *s, *p)).collect()
    }

    #[test]
    fn test_dot_colors() {
        let m = Model {
            reachable: false,
            services: rows(&[("mlx", Tier::Core, SvcState::Healthy, None)]),
        };
        assert_eq!(m.dot(), DotColor::Gray);

        let m = Model {
            reachable: true,
            services: rows(&[
                ("mlx", Tier::Core, SvcState::Healthy, None),
                ("gw", Tier::Core, SvcState::Healthy, None),
            ]),
        };
        assert_eq!(m.dot(), DotColor::Green);

        let m = Model {
            reachable: true,
            services: rows(&[
                ("mlx", Tier::Core, SvcState::Healthy, None),
                ("id", Tier::Core, SvcState::Starting, None),
            ]),
        };
        assert_eq!(m.dot(), DotColor::Yellow);

        // T2 宕不影响点色 — 只看 T0
        let m = Model {
            reachable: true,
            services: rows(&[
                ("mlx", Tier::Core, SvcState::Healthy, None),
                ("biz", Tier::App, SvcState::Failed, None),
            ]),
        };
        assert_eq!(m.dot(), DotColor::Green);

        let m = Model {
            reachable: true,
            services: rows(&[("mlx", Tier::Core, SvcState::Stopped, None)]),
        };
        assert_eq!(m.dot(), DotColor::Red);
    }

    #[test]
    fn test_sector_agg_and_color() {
        let svcs = rows(&[
            ("a", Tier::Core, SvcState::Healthy, None),
            ("b", Tier::Core, SvcState::Healthy, None),
        ]);
        let agg = sector(&svcs.iter().collect::<Vec<_>>());
        assert_eq!(agg.running, 2);
        assert!(agg.all_green);
        assert_eq!(sector_color(&agg), DotColor::Green);

        let svcs = rows(&[
            ("a", Tier::App, SvcState::Healthy, None),
            ("b", Tier::App, SvcState::Stopped, None),
        ]);
        let agg = sector(&svcs.iter().collect::<Vec<_>>());
        assert_eq!(sector_color(&agg), DotColor::Yellow);

        let svcs = rows(&[("a", Tier::App, SvcState::Failed, None)]);
        let agg = sector(&svcs.iter().collect::<Vec<_>>());
        assert_eq!(sector_color(&agg), DotColor::Red);
    }

    #[test]
    fn test_sorted_running_first_registry_order_stable() {
        let svcs = rows(&[
            ("stopped-1", Tier::App, SvcState::Stopped, None),
            ("run-1", Tier::App, SvcState::Healthy, None),
            ("run-2", Tier::App, SvcState::Healthy, None),
            ("stopped-2", Tier::App, SvcState::Stopped, None),
        ]);
        let refs = Model::sorted(svcs.iter().collect());
        let names: Vec<_> = refs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["run-1", "run-2", "stopped-1", "stopped-2"]);
    }

    #[test]
    fn test_filtered_search_case_insensitive() {
        let svcs = rows(&[
            ("fusion-mlx", Tier::Core, SvcState::Healthy, None),
            ("fusion-gateway", Tier::Core, SvcState::Healthy, None),
        ]);
        let refs = Model::filtered(svcs.iter().collect(), "MLX");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "fusion-mlx");
        assert_eq!(Model::filtered(svcs.iter().collect(), "  ").len(), 2);
    }

    #[test]
    fn test_optimistic_pin_and_state() {
        let mut m = Model {
            reachable: true,
            services: rows(&[("biz", Tier::App, SvcState::Stopped, None)]),
        };
        assert!(!m.services[0].boot_auto);
        m.set_pin("biz", Some("auto".into()));
        assert!(m.services[0].boot_auto);
        m.set_pin("biz", Some("manual".into()));
        assert!(!m.services[0].boot_auto);
        m.set_pin("biz", None);
        assert!(!m.services[0].boot_auto); // T2 回退注册表 = 不自启

        let mut m = Model {
            reachable: true,
            services: rows(&[("mlx", Tier::Core, SvcState::Healthy, None)]),
        };
        assert!(m.services[0].boot_auto);
        m.set_pin("mlx", Some("manual".into()));
        assert!(!m.services[0].boot_auto);

        m.set_state("mlx", SvcState::Unhealthy);
        assert_eq!(m.services[0].state, SvcState::Unhealthy);
    }

    #[test]
    fn test_poll_interval_policy() {
        assert_eq!(poll_interval_secs(UiState::Dot, true), 30);
        assert_eq!(poll_interval_secs(UiState::Fan, true), 5);
        assert_eq!(poll_interval_secs(UiState::List(Tier::App), true), 5);
        assert_eq!(poll_interval_secs(UiState::List(Tier::Core), false), 60);
        assert_eq!(poll_interval_secs(UiState::Dot, false), 60);
    }

    #[test]
    fn test_boot_row_parse_and_tier() {
        let row: BootRow = serde_json::from_str(
            r#"{"name":"fusion-mlx","tier":"T0","pinned":null,"state":"Healthy","port":11434}"#,
        )
        .unwrap();
        let s = row.into_svc();
        assert_eq!(s.tier, Tier::Core);
        assert_eq!(s.state, SvcState::Healthy);
        assert!(s.boot_auto);

        let row: BootRow = serde_json::from_str(
            r#"{"name":"x","tier":"T2","pinned":"auto","state":"Stopped","port":11500}"#,
        )
        .unwrap();
        let s = row.into_svc();
        assert_eq!(s.tier, Tier::App);
        assert!(s.boot_auto);
    }
}
