// PRD §4.3 视觉规范: Slate/Zinc 中性色系 + 语义色常量, 深浅两套 style。
// 语义色与 fusion-studio / web 台对齐 (绿 #10b981 / 黄 #f59e0b / 红 #ef4444)。
// 圆角 8px, 紧凑行高 28px, 无衬线字体 (egui 默认即是)。

use eframe::egui;

pub const GREEN: egui::Color32 = egui::Color32::from_rgb(0x10, 0xb9, 0x81);
pub const YELLOW: egui::Color32 = egui::Color32::from_rgb(0xf5, 0x9e, 0x0b);
pub const RED: egui::Color32 = egui::Color32::from_rgb(0xef, 0x44, 0x44);
pub const GRAY: egui::Color32 = egui::Color32::from_rgb(0x94, 0xa3, 0xb8);

pub const ROW_HEIGHT: f32 = 28.0;
pub const CORNER_RADIUS: f32 = 8.0;

// 扇区底色 = 健康色淡化 (alpha 15%)。
pub fn tint(c: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

pub struct Palette {
    pub bg: egui::Color32,
    pub panel: egui::Color32,
    pub text: egui::Color32,
    pub text_dim: egui::Color32,
    pub border: egui::Color32,
    pub hover: egui::Color32,
    pub accent: egui::Color32,
}

// 深色 = 默认 (悬浮点常驻桌面, 深色面板在浅色壁纸上也可读)。
pub const DARK: Palette = Palette {
    bg: egui::Color32::from_rgb(0x0f, 0x17, 0x2a), // slate-900
    panel: egui::Color32::from_rgb(0x1e, 0x29, 0x3b), // slate-800
    text: egui::Color32::from_rgb(0xe2, 0xe8, 0xf0), // slate-200
    text_dim: egui::Color32::from_rgb(0x94, 0xa3, 0xb8), // slate-400
    border: egui::Color32::from_rgb(0x33, 0x41, 0x55), // slate-700
    hover: egui::Color32::from_rgb(0x27, 0x35, 0x49),
    accent: egui::Color32::from_rgb(0x63, 0x66, 0xf1), // indigo-500
};

pub const LIGHT: Palette = Palette {
    bg: egui::Color32::from_rgb(0xf8, 0xfa, 0xfc), // slate-50
    panel: egui::Color32::from_rgb(0xff, 0xff, 0xff),
    text: egui::Color32::from_rgb(0x0f, 0x17, 0x2a), // slate-900
    text_dim: egui::Color32::from_rgb(0x64, 0x74, 0x8b), // slate-500
    border: egui::Color32::from_rgb(0xe2, 0xe8, 0xf0), // slate-200
    hover: egui::Color32::from_rgb(0xf1, 0xf5, 0xf9), // slate-100
    accent: egui::Color32::from_rgb(0x4f, 0x46, 0xe5), // indigo-600
};

pub fn apply_style(ctx: &egui::Context, p: &Palette) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    v.panel_fill = p.panel;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.hover;
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.inactive.bg_fill = p.panel;
    v.widgets.inactive.weak_bg_fill = p.panel;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, p.border);
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.active.weak_bg_fill = p.accent;
    v.selection.bg_fill = p.accent;
    v.override_text_color = Some(p.text);
    v.window_rounding = egui::Rounding::same(CORNER_RADIUS);
    v.menu_rounding = egui::Rounding::same(CORNER_RADIUS);
    ctx.set_style(style);
}

// 跟随系统 appearance (macOS): `defaults read -g AppleInterfaceStyle`,
// 输出 Dark = 深色; key 不存在 (浅色) 命令非零退出。FUSION_TRAY_THEME 覆盖。
pub fn detect_theme() -> &'static str {
    if let Ok(t) = std::env::var("FUSION_TRAY_THEME") {
        return match t.as_str() {
            "light" => "light",
            _ => "dark",
        };
    }
    match std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.trim().eq_ignore_ascii_case("dark") {
                "dark"
            } else {
                "light"
            }
        }
        _ => "light",
    }
}

pub fn palette(theme: &str) -> &'static Palette {
    if theme == "dark" { &DARK } else { &LIGHT }
}
