//! Fluent (Windows 11) design tokens, fonts and small reusable widgets.
//!
//! Colours follow the WinUI theme resources: a Mica base behind the navigation
//! pane, a layer surface for page content, cards on top, and the system
//! signal colours for success, caution, critical and informational states.
//! Geometry follows Windows 11: 4 px for in-page controls, 8 px for overlays.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Shadow, Stroke, Theme, Ui, WidgetText, vec2};
use ystudio_core::lint::Severity;
use ystudio_core::results::Verdict;

pub struct Palette {
    /// Mica base: window background behind the navigation pane and status bar.
    pub base: Color32,
    /// Layer: the content area that holds pages.
    pub layer: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub divider: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_pressed: Color32,
    pub control_stroke: Color32,
    /// Check boxes, radio buttons and slider rails.
    pub control_alt: Color32,
    pub text_input: Color32,
    pub subtle_hover: Color32,
    pub subtle_pressed: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    pub on_accent: Color32,
    pub accent_text: Color32,
    /// Tint behind accent-coloured text, e.g. numbered badges.
    pub accent_bg: Color32,
    pub overlay: Color32,
    pub overlay_stroke: Color32,
    pub success: Color32,
    pub success_bg: Color32,
    pub caution: Color32,
    pub caution_bg: Color32,
    pub critical: Color32,
    pub critical_bg: Color32,
    pub info: Color32,
    pub info_bg: Color32,
    pub neutral: Color32,
    pub neutral_bg: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const DARK: Palette = Palette {
    base: hex(0x202020),
    layer: hex(0x272727),
    card: hex(0x2C2C2C),
    card_stroke: hex(0x1C1C1C),
    divider: hex(0x383838),
    control: hex(0x353535),
    control_hover: hex(0x3C3C3C),
    control_pressed: hex(0x2F2F2F),
    control_stroke: hex(0x424242),
    control_alt: hex(0x1F1F1F),
    text_input: hex(0x2F2F2F),
    subtle_hover: hex(0x2D2D2D),
    subtle_pressed: hex(0x292929),
    text: hex(0xFFFFFF),
    text_secondary: hex(0xC5C5C5),
    accent: hex(0xFE700A),
    accent_hover: hex(0xE8670A),
    accent_pressed: hex(0xD05E0C),
    on_accent: hex(0x000000),
    accent_text: hex(0xFE700A),
    accent_bg: hex(0x3A2614),
    overlay: hex(0x2C2C2C),
    overlay_stroke: hex(0x454545),
    success: hex(0x6CCB5F),
    success_bg: hex(0x393D1B),
    caution: hex(0xFCE100),
    caution_bg: hex(0x433519),
    critical: hex(0xFF99A4),
    critical_bg: hex(0x442726),
    info: hex(0x60CDFF),
    info_bg: hex(0x1E3646),
    neutral: hex(0x9D9D9D),
    neutral_bg: hex(0x333333),
};

pub const LIGHT: Palette = Palette {
    base: hex(0xF3F3F3),
    layer: hex(0xF9F9F9),
    card: hex(0xFFFFFF),
    card_stroke: hex(0xE5E5E5),
    divider: hex(0xE5E5E5),
    control: hex(0xFEFEFE),
    control_hover: hex(0xF6F6F6),
    control_pressed: hex(0xF0F0F0),
    control_stroke: hex(0xD9D9D9),
    control_alt: hex(0xF3F3F3),
    text_input: hex(0xFFFFFF),
    subtle_hover: hex(0xEAEAEA),
    subtle_pressed: hex(0xEDEDED),
    text: hex(0x1A1A1A),
    text_secondary: hex(0x5F5F5F),
    accent: hex(0xC15101),
    accent_hover: hex(0xAD4901),
    accent_pressed: hex(0x9E4301),
    on_accent: hex(0xFFFFFF),
    accent_text: hex(0xAD4901),
    accent_bg: hex(0xFFE8D6),
    overlay: hex(0xFFFFFF),
    overlay_stroke: hex(0xDDDDDD),
    success: hex(0x0F7B0F),
    success_bg: hex(0xDFF6DD),
    caution: hex(0x9D5D00),
    caution_bg: hex(0xFFF4CE),
    critical: hex(0xC42B1C),
    critical_bg: hex(0xFDE7E9),
    info: hex(0x005FB8),
    info_bg: hex(0xE5F0FA),
    neutral: hex(0x5F5F5F),
    neutral_bg: hex(0xEBEBEB),
};

static DARK_MODE: AtomicBool = AtomicBool::new(true);

pub fn palette() -> &'static Palette {
    if DARK_MODE.load(Ordering::Relaxed) { &DARK } else { &LIGHT }
}

pub fn green() -> Color32 {
    palette().success
}
pub fn red() -> Color32 {
    palette().critical
}
pub fn amber() -> Color32 {
    palette().caution
}
pub fn blue() -> Color32 {
    palette().info
}
pub fn grey() -> Color32 {
    palette().neutral
}
pub fn accent() -> Color32 {
    palette().accent_text
}

/// The background that pairs with a signal colour, for badges and info bars.
pub fn signal_bg(color: Color32) -> Color32 {
    let p = palette();
    [(p.success, p.success_bg), (p.caution, p.caution_bg), (p.critical, p.critical_bg), (p.info, p.info_bg), (p.neutral, p.neutral_bg), (p.accent_text, p.accent_bg)]
        .into_iter()
        .find(|(fg, _)| *fg == color)
        .map(|(_, bg)| bg)
        .unwrap_or_else(|| color.gamma_multiply(0.18))
}

/// Segoe Fluent Icons code points (shared with Segoe MDL2 Assets on Windows 10).
pub mod icon {
    pub const HOME: &str = "\u{E80F}";
    pub const LIBRARY: &str = "\u{E8F1}";
    pub const EDIT: &str = "\u{E70F}";
    pub const CODE: &str = "\u{E943}";
    pub const CHECKLIST: &str = "\u{E9D5}";
    pub const ROBOT: &str = "\u{E99A}";
    pub const INBOX: &str = "\u{E8C3}";
    pub const PHOTO: &str = "\u{E91B}";
    pub const CHART: &str = "\u{E9D2}";
    pub const BOOK: &str = "\u{E736}";
    pub const UPLOAD: &str = "\u{E898}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const CONSOLE: &str = "\u{E756}";
}

/// Yardstick brand artwork (assets/branding), embedded in the binary.
pub mod brand {
    use std::sync::OnceLock;

    use eframe::egui::{self, Response, Ui, vec2};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum Art {
        /// The drone carrying a checked box.
        Mark,
        /// The two-tone "yardstick" logotype.
        Wordmark,
    }

    /// Brand orange is `#FE700A`; the palette's `accent` uses it in dark mode and
    /// a darker shade of the same hue in light mode, for text contrast.
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/branding/icon-256.png");

    fn png(art: Art, dark: bool) -> &'static [u8] {
        match (art, dark) {
            (Art::Mark, true) => include_bytes!("../../../assets/branding/mark-dark.png"),
            (Art::Mark, false) => include_bytes!("../../../assets/branding/mark-light.png"),
            (Art::Wordmark, true) => include_bytes!("../../../assets/branding/wordmark-dark.png"),
            (Art::Wordmark, false) => include_bytes!("../../../assets/branding/wordmark-light.png"),
        }
    }

    /// Decoded once, with premultiplied alpha so resampling doesn't pull in the
    /// colour of transparent pixels (dark fringes around light artwork).
    fn source(art: Art, dark: bool) -> &'static image::RgbaImage {
        static CELLS: [OnceLock<image::RgbaImage>; 4] = [const { OnceLock::new() }; 4];
        let slot = (art as usize) * 2 + dark as usize;
        CELLS[slot].get_or_init(|| {
            let mut img = image::load_from_memory(png(art, dark)).map(|i| i.to_rgba8()).unwrap_or_else(|_| image::RgbaImage::new(1, 1));
            for p in img.pixels_mut() {
                let a = p[3] as u16;
                for c in 0..3 {
                    p[c] = ((p[c] as u16 * a + 127) / 255) as u8;
                }
            }
            img
        })
    }

    /// Paints `art` at `height` points for the current theme. The artwork is
    /// resampled on the CPU to the exact device pixel size (and cached), so it
    /// stays sharp on both renderers, which don't generate mipmaps.
    pub fn image(ui: &mut Ui, art: Art, height: f32, label: &str) -> Response {
        let dark = ui.visuals().dark_mode;
        let src = source(art, dark);
        let size = vec2((height * src.width() as f32 / src.height() as f32).round(), height);
        let ppp = ui.ctx().pixels_per_point();
        let (w, h) = (((size.x * ppp).round() as u32).max(1), ((size.y * ppp).round() as u32).max(1));
        let id = egui::Id::unique(("brand-art", art, dark, w, h));
        let tex = ui.ctx().data(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
            let scaled = image::imageops::resize(src, w, h, image::imageops::FilterType::Lanczos3);
            let ci = egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], scaled.as_raw());
            let tex = ui.ctx().load_texture(format!("brand-{w}x{h}"), ci, egui::TextureOptions::LINEAR);
            ui.ctx().data_mut(|d| d.insert_temp(id, tex.clone()));
            tex
        });
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, label));
        egui::Image::new((tex.id(), size)).paint_at(ui, rect);
        resp
    }

    /// The app icon for the window title bar and taskbar.
    pub fn window_icon() -> Option<egui::IconData> {
        let img = image::load_from_memory(ICON_PNG).ok()?.to_rgba8();
        Some(egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() })
    }
}

/// Family names registered by [`install_fonts`].
pub const SEMIBOLD: &str = "semibold";
pub const ICONS: &str = "icons";

pub fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// Fluent uses weight, not colour, for emphasis.
pub trait FluentText {
    fn semibold(self) -> Self;
}

impl FluentText for RichText {
    fn semibold(self) -> Self {
        self.family(semibold_family())
    }
}

/// Segoe UI Variable (Regular and Semibold), Segoe Fluent Icons and Cascadia Mono,
/// falling back to the Windows 10 equivalents, then to egui's bundled fonts.
pub fn install_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontTweak, VariationCoords};
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into()).join("Fonts");
    let read = |names: &[&str]| names.iter().find_map(|n| std::fs::read(dir.join(n)).ok().map(|b| (n.to_string(), b)));
    let mut defs = egui::FontDefinitions::default();
    let default_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let default_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let add = |defs: &mut egui::FontDefinitions, key: &str, bytes: Vec<u8>, tweak: FontTweak| {
        defs.font_data.insert(key.to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes).tweak(tweak)));
        Some(key.to_owned())
    };
    let weight = |w: f32| FontTweak { coords: VariationCoords::new([("wght", w)]), ..Default::default() };

    let (regular, semibold) = match read(&["SegUIVar.ttf"]) {
        Some((_, bytes)) => (add(&mut defs, "segoe-ui", bytes.clone(), weight(400.0)), add(&mut defs, "segoe-ui-semibold", bytes, weight(600.0))),
        None => (
            read(&["segoeui.ttf"]).and_then(|(_, b)| add(&mut defs, "segoe-ui", b, FontTweak::default())),
            read(&["seguisb.ttf"]).and_then(|(_, b)| add(&mut defs, "segoe-ui-semibold", b, FontTweak::default())),
        ),
    };
    let icons = read(&["SegoeIcons.ttf", "segmdl2.ttf"])
        .and_then(|(_, b)| add(&mut defs, "fluent-icons", b, FontTweak { y_offset_factor: 0.08, ..Default::default() }));
    let mono = read(&["CascadiaMono.ttf", "consola.ttf"]).and_then(|(_, b)| add(&mut defs, "mono", b, FontTweak::default()));
    let symbols: Vec<String> = [("segoe-ui-symbol", "seguisym.ttf"), ("segoe-ui-emoji", "seguiemj.ttf")]
        .into_iter()
        .filter_map(|(key, file)| read(&[file]).and_then(|(_, b)| add(&mut defs, key, b, FontTweak::default())))
        .collect();

    let chain = |first: Option<String>, rest: &[String]| -> Vec<String> {
        first.into_iter().chain(icons.clone()).chain(rest.iter().cloned()).chain(symbols.iter().cloned()).collect()
    };
    let prop = chain(regular.clone(), &default_prop);
    defs.families.insert(FontFamily::Name(SEMIBOLD.into()), chain(semibold.or(regular), &default_prop));
    defs.families.insert(FontFamily::Name(ICONS.into()), icons.clone().into_iter().chain(prop.iter().cloned()).collect());
    defs.families.insert(FontFamily::Monospace, chain(mono, &default_mono));
    defs.families.insert(FontFamily::Proportional, prop);
    ctx.set_fonts(defs);
}

/// Installs the Fluent style for both themes and activates one.
pub fn apply(ctx: &egui::Context, dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
    ctx.set_style_of(Theme::Dark, style(&DARK, Theme::Dark));
    ctx.set_style_of(Theme::Light, style(&LIGHT, Theme::Light));
    ctx.set_theme(if dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
}

fn style(p: &Palette, theme: Theme) -> egui::Style {
    use egui::TextStyle;
    let mut s = egui::Style {
        visuals: theme.default_visuals(),
        text_styles: [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
            (TextStyle::Heading, FontId::new(20.0, semibold_family())),
        ]
        .into(),
        ..Default::default()
    };

    let sp = &mut s.spacing;
    sp.item_spacing = vec2(8.0, 6.0);
    sp.button_padding = vec2(12.0, 5.0);
    sp.interact_size = vec2(40.0, 24.0);
    sp.window_margin = Margin::same(24);
    sp.menu_margin = Margin::same(4);
    sp.indent = 20.0;

    let v = &mut s.visuals;
    let ctl = CornerRadius::same(4);
    let widget = |bg: Color32, weak: Color32, stroke: Color32, fg: Color32| egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: ctl,
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    };
    v.widgets.noninteractive = widget(p.layer, p.layer, p.divider, p.text);
    v.widgets.inactive = widget(p.control_alt, p.control, p.control_stroke, p.text);
    v.widgets.hovered = widget(p.control_hover, p.control_hover, p.control_stroke, p.text);
    v.widgets.active = widget(p.control_pressed, p.control_pressed, p.control_stroke, p.text);
    v.widgets.open = widget(p.control_pressed, p.control_pressed, p.control_stroke, p.text);
    v.weak_text_color = Some(p.text_secondary);
    v.override_text_color = None;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.hyperlink_color = p.accent_text;
    v.faint_bg_color = if theme == Theme::Dark { hex(0x2A2A2A) } else { hex(0xF3F3F3) };
    v.extreme_bg_color = p.text_input;
    v.text_edit_bg_color = Some(p.text_input);
    v.code_bg_color = p.control_alt;
    v.warn_fg_color = p.caution;
    v.error_fg_color = p.critical;
    v.panel_fill = p.layer;
    v.window_fill = p.overlay;
    v.window_stroke = Stroke::new(1.0, p.overlay_stroke);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    let alpha = if theme == Theme::Dark { 110 } else { 45 };
    v.window_shadow = Shadow { offset: [0, 8], blur: 32, spread: 0, color: Color32::from_black_alpha(alpha) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(alpha) };
    v.text_cursor.stroke = Stroke::new(1.0, p.text);
    v.slider_trailing_fill = true;
    v.collapsing_header_frame = false;
    v.interact_cursor = None;
    s
}

pub fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Error => red(),
        Severity::Warning => amber(),
        Severity::Hint => blue(),
    }
}

pub fn severity_icon(s: Severity) -> &'static str {
    match s {
        Severity::Error => "\u{EA39}",
        Severity::Warning => "\u{E7BA}",
        Severity::Hint => "\u{E82F}",
    }
}

pub fn verdict_color(v: Verdict) -> Color32 {
    match v {
        Verdict::Validated => green(),
        Verdict::ValidatedWithWarnings => amber(),
        Verdict::DryRunPassed => blue(),
        Verdict::Failed => red(),
        Verdict::Incomplete => grey(),
    }
}

pub fn status_color(status: &str) -> Color32 {
    match status.to_ascii_lowercase().as_str() {
        "pass" | "passed" => green(),
        "fail" | "failed" | "error" => red(),
        "warn" | "warning" => amber(),
        _ => grey(),
    }
}

/// A small status pill on its signal background.
pub fn badge(ui: &mut Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    let text = text.into();
    let galley = WidgetText::from(RichText::new(&text).small().semibold()).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Small);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(16.0, 4.0), egui::Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &text));
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 10, signal_bg(color));
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    resp
}

/// A full-width InfoBar-style message on the signal background.
pub fn banner(ui: &mut Ui, color: Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(signal_bg(color))
        .stroke(Stroke::new(1.0, palette().card_stroke))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
}

pub fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let p = palette();
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.card_stroke)).corner_radius(4).inner_margin(Margin::same(16)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Columns lay out justified; Fluent text is left-aligned and buttons fit their label.
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), add)
    });
}

/// Gap between cards, both between rows and between cards in a row.
pub const CARD_GAP: f32 = 16.0;

/// A row of equal-width, equal-height cards. Each card has a body and a footer;
/// bodies are padded to the tallest body so footers (usually the card's action)
/// line up across the row. Heights are measured each frame and applied on the next.
pub fn card_row<T>(
    ui: &mut Ui,
    state: &mut T,
    id_salt: &str,
    count: usize,
    body: impl Fn(&mut T, usize, &mut Ui),
    footer: impl Fn(&mut T, usize, &mut Ui),
) {
    let id = ui.make_persistent_id(("card-row", id_salt));
    let target: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    let width = ((ui.available_width() - CARD_GAP * (count as f32 - 1.0)) / count as f32).floor();
    let mut tallest = 0.0f32;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = CARD_GAP;
        for i in 0..count {
            ui.allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(width);
                card(ui, |ui| {
                    let natural = ui.scope(|ui| body(state, i, ui)).response.rect.height();
                    tallest = tallest.max(natural);
                    if target > natural {
                        ui.add_space(target - natural);
                    }
                    ui.add_space(4.0);
                    footer(state, i, ui);
                });
            });
        }
    });
    if (tallest - target).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, tallest));
        ui.ctx().request_repaint();
    }
}

/// Fluent "Title": the one large heading at the top of a page.
pub fn page_title(ui: &mut Ui, text: &str) -> Response {
    ui.label(RichText::new(text).size(28.0).family(semibold_family()))
}

/// Fluent "Subtitle": a section heading inside a page or card.
pub fn section_heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).heading());
    ui.add_space(4.0);
}

/// An accent (primary) button. Disabled accent buttons fall back to the
/// neutral disabled look, as in WinUI.
pub fn primary(ui: &mut Ui, enabled: bool, text: impl Into<WidgetText>) -> Response {
    ui.scope(|ui| {
        if enabled {
            let p = palette();
            let w = &mut ui.visuals_mut().widgets;
            for (v, fill) in [(&mut w.inactive, p.accent), (&mut w.hovered, p.accent_hover), (&mut w.active, p.accent_pressed)] {
                v.weak_bg_fill = fill;
                v.bg_fill = fill;
                v.bg_stroke = Stroke::new(1.0, fill);
                v.fg_stroke.color = p.on_accent;
            }
        }
        ui.add_enabled(enabled, egui::Button::new(text))
    })
    .inner
}

/// A NavigationView item: subtle backplate, accent selection pill, 16 px icon.
pub fn nav_item(ui: &mut Ui, glyph: &str, label: &str, selected: bool, busy: bool) -> Response {
    let p = palette();
    let (outer, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, label));
    if !ui.is_rect_visible(outer) {
        return resp;
    }
    let rect = outer.shrink2(vec2(4.0, 2.0));
    let fill = match (selected, resp.is_pointer_button_down_on(), resp.hovered()) {
        (_, true, _) => p.subtle_pressed,
        (true, _, true) => p.control_hover,
        (true, _, false) | (false, _, true) => p.subtle_hover,
        _ => Color32::TRANSPARENT,
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 4, fill);
    if selected {
        let pill = egui::Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), vec2(3.0, 16.0));
        painter.rect_filled(pill, 2, p.accent);
    }
    if resp.has_focus() {
        painter.rect_stroke(rect, 4, Stroke::new(2.0, p.text), egui::StrokeKind::Inside);
    }
    let y = rect.center().y;
    painter.text(egui::pos2(rect.left() + 12.0, y), egui::Align2::LEFT_CENTER, glyph, FontId::new(16.0, FontFamily::Name(ICONS.into())), p.text);
    painter.text(egui::pos2(rect.left() + 44.0, y), egui::Align2::LEFT_CENTER, label, FontId::proportional(14.0), p.text);
    if busy {
        painter.circle_filled(egui::pos2(rect.right() - 14.0, y), 3.5, p.accent);
    }
    resp
}
