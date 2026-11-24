//! Visual system: one palette per theme, the OS UI font at a fixed type scale, and the handful of
//! widgets every screen shares. Nothing else in the app picks colours, fonts or sizes.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response, RichText,
    Sense, Stroke, StrokeKind, TextStyle, Theme, ThemePreference, Ui,
};
use mb_core::conn::Engine;

use crate::store::ThemeChoice;

pub struct Pal {
    pub bg: Color32,
    pub sidebar: Color32,
    pub card: Color32,
    pub border: Color32,
    pub input: Color32,
    pub input_border: Color32,
    pub hover: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub accent_soft: Color32,
    pub green: Color32,
    pub amber: Color32,
    pub red: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub static DARK: Pal = Pal {
    bg: rgb(29, 29, 32),
    sidebar: rgb(25, 25, 28),
    card: rgb(34, 34, 38),
    border: rgb(50, 50, 56),
    input: rgb(24, 24, 27),
    input_border: rgb(56, 56, 63),
    hover: rgb(44, 44, 50),
    text: rgb(236, 236, 240),
    muted: rgb(150, 150, 160),
    faint: rgb(100, 100, 110),
    accent: rgb(146, 128, 255),
    on_accent: rgb(255, 255, 255),
    accent_soft: rgb(52, 49, 76),
    green: rgb(74, 222, 128),
    amber: rgb(251, 191, 36),
    red: rgb(248, 113, 113),
};

pub static LIGHT: Pal = Pal {
    bg: rgb(248, 248, 250),
    sidebar: rgb(241, 241, 245),
    card: rgb(255, 255, 255),
    border: rgb(228, 228, 234),
    input: rgb(252, 252, 253),
    input_border: rgb(214, 214, 222),
    hover: rgb(234, 234, 240),
    text: rgb(24, 24, 27),
    muted: rgb(108, 108, 120),
    faint: rgb(160, 160, 170),
    accent: rgb(108, 88, 240),
    on_accent: rgb(255, 255, 255),
    accent_soft: rgb(232, 229, 254),
    green: rgb(22, 163, 74),
    amber: rgb(202, 138, 4),
    red: rgb(220, 38, 38),
};

pub fn pal(ui: &Ui) -> &'static Pal {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

// ---- type scale -------------------------------------------------------------------------------

pub const BODY: f32 = 14.0;
pub const SMALL: f32 = 12.0;
pub const TITLE: f32 = 16.0;
pub const HEADING: f32 = 22.0;
pub const MONO: f32 = 12.5;

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn strong(t: impl Into<String>) -> RichText {
    RichText::new(t).family(semibold())
}
pub fn title(t: impl Into<String>) -> RichText {
    RichText::new(t).family(semibold()).size(TITLE)
}
pub fn heading(t: impl Into<String>) -> RichText {
    RichText::new(t).family(semibold()).size(HEADING)
}
pub fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).size(SMALL)
}
pub fn mono(t: impl Into<String>) -> RichText {
    RichText::new(t).monospace()
}

/// Uppercase, letter-spaced section label ("SOURCE", "JOBS").
pub fn section(ui: &mut Ui, t: &str, accent: bool) {
    let p = pal(ui);
    ui.label(
        RichText::new(t.to_uppercase())
            .family(semibold())
            .size(11.5)
            .extra_letter_spacing(1.2)
            .color(if accent { p.accent } else { p.faint }),
    );
}

pub fn muted(ui: &mut Ui, t: impl Into<String>) -> Response {
    let c = pal(ui).muted;
    ui.label(RichText::new(t).color(c))
}

// ---- fonts & style ----------------------------------------------------------------------------

fn read_first(paths: &[String]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

/// Use the platform UI font (Segoe UI / SF / Noto-DejaVu) everywhere, with a semibold face for
/// emphasis; egui's bundled fonts stay as glyph fallbacks and for monospace.
pub fn install_fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let w = |f: &str| format!("{windir}\\Fonts\\{f}");
    let (reg, semi): (Vec<String>, Vec<String>) = if cfg!(windows) {
        (vec![w("segoeui.ttf")], vec![w("seguisb.ttf"), w("segoeuib.ttf")])
    } else if cfg!(target_os = "macos") {
        (
            vec!["/System/Library/Fonts/SFNS.ttf".into(), "/System/Library/Fonts/Helvetica.ttc".into()],
            vec!["/System/Library/Fonts/SFNS.ttf".into(), "/System/Library/Fonts/Helvetica.ttc".into()],
        )
    } else {
        let regs = [
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ];
        let semis = [
            "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
            "/usr/share/fonts/noto/NotoSans-SemiBold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        ];
        (regs.iter().map(|s| s.to_string()).collect(), semis.iter().map(|s| s.to_string()).collect())
    };
    let mut fonts = FontDefinitions::default();
    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let regular = read_first(&reg);
    let bold = read_first(&semi);
    let mut prop = fallbacks.clone();
    if let Some(bytes) = regular {
        fonts.font_data.insert("ui".into(), FontData::from_owned(bytes).into());
        prop.insert(0, "ui".into());
    }
    let mut semi_family = fallbacks;
    if let Some(bytes) = bold {
        fonts.font_data.insert("ui-semibold".into(), FontData::from_owned(bytes).into());
        semi_family.insert(0, "ui-semibold".into());
    } else if prop.first().map(|s| s == "ui").unwrap_or(false) {
        semi_family.insert(0, "ui".into());
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(semibold(), semi_family);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_theme(match choice {
        ThemeChoice::System => ThemePreference::System,
        ThemeChoice::Dark => ThemePreference::Dark,
        ThemeChoice::Light => ThemePreference::Light,
    });
    for theme in [Theme::Dark, Theme::Light] {
        let p = if theme == Theme::Dark { &DARK } else { &LIGHT };
        ctx.style_mut_of(theme, |s| {
            s.text_styles = [
                (TextStyle::Small, FontId::proportional(SMALL)),
                (TextStyle::Body, FontId::proportional(BODY)),
                (TextStyle::Button, FontId::proportional(BODY)),
                (TextStyle::Heading, FontId::new(HEADING, semibold())),
                (TextStyle::Monospace, FontId::monospace(MONO)),
            ]
            .into();
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.window_margin = Margin::same(20);
            s.spacing.menu_margin = Margin::same(6);
            s.spacing.scroll.bar_width = 8.0;
            s.spacing.scroll.floating = true;

            let v = &mut s.visuals;
            v.dark_mode = theme == Theme::Dark;
            v.panel_fill = p.bg;
            v.window_fill = p.card;
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_corner_radius = CornerRadius::same(14);
            v.menu_corner_radius = CornerRadius::same(10);
            v.extreme_bg_color = p.input;
            v.text_edit_bg_color = Some(p.input);
            v.faint_bg_color = p.hover;
            v.code_bg_color = p.input;
            v.override_text_color = Some(p.text);
            v.hyperlink_color = p.accent;
            v.selection.bg_fill = p.accent.gamma_multiply(0.35);
            v.selection.stroke = Stroke::new(1.0, p.accent);
            v.warn_fg_color = p.amber;
            v.error_fg_color = p.red;
            v.window_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(60) };
            v.popup_shadow = egui::epaint::Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(40) };
            let r = CornerRadius::same(8);
            let w = &mut v.widgets;
            w.noninteractive.bg_fill = p.card;
            w.noninteractive.weak_bg_fill = p.card;
            w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
            w.inactive.bg_fill = p.input;
            w.inactive.weak_bg_fill = p.input;
            w.inactive.bg_stroke = Stroke::new(1.0, p.input_border);
            w.inactive.fg_stroke = Stroke::new(1.0, p.text);
            w.hovered.bg_fill = p.hover;
            w.hovered.weak_bg_fill = p.hover;
            w.hovered.bg_stroke = Stroke::new(1.0, p.input_border);
            w.hovered.fg_stroke = Stroke::new(1.0, p.text);
            w.active.bg_fill = p.hover;
            w.active.weak_bg_fill = p.hover;
            w.active.bg_stroke = Stroke::new(1.0, p.accent);
            w.active.fg_stroke = Stroke::new(1.0, p.text);
            w.open.bg_fill = p.hover;
            w.open.weak_bg_fill = p.hover;
            w.open.bg_stroke = Stroke::new(1.0, p.input_border);
            w.open.fg_stroke = Stroke::new(1.0, p.text);
            for st in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                st.corner_radius = r;
                st.expansion = 0.0;
            }
        });
    }
}

// ---- widgets ----------------------------------------------------------------------------------

/// Rounded container used for every grouped block of content.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = pal(ui);
    egui::Frame::NONE
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(22))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn sized(text: RichText, color: Color32) -> RichText {
    text.color(color)
}

pub fn primary(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let p = pal(ui);
    ui.add_enabled(
        enabled,
        egui::Button::new(sized(strong(text), p.on_accent))
            .fill(if enabled { p.accent } else { p.accent.gamma_multiply(0.4) })
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 34.0)),
    )
}

pub fn danger(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let p = pal(ui);
    ui.add_enabled(
        enabled,
        egui::Button::new(sized(strong(text), Color32::WHITE))
            .fill(if enabled { p.red } else { p.red.gamma_multiply(0.4) })
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 34.0)),
    )
}

pub fn secondary(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(strong(text)).corner_radius(CornerRadius::same(8)).min_size(egui::vec2(0.0, 34.0)),
    )
}

/// Borderless text button.
pub fn ghost(ui: &mut Ui, text: &str) -> Response {
    let c = pal(ui).muted;
    ui.add(egui::Button::new(RichText::new(text).color(c)).frame(false))
}

/// Full-width secondary button (e.g. "Test Connection").
pub fn wide(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let w = ui.available_width();
    ui.add_enabled(
        enabled,
        egui::Button::new(strong(text)).corner_radius(CornerRadius::same(8)).min_size(egui::vec2(w, 40.0)),
    )
}

pub fn field_label(ui: &mut Ui, t: &str) {
    let c = pal(ui).muted;
    ui.label(strong(t).color(c));
}

/// Single-line input in the screenshot style (tall, roomy padding).
pub fn input(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .margin(Margin::symmetric(12, 9))
            .min_size(egui::vec2(width, 40.0))
            .desired_width(width),
    )
}

pub fn password(ui: &mut Ui, value: &mut String, width: f32) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .password(true)
            .hint_text("Not set")
            .margin(Margin::symmetric(12, 9))
            .min_size(egui::vec2(width, 40.0))
            .desired_width(width),
    )
}

/// Radio button with an accent-filled selected state.
pub fn radio(ui: &mut Ui, selected: bool, text: &str) -> Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(BODY), p.text);
    let size = egui::vec2(20.0 + 8.0 + galley.size().x, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let c = egui::pos2(rect.left() + 10.0, rect.center().y);
    let painter = ui.painter();
    if selected {
        painter.circle_filled(c, 10.0, p.accent);
        painter.circle_filled(c, 4.0, p.input);
    } else {
        painter.circle_filled(c, 10.0, p.input);
        painter.circle_stroke(c, 9.5, Stroke::new(1.0, if resp.hovered() { p.muted } else { p.input_border }));
    }
    let text_pos = egui::pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, p.text);
    resp
}

/// Pill-shaped segmented control. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let p = pal(ui);
    let mut changed = false;
    egui::Frame::NONE
        .fill(p.input)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (v, label) in options {
                    let sel = *value == *v;
                    let text = if sel { strong(*label).color(p.text) } else { RichText::new(*label).color(p.muted) };
                    let b = egui::Button::new(text)
                        .fill(if sel { p.hover } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::same(7))
                        .min_size(egui::vec2(0.0, 28.0));
                    if ui.add(b).clicked() && !sel {
                        *value = *v;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Small tinted pill.
pub fn chip(ui: &mut Ui, text: &str, color: Color32) -> Response {
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.16))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| ui.label(strong(text).size(SMALL).color(color)))
        .response
}

/// Two-letter engine badge painted at `left_center`. Returns its width.
pub fn engine_badge(ui: &Ui, left_center: egui::Pos2, engine: Engine) -> f32 {
    let p = pal(ui);
    let rect = egui::Rect::from_min_size(left_center - egui::vec2(0.0, 10.0), egui::vec2(28.0, 20.0));
    ui.painter().rect_filled(rect, CornerRadius::same(5), p.accent_soft);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, engine.short(), FontId::new(10.5, semibold()), p.accent);
    rect.width()
}

pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Thin rounded progress bar.
pub fn progress(ui: &mut Ui, frac: f32, width: f32, height: f32, color: Color32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    let r = CornerRadius::same((height / 2.0) as u8);
    ui.painter().rect_filled(rect, r, p.hover);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * f).max(height));
        ui.painter().rect_filled(fill, r, color);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Open,
    Done,
    Locked,
}

/// Numbered step marker: number, check when complete, lock when gated.
pub fn step_marker(ui: &Ui, center: egui::Pos2, n: usize, st: StepState, active: bool) {
    let p = pal(ui);
    let painter = ui.painter();
    let r = 10.0;
    match st {
        StepState::Done => {
            painter.circle_filled(center, r, p.green.gamma_multiply(0.22));
            let s = Stroke::new(1.8, p.green);
            painter.line_segment([center + egui::vec2(-4.0, 0.2), center + egui::vec2(-1.2, 3.0)], s);
            painter.line_segment([center + egui::vec2(-1.2, 3.0), center + egui::vec2(4.2, -3.0)], s);
        }
        StepState::Locked => {
            painter.circle_filled(center, r, p.hover);
            let body = egui::Rect::from_center_size(center + egui::vec2(0.0, 1.8), egui::vec2(8.0, 6.0));
            painter.rect_filled(body, CornerRadius::same(1), p.faint);
            let arc = egui::Rect::from_center_size(center + egui::vec2(0.0, -2.0), egui::vec2(5.6, 6.0));
            painter.rect_stroke(arc, CornerRadius::same(3), Stroke::new(1.4, p.faint), StrokeKind::Middle);
        }
        StepState::Open => {
            painter.circle_filled(center, r, if active { p.accent } else { p.hover });
            painter.text(
                center,
                Align2::CENTER_CENTER,
                n.to_string(),
                FontId::new(11.5, semibold()),
                if active { p.on_accent } else { p.muted },
            );
        }
    }
}

/// A full-width clickable row with hover/selected fill. Returns the response and the content rect.
pub fn row(ui: &mut Ui, height: f32, selected: bool) -> (Response, egui::Rect) {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    let fill = if selected {
        p.accent_soft
    } else if resp.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    }
    (resp, rect)
}

pub fn empty_state(ui: &mut Ui, head: &str, sub: &str) {
    ui.add_space(64.0);
    ui.vertical_centered(|ui| {
        ui.label(title(head));
        muted(ui, sub);
    });
}

// ---- formatting -------------------------------------------------------------------------------

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn fmt_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else if ms < 3_600_000.0 {
        format!("{}m {:02}s", (ms / 60_000.0) as u64, ((ms / 1000.0) as u64) % 60)
    } else {
        format!("{}h {:02}m", (ms / 3_600_000.0) as u64, ((ms / 60_000.0) as u64) % 60)
    }
}

pub fn fmt_ago(ms: i64) -> String {
    let d = (crate::store::now_ms() - ms).max(0) / 1000;
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}
