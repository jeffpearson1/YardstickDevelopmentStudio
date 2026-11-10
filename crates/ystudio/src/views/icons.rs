//! Icon finder: searches installers, Wikidata, Chocolatey, the vendor site and
//! an image search in parallel, ranks the results, and saves a cleaned-up,
//! square, size-limited PNG into the workspace's Icons folder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::SystemTime;

use eframe::egui::{self, Color32, RichText, TextureHandle, Ui};
use ystudio_core::icons::{self, Candidate, IconQuery, Process, RgbaImage, SearchEvent};

use crate::app::StudioApp;
use crate::theme::{self, FluentText as _};

const THUMB: f32 = 112.0;
const PREVIEW: f32 = 192.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Backdrop {
    #[default]
    Checker,
    Dark,
    Light,
}

#[derive(Default)]
pub struct IconsState {
    pub name: String,
    pub publisher: String,
    pub vendor_url: String,
    pub file_name: String,
    pub files: Vec<PathBuf>,
    /// Recipe the icon is for; its `iconFile` can be set after saving.
    pub recipe_id: Option<String>,
    /// Batch item the icon is for (batch id, item index).
    pub batch_item: Option<(String, usize)>,
    offline: bool,
    filter: String,
    rx: Option<Receiver<SearchEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    status: String,
    errors: Vec<String>,
    candidates: Vec<Candidate>,
    thumbs: HashMap<String, TextureHandle>,
    selected: Option<String>,
    user_picked: bool,
    process: Process,
    preview: Option<Preview>,
    backdrop: Backdrop,
    last_saved: Option<String>,
    current: Option<CurrentIcon>,
    /// Start a search on the next frame (set by other views' hooks).
    pub auto_search: bool,
}

/// The icon file the recipe uses today, shown beside the candidates.
struct CurrentIcon {
    path: PathBuf,
    stamp: Option<SystemTime>,
    /// `None` when the file is missing or cannot be decoded.
    texture: Option<(TextureHandle, (u32, u32))>,
}

struct Preview {
    origin: String,
    process: Process,
    image: RgbaImage,
    texture: TextureHandle,
}

impl IconsState {
    /// Prefills the form for a recipe or batch item and starts searching.
    pub fn prepare(&mut self, name: &str, publisher: &str, vendor_url: &str, file_name: &str) {
        self.stop();
        self.name = name.trim().to_string();
        self.publisher = publisher.trim().to_string();
        self.vendor_url = vendor_url.trim().to_string();
        self.file_name = if file_name.trim().is_empty() { icons::icon_file_name(name) } else { file_name.trim().to_string() };
        self.files.clear();
        self.recipe_id = None;
        self.batch_item = None;
        self.candidates.clear();
        self.thumbs.clear();
        self.selected = None;
        self.preview = None;
        self.errors.clear();
        self.status.clear();
        self.last_saved = None;
        self.auto_search = !self.name.is_empty();
    }

    pub fn searching(&self) -> bool {
        self.rx.is_some()
    }

    fn stop(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
        self.cancel = None;
        self.rx = None;
    }

    fn start(&mut self, waker: ystudio_core::process::Waker) {
        self.stop();
        self.candidates.clear();
        self.thumbs.clear();
        self.selected = None;
        self.user_picked = false;
        self.preview = None;
        self.errors.clear();
        self.last_saved = None;
        self.status = "Searching…".into();
        let query = IconQuery {
            name: self.name.trim().to_string(),
            publisher: self.publisher.trim().to_string(),
            vendor_url: self.vendor_url.trim().to_string(),
            files: self.files.clone(),
            offline: self.offline,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.rx = Some(icons::search(query, cancel.clone(), Some(waker)));
        self.cancel = Some(cancel);
    }

    /// Drains search events; cheap when nothing is pending.
    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(SearchEvent::Status(s)) => {
                    if s.to_lowercase().contains("fail") || s.to_lowercase().contains("error") {
                        self.errors.push(s.clone());
                    }
                    self.status = s;
                }
                Ok(SearchEvent::Candidate(c)) => {
                    icons::merge(&mut self.candidates, *c);
                    // Follow the best candidate until the user picks one.
                    if !self.user_picked
                        && let Some(top) = self.candidates.first()
                        && self.selected.as_ref() != Some(&top.origin)
                    {
                        self.process = Process::auto(&top.image);
                        self.selected = Some(top.origin.clone());
                    }
                }
                Ok(SearchEvent::Finished) | Err(TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if done {
            self.rx = None;
            self.cancel = None;
            self.status = format!("Done - {} candidate(s).", self.candidates.len());
        }
    }

    fn selected(&self) -> Option<&Candidate> {
        let o = self.selected.as_ref()?;
        self.candidates.iter().find(|c| &c.origin == o)
    }

    fn select(&mut self, origin: String) {
        self.user_picked = true;
        if self.selected.as_ref() != Some(&origin) {
            if let Some(c) = self.candidates.iter().find(|c| c.origin == origin) {
                self.process = Process::auto(&c.image);
            }
            self.selected = Some(origin);
        }
    }
}

fn texture(ctx: &egui::Context, name: &str, img: &RgbaImage) -> TextureHandle {
    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    ctx.load_texture(name, ci, egui::TextureOptions::LINEAR)
}

/// Fits `size` into a `side` square, preserving the aspect ratio.
fn fit(size: (u32, u32), side: f32) -> egui::Vec2 {
    let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let s = side / w.max(h);
    egui::vec2(w * s, h * s)
}

fn backdrop(ui: &Ui, rect: egui::Rect, kind: Backdrop) {
    let p = ui.painter();
    match kind {
        Backdrop::Dark => {
            p.rect_filled(rect, 4.0, Color32::from_rgb(32, 32, 36));
        }
        Backdrop::Light => {
            p.rect_filled(rect, 4.0, Color32::WHITE);
        }
        Backdrop::Checker => {
            p.rect_filled(rect, 4.0, Color32::from_gray(235));
            let cell = 12.0;
            let cols = (rect.width() / cell).ceil() as i32;
            let rows = (rect.height() / cell).ceil() as i32;
            for y in 0..rows {
                for x in 0..cols {
                    if (x + y) % 2 == 0 {
                        let min = rect.min + egui::vec2(x as f32 * cell, y as f32 * cell);
                        let r = egui::Rect::from_min_size(min, egui::vec2(cell, cell)).intersect(rect);
                        p.rect_filled(r, 0.0, Color32::from_gray(200));
                    }
                }
            }
        }
    }
}

/// Recipes whose `iconFile` is empty or missing from Icons\.
fn missing_icons(app: &StudioApp) -> Vec<(String, String)> {
    let have: std::collections::HashSet<String> = app.ws_cache.icons.iter().map(|i| i.to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in &app.recipes {
        let f = r.meta.icon_file.trim();
        if (f.is_empty() || !have.contains(&f.to_lowercase())) && seen.insert(r.id.to_lowercase()) {
            out.push((r.id.clone(), if f.is_empty() { "no iconFile".to_string() } else { format!("{f} missing") }));
        }
    }
    out
}

/// Prefills the finder for a recipe in the library.
pub fn prepare_for_recipe(app: &mut StudioApp, id: &str) {
    let entry = app.recipes.iter().find(|r| r.id == id).cloned();
    let (name, publisher, url, file) = match &entry {
        Some(e) => (
            if e.meta.display_name.is_empty() { e.id.clone() } else { e.meta.display_name.clone() },
            e.meta.publisher.clone(),
            e.meta.url.clone(),
            if e.meta.icon_file.is_empty() { icons::icon_file_name(&e.id) } else { e.meta.icon_file.clone() },
        ),
        None => (id.to_string(), String::new(), String::new(), icons::icon_file_name(id)),
    };
    app.icons.prepare(&name, &publisher, &url, &file);
    app.icons.recipe_id = Some(id.to_string());
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    app.icons.poll();
    if app.icons.auto_search {
        app.icons.auto_search = false;
        app.icons.start(app.waker.clone());
    }
    // The open recipe's Icon tab; otherwise the library-wide finder.
    let for_open_recipe = app.icons.batch_item.is_none()
        && app.recipe.as_ref().is_some_and(|r| app.icons.recipe_id.as_deref() == Some(r.id.as_str()));
    if for_open_recipe {
        super::recipe_header(app, ui);
    } else {
        ui.horizontal(|ui| {
            theme::page_title(ui, "Icon finder");
            ui.label(RichText::new("installer resources · Wikidata · Chocolatey · vendor site · image search").weak());
        });
        egui::Panel::left("icons-missing").resizable(true).default_size(260.0).show(ui, |ui| missing_panel(app, ui));
    }
    egui::Panel::right("icons-preview").resizable(true).default_size(330.0).show(ui, |ui| preview_panel(app, ui));
    egui::ScrollArea::vertical().id_salt("icons-main").auto_shrink(false).show(ui, |ui| {
        query_form(app, ui);
        ui.separator();
        candidate_grid(app, ui);
    });
}

fn missing_panel(app: &mut StudioApp, ui: &mut Ui) {
    if app.ws.is_none() {
        ui.label(RichText::new("Open a Yardstick folder to see which recipes lack icons.").weak());
        return;
    }
    let missing = missing_icons(app);
    theme::section_heading(ui, &format!("Missing icons ({})", missing.len()));
    ui.add(egui::TextEdit::singleline(&mut app.icons.filter).hint_text("filter").desired_width(f32::INFINITY));
    let f = app.icons.filter.to_lowercase();
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("icons-missing-scroll").show(ui, |ui| {
        for (id, why) in missing.iter().filter(|(id, _)| f.is_empty() || id.to_lowercase().contains(&f)) {
            let selected = app.icons.recipe_id.as_deref() == Some(id.as_str());
            let resp = ui.add(egui::Button::selectable(selected, RichText::new(id).monospace())).on_hover_text(why);
            if resp.clicked() {
                pick = Some(id.clone());
            }
        }
        if missing.is_empty() {
            ui.colored_label(theme::green(), "\u{E73E}  Every recipe's iconFile exists.");
        }
    });
    if let Some(id) = pick {
        prepare_for_recipe(app, &id);
    }
}

fn query_form(app: &mut StudioApp, ui: &mut Ui) {
    let ids: Vec<String> = app.recipes.iter().map(|r| r.id.clone()).collect();
    let mut pick_recipe = None;
    let st = &mut app.icons;
    egui::Grid::new("icon-query").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        ui.label("Application");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.name).hint_text("e.g. QGIS").desired_width(260.0));
            egui::ComboBox::from_id_salt("icon-recipe")
                .selected_text(st.recipe_id.clone().unwrap_or_else(|| "for recipe…".into()))
                .height(400.0)
                .show_ui(ui, |ui| {
                    for id in &ids {
                        if ui.selectable_label(st.recipe_id.as_deref() == Some(id.as_str()), id).clicked() {
                            pick_recipe = Some(id.clone());
                        }
                    }
                });
        });
        ui.end_row();
        ui.label("Publisher");
        ui.add(egui::TextEdit::singleline(&mut st.publisher).hint_text("optional, improves ranking").desired_width(260.0));
        ui.end_row();
        ui.label("Vendor page");
        ui.add(egui::TextEdit::singleline(&mut st.vendor_url).hint_text("optional: site favicon / manifest / touch icon").desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Installers");
        ui.horizontal_wrapped(|ui| {
            let mut remove = None;
            for (i, f) in st.files.iter().enumerate() {
                let n = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if ui.small_button(format!("\u{E711}  {n}")).on_hover_text(f.display().to_string()).clicked() {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                st.files.remove(i);
            }
            if ui.small_button("\u{E710}  Add EXE / MSI / image…").clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .add_filter("Installers and images", &["exe", "msi", "dll", "ico", "png", "svg", "jpg", "jpeg", "webp", "bmp", "gif"])
                    .pick_files()
            {
                st.files.extend(paths);
            }
        });
        ui.end_row();
    });
    if let Some(id) = pick_recipe {
        prepare_for_recipe(app, &id);
        app.icons.auto_search = false;
    }
    // Dropped installers become file sources.
    let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
    let waker = app.waker.clone();
    let st = &mut app.icons;
    st.files.extend(dropped);
    ui.horizontal(|ui| {
        let can = !st.name.trim().is_empty() || !st.files.is_empty();
        if st.searching() {
            ui.spinner();
            if ui.button("\u{E71A}  Cancel").clicked() {
                st.stop();
                st.status = "Cancelled.".into();
            }
        } else if theme::primary(ui, can, RichText::new("\u{E721}  Search")).clicked() {
            st.start(waker);
        }
        ui.checkbox(&mut st.offline, "Installers only (offline)");
        ui.label(RichText::new(&st.status).weak());
    });
    if !app.icons.errors.is_empty() {
        ui.collapsing(format!("{} source(s) had problems", app.icons.errors.len()), |ui| {
            for e in &app.icons.errors {
                ui.label(RichText::new(e).small().weak());
            }
        });
    }
}

fn candidate_grid(app: &mut StudioApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let st = &mut app.icons;
    if st.candidates.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            let idle = if st.recipe_id.is_some() { "Search to find candidates for this recipe." } else { "Pick a recipe on the left, or type a name and search." };
            ui.label(RichText::new(if st.searching() { "Looking…" } else { idle }).weak());
            ui.label(RichText::new("Drop an installer here to read its embedded icons.").small().weak());
        });
        return;
    }
    let mut clicked = None;
    let width = ui.available_width();
    let per_row = ((width / (THUMB + 28.0)).floor() as usize).max(1);
    let backdrop_kind = st.backdrop;
    egui::Grid::new("icon-candidates").spacing([10.0, 10.0]).show(ui, |ui| {
        for (i, c) in st.candidates.iter().enumerate() {
            let tex = st.thumbs.entry(c.origin.clone()).or_insert_with(|| {
                let small = icons::resize_fit(&c.image, (THUMB * 2.0) as u32);
                texture(&ctx, &c.origin, &small)
            });
            let selected = st.selected.as_ref() == Some(&c.origin);
            let frame = egui::Frame::new()
                .stroke(egui::Stroke::new(if selected { 2.5 } else { 1.0 }, if selected { theme::accent() } else { ui.visuals().widgets.noninteractive.bg_stroke.color }))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(6));
            let resp = frame
                .show(ui, |ui| {
                    ui.set_width(THUMB + 8.0);
                    ui.vertical_centered(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(THUMB, THUMB), egui::Sense::hover());
                        backdrop(ui, rect, backdrop_kind);
                        let size = fit((c.image.width(), c.image.height()), THUMB - 8.0);
                        egui::Image::new((tex.id(), size)).paint_at(ui, egui::Rect::from_center_size(rect.center(), size));
                        let color = if c.score >= 0.7 { theme::green() } else if c.score >= 0.45 { theme::amber() } else { theme::grey() };
                        ui.horizontal(|ui| {
                            theme::badge(ui, c.source.label(), theme::blue());
                            ui.label(RichText::new(format!("{:.0}%", c.score * 100.0)).small().color(color));
                        });
                        let dims = if c.vector { "SVG".to_string() } else { format!("{}×{}", c.native.0, c.native.1) };
                        ui.label(RichText::new(dims).small().weak());
                    });
                })
                .response
                .interact(egui::Sense::click());
            let mut tip = format!("{}\n{}\n", c.label, c.origin);
            for r in &c.reasons {
                tip += &format!("\n• {r}");
            }
            if !c.also_from.is_empty() {
                tip += &format!("\n\nAlso found via: {}", c.also_from.join(", "));
            }
            if resp.on_hover_text(tip).clicked() {
                clicked = Some(c.origin.clone());
            }
            if (i + 1) % per_row == 0 {
                ui.end_row();
            }
        }
    });
    if let Some(o) = clicked {
        st.select(o);
    }
}

/// The recipe's `iconFile`, or the file name being saved when no recipe is chosen.
fn current_icon_path(app: &StudioApp) -> Option<PathBuf> {
    let ws = app.ws.as_ref()?;
    let from_recipe = app.icons.recipe_id.as_ref()
        .and_then(|id| app.recipes.iter().find(|r| &r.id == id))
        .map(|r| r.meta.icon_file.trim().to_string())
        .filter(|f| !f.is_empty());
    let file = from_recipe.unwrap_or_else(|| app.icons.file_name.trim().to_string());
    (!file.is_empty() && !file.contains(['\\', '/', ':'])).then(|| ws.layout.icons.join(file))
}

fn current_icon(app: &mut StudioApp, ui: &mut Ui) {
    let Some(path) = current_icon_path(app) else { return };
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let st = &mut app.icons;
    if st.current.as_ref().is_none_or(|c| c.path != path || c.stamp != stamp) {
        let loaded = stamp.and_then(|_| std::fs::read(&path).ok()).and_then(|b| icons::decode(&b).ok())
            .map(|d| (texture(ui.ctx(), "icon-current", &d.image), (d.image.width(), d.image.height())));
        st.current = Some(CurrentIcon { path: path.clone(), stamp, texture: loaded });
    }
    let c = st.current.as_ref().unwrap();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    theme::section_heading(ui, "Current icon");
    match &c.texture {
        Some((tex, size)) => {
            ui.horizontal(|ui| {
                for side in [96.0, 32.0] {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                    backdrop(ui, r, st.backdrop);
                    let s = fit(*size, side - 4.0);
                    egui::Image::new((tex.id(), s)).paint_at(ui, egui::Rect::from_center_size(r.center(), s));
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new(&name).monospace());
                    ui.label(RichText::new(format!("{}×{}", size.0, size.1)).small().weak());
                });
            });
        }
        None if c.stamp.is_some() => {
            ui.colored_label(theme::amber(), format!("Icons\\{name} exists but could not be read."));
        }
        None => {
            ui.label(RichText::new(format!("None yet - Icons\\{name} does not exist.")).weak());
        }
    }
    ui.add_space(6.0);
}

fn preview_panel(app: &mut StudioApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    egui::ScrollArea::vertical().id_salt("icons-preview-scroll").show(ui, |ui| {
        current_icon(app, ui);
        let Some(c) = app.icons.selected().cloned() else {
            ui.add_space(14.0);
            ui.label(RichText::new("Select a candidate to preview and save it.").weak());
            return;
        };
        theme::section_heading(ui, "Preview");
        ui.label(RichText::new(&c.label).semibold());
        ui.label(RichText::new(&c.origin).small().weak());

        // Recompute only when the candidate or the settings change.
        let st = &mut app.icons;
        let stale = st.preview.as_ref().is_none_or(|p| p.origin != c.origin || p.process != st.process);
        if stale {
            let image = icons::process(&c.image, &st.process);
            let texture = texture(&ctx, "icon-preview", &image);
            st.preview = Some(Preview { origin: c.origin.clone(), process: st.process, image, texture });
        }
        let p = st.preview.as_ref().unwrap();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut st.backdrop, Backdrop::Checker, "Checker");
            ui.selectable_value(&mut st.backdrop, Backdrop::Dark, "Dark");
            ui.selectable_value(&mut st.backdrop, Backdrop::Light, "Light");
        });
        let (rect, _) = ui.allocate_exact_size(egui::vec2(PREVIEW, PREVIEW), egui::Sense::hover());
        backdrop(ui, rect, st.backdrop);
        let size = fit((p.image.width(), p.image.height()), PREVIEW - 8.0);
        egui::Image::new((p.texture.id(), size)).paint_at(ui, egui::Rect::from_center_size(rect.center(), size));
        // Company Portal shows icons small; check legibility too.
        ui.horizontal(|ui| {
            for side in [64.0, 32.0] {
                let (r, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                backdrop(ui, r, st.backdrop);
                let s = fit((p.image.width(), p.image.height()), side);
                egui::Image::new((p.texture.id(), s)).paint_at(ui, egui::Rect::from_center_size(r.center(), s));
            }
            ui.label(RichText::new(format!("{}×{}", p.image.width(), p.image.height())).small().weak());
        });

        ui.add_space(6.0);
        theme::section_heading(ui, "Clean-up");
        let pr = &mut st.process;
        ui.checkbox(&mut pr.remove_background, "Remove flat background").on_hover_text("Flood-fills from the edges only, so white inside the logo stays.");
        ui.add_enabled(pr.remove_background, egui::Slider::new(&mut pr.tolerance, 4..=80).text("tolerance"));
        ui.checkbox(&mut pr.trim, "Trim transparent margins");
        ui.checkbox(&mut pr.square, "Pad to square");
        ui.add(egui::Slider::new(&mut pr.margin, 0.0..=0.3).text("margin").fixed_decimals(2));
        let mut fill = pr.fill.is_some();
        ui.horizontal(|ui| {
            if ui.checkbox(&mut fill, "Solid background").changed() {
                pr.fill = if fill { Some([255, 255, 255]) } else { None };
            }
            if let Some(rgb) = &mut pr.fill {
                ui.color_edit_button_srgb(rgb);
            }
        });
        if ui.small_button("Reset to automatic").clicked() {
            *pr = Process::auto(&c.image);
        }

        ui.add_space(6.0);
        theme::section_heading(ui, "Save");
        ui.horizontal(|ui| {
            ui.label("Icons\\");
            ui.add(egui::TextEdit::singleline(&mut st.file_name).desired_width(180.0));
        });
        let file = st.file_name.trim().to_string();
        let valid = file.to_lowercase().ends_with(".png") && !file.contains(['\\', '/', ':']);
        let exists = app.ws_cache.icons.iter().any(|i| i.eq_ignore_ascii_case(&file));
        if !valid {
            ui.colored_label(theme::red(), "Use a plain file name ending in .png.");
        } else if exists {
            ui.colored_label(theme::amber(), "A file with this name exists and will be replaced.");
        }
        let writable = app.writable();
        if !writable {
            ui.label(RichText::new("Open a development folder to save icons (production is read-only).").small().weak());
        }
        let label = if exists { "\u{E74E}  Replace icon" } else { "\u{E74E}  Save icon" };
        let mut saved = None;
        if theme::primary(ui, valid && writable, RichText::new(label)).clicked() {
            let ws = app.ws.as_ref().unwrap();
            let path = ws.layout.icons.join(&file);
            let img = app.icons.preview.as_ref().unwrap().image.clone();
            match icons::save_png(&img, &path) {
                Ok(s) => saved = Some(s),
                Err(e) => app.error(format!("Could not save icon: {e:#}")),
            }
        }
        if let Some(s) = saved {
            app.notify(format!("Saved {} ({}×{}, {} KB).", s.path.display(), s.width, s.height, s.bytes.div_ceil(1024)));
            app.icons.last_saved = Some(file.clone());
            app.refresh_recipes();
            if let Some((batch_id, idx)) = app.icons.batch_item.clone() {
                super::batch::icon_saved(app, &batch_id, idx, &file);
            }
        }
        if app.icons.last_saved.as_deref() == Some(file.as_str())
            && let Some(id) = app.icons.recipe_id.clone()
        {
            let current = app.recipes.iter().find(|r| r.id == id).map(|r| r.meta.icon_file.clone()).unwrap_or_default();
            if current.eq_ignore_ascii_case(&file) {
                ui.colored_label(theme::green(), format!("\u{E73E}  {id} uses this icon."));
            } else if ui.add_enabled(writable, egui::Button::new(format!("Set iconFile: {file} in {id}"))).clicked() {
                set_icon_file(app, &id, &file);
            }
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!("Saved PNGs are at most {} px and {} KB, as Intune expects.", icons::MAX_SIDE, icons::MAX_BYTES / 1024))
                .small()
                .weak(),
        );
    });
}

/// Writes `iconFile` into a recipe, through the open editor buffer when it is open.
fn set_icon_file(app: &mut StudioApp, id: &str, file: &str) {
    if let Some(r) = app.recipe.as_mut()
        && r.id == id
    {
        if r.read_only {
            app.error("The open recipe is read-only.");
            return;
        }
        r.text = ystudio_core::recipe::set_top_level_scalar(&r.text, "iconFile", file);
        r.changed();
        app.notify(format!("Set iconFile in {id} - save the recipe to keep it."));
        return;
    }
    let Some(path) = app.recipes.iter().find(|r| r.id == id).map(|r| r.path.clone()) else { return };
    let result = ystudio_core::read_text(&path)
        .map_err(anyhow::Error::from)
        .and_then(|t| Ok(std::fs::write(&path, ystudio_core::recipe::set_top_level_scalar(&t, "iconFile", file))?));
    match result {
        Ok(()) => {
            app.notify(format!("Set iconFile: {file} in {}.", path.display()));
            app.refresh_recipes();
        }
        Err(e) => app.error(format!("Could not update {}: {e:#}", path.display())),
    }
}
