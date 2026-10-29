// PRD app-tiering (issue #62) P2+P3: 桌面悬浮点 tray。
// L1 悬浮点 (屏幕右缘垂直居中, 健康点色 + 异常呼吸) → L2 半圆扇形 (3 扇区) → L3 分类列表 (搜索/开关/分组/pin)。
// 数据面: 后台线程 ureq 轮询 GET /api/v1/boot (开面板 5s / dot 30s / 不可达 60s);
// 写面: UI 线程入队 → worker 线程 ureq POST (120s 超时), 乐观更新 + 探活回填纠错。
// 不可达: 点变灰 + 面板显示「daemon 未运行」+ 自愈 (spawn `fusion-sv up`)。

mod api;
mod state;
mod theme;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use eframe::egui::{Align2, Color32, FontId, Pos2, Stroke, Vec2};

use state::{DotColor, Model, SvcState, Tier, UiState, sector_color};
use theme::{GRAY, GREEN, RED, ROW_HEIGHT, YELLOW, palette};

const DOT_WIN: f32 = 44.0;
const DOT_R: f32 = 7.0;
const FAN_WIN: f32 = 340.0;
const LIST_W: f32 = 300.0;
const LIST_H: f32 = 400.0;
const FAN_R: f32 = 160.0;

pub struct Shared {
    pub model: Mutex<Model>,
    ui_state: Mutex<UiState>,
    pub queue: Mutex<VecDeque<api::Cmd>>,
    pub base: String,
    pub token: String,
}

impl Shared {
    fn ui_state(&self) -> UiState {
        *self.ui_state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn set_ui_state(&self, s: UiState) {
        *self.ui_state.lock().unwrap_or_else(|p| p.into_inner()) = s;
    }
}

fn dot_color_of(c: DotColor) -> Color32 {
    match c {
        DotColor::Green => GREEN,
        DotColor::Yellow => YELLOW,
        DotColor::Red => RED,
        DotColor::Gray => GRAY,
    }
}

fn main() -> eframe::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let base = std::env::var("FUSION_SV_BASE").unwrap_or_else(|_| api::DEFAULT_BASE.into());
    let token = std::env::var("FUSION_SV_TOKEN").unwrap_or_default();
    tracing::info!(%base, "fusion-tray starting");

    let theme_name = theme::detect_theme().to_string();
    let shared = Arc::new(Shared {
        model: Mutex::new(Model::default()),
        ui_state: Mutex::new(UiState::Dot),
        queue: Mutex::new(VecDeque::new()),
        base,
        token,
    });

    // 后台轮询线程: 间隔随 UI 态切换, 失败 reachable=false 点变灰。
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("poller".into())
            .spawn(move || poller_loop(shared))
            .expect("spawn poller");
    }
    // 写操作 worker: 队列串行执行 (ureq 同步), UI 线程零阻塞。
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("worker".into())
            .spawn(move || worker_loop(shared))
            .expect("spawn worker");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_active(false)
            .with_resizable(false),
        ..Default::default()
    };
    let app = TrayApp {
        shared,
        ui_state: UiState::Dot,
        search: String::new(),
        pending_confirm: None,
        theme: theme_name,
        last_apply: None,
    };
    eframe::run_native(
        "fusion-tray",
        options,
        Box::new(|cc| {
            load_cjk_font(&cc.egui_ctx);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

// CJK 字体: egui 默认字体无中文字形。构建期 pyftsubset 子集化 Hiragino Sans GB
// (tray/assets/cjk-subset.ttf ~120KB, 覆盖 src 全部非 ASCII 字符 — 回归测试
// test_subset_font_covers_source_charset 守护漂移; 再生成脚本 tray/gen_font_subset.sh)。
// 全量系统字体 22.4MB 会驻留 3 份 malloc 拷贝 (from_owned Vec + ab_glyph 解析副本 +
// glyph 数据) — 常驻内存硬约束下必须子集化。缺字形 (◀◐📌📍🔍) 走 egui 默认字体兜底。
const CJK_SUBSET: &[u8] = include_bytes!("../assets/cjk-subset.ttf");

fn load_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), egui::FontData::from_static(CJK_SUBSET));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
    tracing::info!(bytes = CJK_SUBSET.len(), "CJK subset font loaded");
}

fn poller_loop(shared: Arc<Shared>) {
    let agent = api::new_agent();
    let mut last_poll = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        // 单轮 panic 隔离: catch_unwind 包裹, 崩了记 ERROR 下一轮续跑 —
        // 轮询线程死掉 = 点永久停格, 常驻组件不可接受。
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_one(&shared, &agent, &mut last_poll)
        }));
        if let Err(p) = res {
            tracing::error!(panic = ?p, "poller 单轮 panic (自愈续跑)");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn poll_one(shared: &Shared, agent: &ureq::Agent, last_poll: &mut std::time::Instant) {
    let (ui, reachable) = {
        let m = shared.model.lock().unwrap_or_else(|p| p.into_inner());
        (shared.ui_state(), m.reachable)
    };
    let interval = state::poll_interval_secs(ui, reachable);
    if last_poll.elapsed() >= Duration::from_secs(interval) {
        match api::fetch_boot(agent, &shared.base) {
            Ok(m) => {
                *shared.model.lock().unwrap_or_else(|p| p.into_inner()) = m;
            }
            Err(e) => {
                // /boot 超时 ≠ daemon 挂: 批量组操作占住核心锁数秒→数十秒,
                // /health handler 不锁核心仍应答。/health 通 = 忙, 保持最后模型不置灰。
                match api::fetch_health(agent, &shared.base) {
                    Ok(()) => {
                        tracing::info!(err = %e, "boot poll timeout 但 /health 可达 (daemon 忙, 保持现状)");
                    }
                    Err(he) => {
                        tracing::warn!(err = %e, health_err = %he, interval, "boot + health 均失败 (点变灰)");
                        shared
                            .model
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .reachable = false;
                    }
                }
            }
        }
        *last_poll = std::time::Instant::now();
    }
}

fn worker_loop(shared: Arc<Shared>) {
    let agent = api::new_agent();
    loop {
        // 单轮 panic 隔离: 同 poller — 崩了记 ERROR 续跑。
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker_one(&shared, &agent)));
        if let Err(p) = res {
            tracing::error!(panic = ?p, "worker 单轮 panic (自愈续跑)");
        }
        if shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
        {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn worker_one(shared: &Shared, agent: &ureq::Agent) {
    let batch: Vec<api::Cmd> = shared
        .queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(..)
        .collect();
    for cmd in &batch {
        let (url, body) = api::cmd_request(&shared.base, cmd);
        match api::post_cmd(agent, &url, body, &shared.token) {
            Ok(_) => tracing::info!(?cmd, "cmd ok"),
            Err(e) => tracing::error!(?cmd, err = %e, "cmd failed (探活回填纠错)"),
        }
    }
}

struct Confirm {
    text: String,
    cmds: Vec<api::Cmd>,
}

struct TrayApp {
    shared: Arc<Shared>,
    ui_state: UiState,
    search: String,
    pending_confirm: Option<Confirm>,
    theme: String,
    last_apply: Option<UiState>,
}

impl TrayApp {
    // 窗口几何: dot 屏幕右缘垂直居中; fan 窗口右缘贴 dot 向左展开 (竖放半圆);
    // list 在 dot 左侧垂直居中。
    fn apply_viewport(&mut self, ctx: &egui::Context) {
        if self.last_apply == Some(self.ui_state) {
            return;
        }
        let monitor = ctx.input(|i| {
            i.viewport()
                .monitor_size
                .unwrap_or(Vec2::new(1440.0, 900.0))
        });
        let dx = monitor.x - 24.0; // dot 中心 x (右缘内缩 24px)
        let dy = monitor.y / 2.0; // dot 中心 y
        let (size, pos) = match self.ui_state {
            UiState::Dot => (
                Vec2::new(DOT_WIN, DOT_WIN),
                Pos2::new(dx - DOT_WIN / 2.0, dy - DOT_WIN / 2.0),
            ),
            UiState::Fan => (
                Vec2::new(FAN_WIN, FAN_WIN),
                Pos2::new(dx - FAN_WIN, dy - FAN_WIN / 2.0), // 右缘贴 dot, 垂直居中
            ),
            UiState::List(_) => (
                Vec2::new(LIST_W, LIST_H),
                Pos2::new(dx - LIST_W - 12.0, dy - LIST_H / 2.0),
            ),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        self.last_apply = Some(self.ui_state);
    }

    fn enqueue(&self, cmd: api::Cmd) {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(cmd);
    }

    // 破坏性操作确认 (PRD §4.2): CORE 停止 / 全部停止 / 一键重启 / drain / rollout。
    fn destructive(&mut self, text: impl Into<String>, cmds: Vec<api::Cmd>) {
        self.pending_confirm = Some(Confirm {
            text: text.into(),
            cmds,
        });
    }

    // 扇区序 (竖放, 左半圆自顶向下): 顶=CORE, 中=PLATFORM, 底=APP。
    fn sector_tier(i: usize) -> Tier {
        match i {
            0 => Tier::Core,
            1 => Tier::Platform,
            _ => Tier::App,
        }
    }

    fn ui_unreachable(&mut self, ctx: &egui::Context) {
        let pal = palette(&self.theme);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(pal.bg)
                    .rounding(egui::Rounding::same(theme::CORNER_RADIUS))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.heading("daemon 未运行");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("无法连接 {}", self.shared.base))
                            .size(11.0)
                            .weak(),
                    );
                    ui.add_space(8.0);
                    if ui.button("一键拉起 (fusion-sv up)").clicked() {
                        self.self_heal();
                    }
                    if ui.button("关闭 (回 dot)").clicked() {
                        self.ui_state = UiState::Dot;
                    }
                });
            });
    }

    // 自愈 (P3): spawn `fusion-sv up` — daemon 恢复后 boot-set 拉起 T0 ∪ pin=auto。
    // 默认路径解析: FUSION_SV_BIN > 同目录 sibling (LaunchAgent/nohup 场景) > PATH。
    // (相对名 "fusion-sv" 依赖 PATH 含 target/release — 生产日志验证过会 spawn fail)
    fn self_heal(&mut self) {
        let bin = std::env::var("FUSION_SV_BIN").unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("fusion-sv")))
                .filter(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "fusion-sv".into())
        });
        match std::process::Command::new(&bin).arg("up").spawn() {
            Ok(_) => tracing::info!(%bin, "self-heal: fusion-sv up spawned"),
            Err(e) => tracing::error!(%bin, err = %e, "self-heal spawn failed"),
        }
    }
}

impl eframe::App for TrayApp {
    // dot 态窗口背景必须全透明 — eframe 默认 clear rgba(12,12,12,180) 即用户看到的
    // "灰色正方形底" (with_transparent 只开窗口能力, 清屏色仍不透明)。fan/list/
    // unreachable 态由各自 CentralPanel paint pal.bg 不透明底, 不受影响。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::apply_style(ctx, palette(&self.theme));
        self.apply_viewport(ctx);
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if esc {
            if self.pending_confirm.is_some() {
                self.pending_confirm = None;
            } else {
                // ESC 逐级回退: List → Fan → Dot。
                self.ui_state = match self.ui_state {
                    UiState::List(_) => UiState::Fan,
                    _ => UiState::Dot,
                };
            }
        }
        self.shared.set_ui_state(self.ui_state);

        // 确认气泡 (模态覆盖层, 吞掉本帧其余 UI)。
        if self.pending_confirm.is_some() {
            let mut decision: Option<bool> = None;
            egui::Area::new(egui::Id::new("confirm"))
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(220.0);
                        if let Some(c) = &self.pending_confirm {
                            ui.label(&c.text);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("确认").clicked() {
                                decision = Some(true);
                            }
                            if ui.button("取消").clicked() {
                                decision = Some(false);
                            }
                        });
                    });
                });
            match decision {
                Some(true) => {
                    if let Some(c) = self.pending_confirm.take() {
                        for cmd in c.cmds {
                            self.enqueue(cmd);
                        }
                    }
                }
                Some(false) => self.pending_confirm = None,
                None => {}
            }
            ctx.request_repaint_after(Duration::from_millis(150));
            return;
        }

        match self.ui_state {
            UiState::Dot => self.ui_dot(ctx),
            UiState::Fan => self.ui_fan(ctx),
            UiState::List(t) => self.ui_list(ctx, t),
        }
        // 呼吸动画 + hover 检测需要持续重绘。
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

// ---------- L1 悬浮点 ----------

impl TrayApp {
    fn ui_dot(&mut self, ctx: &egui::Context) {
        let model = self
            .shared
            .model
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let c = model.dot();
        let base = dot_color_of(c);
        // 呼吸仅异常态 (PRD §4.2: 2s 呼吸, 只在不健康时)。
        let breathe = matches!(c, DotColor::Red | DotColor::Yellow);
        let (r, alpha) = if breathe {
            let t = ctx.input(|i| i.time);
            let k = 0.5 + 0.5 * ((t * std::f64::consts::PI).sin() as f32);
            (DOT_R + 2.0 * k, 0.6 + 0.4 * k)
        } else {
            (DOT_R, 1.0)
        };
        let col =
            Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), (alpha * 255.0) as u8);
        egui::Area::new(egui::Id::new("dot"))
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::splat(DOT_WIN), egui::Sense::click());
                let painter = ui.painter_at(rect);
                painter.circle_filled(rect.center(), r, col);
                painter.circle_stroke(
                    rect.center(),
                    r + 2.0,
                    Stroke::new(1.5_f32, col.gamma_multiply(0.35)),
                );
                if resp.clicked() {
                    self.ui_state = UiState::Fan;
                }
                resp.on_hover_text("fusion 状态悬浮点 (点击展开)");
            });
    }
}

// ---------- L2 扇形 ----------

impl TrayApp {
    fn ui_fan(&mut self, ctx: &egui::Context) {
        let model = self
            .shared
            .model
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if !model.reachable {
            self.ui_unreachable(ctx);
            return;
        }
        let pal = palette(&self.theme);
        egui::Area::new(egui::Id::new("fan"))
            .anchor(Align2::LEFT_TOP, [0.0, 0.0])
            .show(ctx, |ui| {
                let size = ctx
                    .input(|i| i.viewport().inner_rect.map(|r| r.size()))
                    .unwrap_or(Vec2::splat(FAN_WIN));
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                let painter = ui.painter_at(rect);
                let center = Pos2::new(rect.right(), rect.center().y); // 扇心 = 右缘中点 = dot
                painter.circle_filled(center, DOT_R + 1.0, pal.text);

                let mpos = ctx.input(|i| i.pointer.latest_pos());
                let mut hover: Option<usize> = None;
                if let Some(mp) = mpos {
                    let rel = mp - center;
                    let dist = rel.length();
                    if dist <= FAN_R && rel.x < 0.0 {
                        // 左半平面 (扇区向左展开): 顶=CORE, 中=PLATFORM, 底=APP, 每 60° 一扇区。
                        let mut ang = (-rel.y).atan2(rel.x);
                        if ang < 0.0 {
                            ang += 2.0 * std::f32::consts::PI;
                        }
                        let idx = (((ang - std::f32::consts::FRAC_PI_2)
                            / (std::f32::consts::PI / 3.0))
                            as usize)
                            .min(2);
                        hover = Some(idx);
                    }
                }

                for i in 0..3usize {
                    let tier = Self::sector_tier(i);
                    let a0 = std::f32::consts::FRAC_PI_2 + i as f32 * (std::f32::consts::PI / 3.0);
                    let a1 =
                        std::f32::consts::FRAC_PI_2 + (i + 1) as f32 * (std::f32::consts::PI / 3.0);
                    let svcs = model.tier_svcs(tier);
                    let agg = state::sector(&svcs);
                    let alpha = if hover == Some(i) { 70u8 } else { 38u8 };
                    let fill = theme::tint(dot_color_of(sector_color(&agg)), alpha);
                    draw_sector(&painter, center, FAN_R, a0, a1, fill, pal.border);

                    let mid = (a0 + a1) / 2.0;
                    let label_r = FAN_R * 0.62;
                    let lp = Pos2::new(
                        center.x + label_r * mid.cos(),
                        center.y - label_r * mid.sin(),
                    );
                    painter.text(
                        lp,
                        Align2::CENTER_CENTER,
                        tier.label(),
                        FontId::proportional(13.0),
                        pal.text,
                    );
                    painter.text(
                        lp + Vec2::new(0.0, 16.0),
                        Align2::CENTER_CENTER,
                        format!("{}/{} 运行", agg.running, agg.total),
                        FontId::proportional(10.0),
                        pal.text_dim,
                    );
                    // 健康点阵: 径向向外堆行, 切向排列 (适配竖放左半圆)。
                    let cols = ((agg.dots.len() as f32).sqrt().ceil() as usize).max(1);
                    let (ux, uy) = (mid.cos(), -mid.sin());
                    let (px, py) = (-uy, ux);
                    for (di, dc) in agg.dots.iter().enumerate() {
                        let row = (di / cols) as f32;
                        let colx = (di % cols) as f32;
                        let off = (colx - (cols as f32 - 1.0) / 2.0) * 10.0;
                        let rr = FAN_R * 0.30 + row * 10.0;
                        let dp =
                            Pos2::new(center.x + ux * rr + px * off, center.y + uy * rr + py * off);
                        painter.circle_filled(dp, 2.5, dot_color_of(*dc));
                    }
                }

                // 点击: 扇区内 → L3; 其余 (含圆心) → 回 dot。
                if resp.clicked() {
                    if let Some(i) = hover {
                        self.ui_state = UiState::List(Self::sector_tier(i));
                    } else {
                        self.ui_state = UiState::Dot;
                    }
                }
            });
    }
}

fn draw_sector(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    a0: f32,
    a1: f32,
    fill: Color32,
    stroke_col: Color32,
) {
    let n = 24;
    let mut pts = vec![center];
    for k in 0..=n {
        let a = a0 + (a1 - a0) * (k as f32 / n as f32);
        pts.push(Pos2::new(
            center.x + radius * a.cos(),
            center.y - radius * a.sin(),
        ));
    }
    painter.add(egui::Shape::Path(egui::epaint::PathShape {
        points: pts,
        closed: true,
        fill,
        stroke: Stroke::new(1.0_f32, stroke_col).into(),
    }));
}

// ---------- L3 列表 ----------

impl TrayApp {
    fn ui_list(&mut self, ctx: &egui::Context, tier: Tier) {
        let model = self
            .shared
            .model
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if !model.reachable {
            self.ui_unreachable(ctx);
            return;
        }
        let pal = palette(&self.theme);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(pal.bg)
                    .rounding(egui::Rounding::same(theme::CORNER_RADIUS))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                // 头部: 返回 + 标题 + 运行计数。
                ui.horizontal(|ui| {
                    if ui.button("◀").clicked() {
                        self.ui_state = UiState::Fan;
                    }
                    ui.label(egui::RichText::new(tier.label()).strong());
                    let svcs = model.tier_svcs(tier);
                    let running = svcs.iter().filter(|s| s.state.is_running()).count();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{}/{}", running, svcs.len())).weak());
                    });
                });
                ui.separator();
                // 搜索 (实时过滤, 大小写不敏感)。
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("搜索…")
                            .desired_width(ui.available_width()),
                    );
                });
                ui.separator();
                // 行序: 运行中在前 + 注册表序 (稳定)。
                let shown = Model::sorted(Model::filtered(model.tier_svcs(tier), &self.search));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for s in &shown {
                        self.row_ui(ui, s);
                    }
                });
                ui.separator();
                // 分组动作: 三级统一 一键启动 + 一键停止 (停止为破坏性, 走确认弹窗)。
                ui.horizontal(|ui| {
                    let g = match tier {
                        Tier::Core => api::TierKey::T0,
                        Tier::Platform => api::TierKey::T1,
                        Tier::App => api::TierKey::T2,
                    };
                    if ui.button("一键启动").clicked() {
                        self.enqueue(api::Cmd::Group {
                            group: g,
                            action: api::Action::Up,
                        });
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("一键停止").clicked() {
                            self.destructive(
                                format!("确认停止 {} 全部服务?", tier.label()),
                                vec![api::Cmd::Group {
                                    group: g,
                                    action: api::Action::Down,
                                }],
                            );
                        }
                    });
                });
            });
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, s: &state::Svc) {
        let resp = ui
            .horizontal(|ui| {
                let (col, ch) = match s.state {
                    SvcState::Healthy => (GREEN, "●"),
                    SvcState::Starting => (YELLOW, "◐"),
                    SvcState::Unhealthy | SvcState::Failed => (RED, "●"),
                    SvcState::Stopped => (GRAY, "○"),
                };
                ui.add_sized(
                    [16.0, ROW_HEIGHT],
                    egui::Label::new(egui::RichText::new(ch).color(col).size(12.0)),
                );
                ui.add_sized(
                    [150.0, ROW_HEIGHT],
                    egui::Label::new(egui::RichText::new(&s.name).size(12.0)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(s.port.to_string()).weak().size(11.0));
                    // pin 切换 (T1/T2): 📌=已 auto, 📍=未 pin; 写 /api/v1/boot/pin。
                    if s.tier != Tier::Core {
                        let pinned = s.pinned.as_deref() == Some("auto");
                        let pin_btn = ui.add(
                            egui::Button::new(if pinned { "📌" } else { "📍" })
                                .small()
                                .fill(Color32::TRANSPARENT),
                        );
                        if pin_btn.clicked() {
                            let mode = if pinned {
                                api::PinMode::Clear
                            } else {
                                api::PinMode::Auto
                            };
                            // 乐观更新 (poll 探活回填纠错)。
                            let pin_val = match mode {
                                api::PinMode::Auto => Some("auto".into()),
                                api::PinMode::Manual => Some("manual".into()),
                                api::PinMode::Clear => None,
                            };
                            self.shared
                                .model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .set_pin(&s.name, pin_val);
                            self.enqueue(api::Cmd::Pin {
                                service: s.name.clone(),
                                mode,
                            });
                        }
                        pin_btn.on_hover_text("pin 为开机自启 (T0 注册表外)");
                    }
                    // 启停按钮 (文案随状态翻转), 乐观更新。
                    let running = s.state.is_running();
                    let label = if running { "停止" } else { "启动" };
                    if ui.small_button(label).clicked() {
                        if running {
                            self.shared
                                .model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .set_state(&s.name, SvcState::Stopped);
                        } else {
                            self.shared
                                .model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .set_state(&s.name, SvcState::Starting);
                        }
                        let cmd = if running {
                            api::Cmd::Down(s.name.clone())
                        } else {
                            api::Cmd::Up(s.name.clone())
                        };
                        if s.tier == Tier::Core && running {
                            self.destructive(format!("确认停止 {}?", s.name), vec![cmd]);
                        } else {
                            self.enqueue(cmd);
                        }
                    }
                });
            })
            .response;
        // 右键二级菜单 (P3): restart / drain / rollout (复用现有端点)。
        resp.context_menu(|ui| {
            if ui.button("restart").clicked() {
                self.enqueue(api::Cmd::Restart(s.name.clone()));
                ui.close_menu();
            }
            if ui.button("drain").clicked() {
                self.destructive(
                    format!("确认 drain {}?", s.name),
                    vec![api::Cmd::Drain(s.name.clone())],
                );
                ui.close_menu();
            }
            if ui.button("rollout").clicked() {
                self.destructive(
                    format!("确认 rollout {}?", s.name),
                    vec![api::Cmd::Rollout(s.name.clone())],
                );
                ui.close_menu();
            }
        });
    }
}

#[cfg(test)]
mod font_tests {
    // 子集字体覆盖回归: src/*.rs 全部非 ASCII 字符必须命中 cjk-subset.ttf 的 cmap,
    // 否则新增中文标签渲染成方块。例外 FONT_MISSING: 源字体 (Hiragino Sans GB)
    // 本身缺这些字形 (◀◐ 几何形走 egui 默认字体兜底; emoji 无 sans 字形) —
    // 与全量字体行为一致, 子集不引入回归。
    use super::CJK_SUBSET;

    const FONT_MISSING: &[char] = &['◀', '◐', '📌', '📍', '🔍'];

    #[test]
    fn test_subset_font_covers_source_charset() {
        let face = ttf_parser::Face::parse(CJK_SUBSET, 0).expect("内嵌 cjk-subset.ttf 必须可解析");
        let mut missing = Vec::new();
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src_dir).expect("src 目录可读") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("src 文件可读");
            for ch in text.chars().filter(|c| c.is_non_ascii_test()) {
                if face.glyph_index(ch).is_none() && !FONT_MISSING.contains(&ch) {
                    missing.push((ch, path.display().to_string()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "子集字体缺字形 (跑 tray/gen_font_subset.sh 再生成): {missing:?}"
        );
    }

    trait NonAsciiTest {
        fn is_non_ascii_test(&self) -> bool;
    }
    impl NonAsciiTest for char {
        fn is_non_ascii_test(&self) -> bool {
            *self as u32 > 0x7E
        }
    }
}
