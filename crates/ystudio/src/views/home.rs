//! Home: what to do next - continue a request, import one, write a new
//! recipe, edit one, or update several - plus the workspace and creating a
//! development clone.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::intake::Batch;
use ystudio_core::git;
use ystudio_core::settings::Settings;
use ystudio_core::workspace::WorkspaceKind;

use crate::app::{JobKind, StudioApp, View};
use crate::theme::{self, FluentText as _};

pub struct HomeState {
    pub clone_dest: String,
    pub clone_url: String,
    /// Whether `clone_dest` is a production path, keyed by the inputs (canonicalizing hits the disk).
    prod_check: Option<(String, Vec<PathBuf>, bool)>,
    /// "Edit a recipe" search text.
    edit_filter: String,
    /// Saved requests with work left, re-read every few seconds.
    recent: Option<(Instant, Vec<Recent>)>,
}

struct Recent {
    dir: PathBuf,
    label: String,
    open: usize,
    total: usize,
}

impl HomeState {
    pub fn new(settings: &Settings) -> Self {
        let dest = settings
            .primary_production()
            .and_then(|p| p.parent())
            .map(|p| p.join("YardstickDev"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Intune\YardstickDev"));
        Self {
            clone_dest: dest.display().to_string(),
            clone_url: settings.repo_url.clone(),
            prod_check: None,
            edit_filter: String::new(),
            recent: None,
        }
    }
}

/// Vertical rhythm inside a card: heading to content, and content to actions.
const GROUP_GAP: f32 = 12.0;

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::page_title(ui, "Yardstick Development Studio");
        ui.add_space(16.0);
        super::tools::banner(app, ui);
        // Only what blocks work here; the workspace itself is in the status bar.
        if workspace_notices(app, ui) {
            ui.add_space(theme::CARD_GAP);
        }
        if plugin_activity_card(app, ui) {
            ui.add_space(theme::CARD_GAP);
        }
        if app.ws.is_some() {
            launcher(app, ui);
            ui.add_space(theme::CARD_GAP);
        }
        if app.ws.is_none() {
            no_workspace(app, ui);
            ui.add_space(theme::CARD_GAP);
        }
        if app.writable() {
            egui::CollapsingHeader::new("Create another development workspace").id_salt("home-create").show(ui, |ui| {
                theme::card(ui, |ui| {
                    create_body(app, ui);
                    ui.add_space(GROUP_GAP - 6.0);
                    create_footer(app, ui);
                });
            });
        } else {
            theme::card(ui, |ui| {
                create_body(app, ui);
                ui.add_space(GROUP_GAP - 6.0);
                create_footer(app, ui);
            });
        }
        ui.add_space(8.0);
    });
}

/// Work that would otherwise require visiting each plugin tab to discover.
fn plugin_activity_card(app: &mut StudioApp, ui: &mut Ui) -> bool {
    super::plugin_page::refresh_activity(app, false);
    let mut active: Vec<_> = app
        .plugins
        .activity
        .iter()
        .filter(|activity| activity.state != super::plugin_page::PluginActivityState::Complete)
        .cloned()
        .collect();
    if active.is_empty() {
        return false;
    }
    active.sort_by_key(|activity| match activity.state {
        super::plugin_page::PluginActivityState::Attention => 0,
        super::plugin_page::PluginActivityState::Waiting => 1,
        super::plugin_page::PluginActivityState::Running => 2,
        super::plugin_page::PluginActivityState::Ready => 3,
        super::plugin_page::PluginActivityState::Complete => 4,
    });
    let attention = active.iter().filter(|activity| activity.state.needs_attention()).count();
    let running = active
        .iter()
        .filter(|activity| activity.state == super::plugin_page::PluginActivityState::Running)
        .count();
    let mut open = None;
    let mut view_all = false;
    theme::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            theme::section_heading(ui, "Plugin work");
            if attention > 0 {
                theme::badge(ui, format!("{attention} need attention"), theme::amber());
            }
            if running > 0 {
                theme::badge(ui, format!("{running} running"), theme::blue());
            }
        });
        ui.label(RichText::new("Continue interrupted, waiting and active plugin work without searching each plugin tab.").weak());
        ui.add_space(8.0);
        for activity in active.iter().take(3) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&activity.title).semibold());
                theme::badge(ui, activity.state.label(), plugin_activity_color(activity.state));
                ui.label(RichText::new(format!("{} · {}/{} stages", activity.plugin_name, activity.done, activity.total)).small().weak());
                if ui.small_button("Open").clicked() {
                    open = Some(activity.clone());
                }
            });
            if !activity.note.is_empty() {
                ui.label(RichText::new(&activity.note).small().weak());
            }
        }
        ui.add_space(6.0);
        view_all = ui.button(format!("View all plugin work ({})", active.len())).clicked();
    });
    if let Some(activity) = open {
        super::plugin_page::open_activity(app, &activity);
    } else if view_all {
        app.runs_ui.show_plugin_work();
        app.view = View::Runs;
    }
    true
}

pub(crate) fn plugin_activity_color(state: super::plugin_page::PluginActivityState) -> egui::Color32 {
    match state {
        super::plugin_page::PluginActivityState::Running => theme::blue(),
        super::plugin_page::PluginActivityState::Waiting => theme::amber(),
        super::plugin_page::PluginActivityState::Attention => theme::red(),
        super::plugin_page::PluginActivityState::Ready => theme::grey(),
        super::plugin_page::PluginActivityState::Complete => theme::green(),
    }
}

/// A request's name: the file it came from, or what the Studio called it.
pub fn request_label(b: &Batch) -> String {
    b.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| b.id.clone())
}

/// Up to three other saved requests that still have items to do, newest first.
fn recent_requests(app: &mut StudioApp) -> &[Recent] {
    let stale = app.home.recent.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(3));
    if stale {
        let current = app.batch.batch.as_ref().map(|b| b.id.clone());
        let list = Batch::list()
            .into_iter()
            .filter(|(name, _)| current.as_deref() != Some(name.as_str()))
            .filter_map(|(_, dir)| {
                let b = Batch::load(&dir).ok()?;
                let open = b.open_items();
                (open > 0).then(|| Recent { label: request_label(&b), open, total: b.items.len(), dir })
            })
            .take(3)
            .collect();
        app.home.recent = Some((Instant::now(), list));
    }
    &app.home.recent.as_ref().unwrap().1
}

const ACTIONS: [(&str, &str, &str); 3] = [
    (theme::icon::INBOX, "Continue", ""),
    (theme::icon::UPLOAD, "Import requests", "A ticket export, spreadsheet, email or pasted list."),
    (theme::icon::ROBOT, "New recipe", "The agent researches, writes, tests and repairs it."),
];

fn action_title(ui: &mut Ui, i: usize) {
    let (icon, title, text) = ACTIONS[i];
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).family(egui::FontFamily::Name(theme::ICONS.into())).size(18.0).color(theme::accent()));
        ui.label(RichText::new(title).size(16.0).semibold());
    });
    if !text.is_empty() {
        ui.add_space(2.0);
        ui.label(RichText::new(text).weak());
    }
}

fn launcher(app: &mut StudioApp, ui: &mut Ui) {
    theme::card_row(
        ui,
        app,
        "home-actions-1",
        3,
        |app, i, ui| {
            action_title(ui, i);
            if i == 0 {
                continue_body(app, ui);
            }
        },
        |app, i, ui| {
            ui.add_space(GROUP_GAP - 4.0);
            let busy = app.batch.busy(app);
            match i {
                0 => {
                    let newest = app.home.recent.as_ref().and_then(|(_, v)| v.first()).map(|r| r.dir.clone());
                    let current = app.batch.batch.is_some();
                    if ui.add_enabled(current || newest.is_some(), egui::Button::new("Continue")).clicked() {
                        match newest.filter(|_| !current) {
                            Some(dir) if !busy => {
                                super::batch::open_saved(app, &dir);
                                app.home.recent = None;
                            }
                            _ => {}
                        }
                        app.view = View::Batch;
                    }
                }
                1 => {
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(!busy, egui::Button::new("\u{E838}  Open a file…")).clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .add_filter("Request lists", ystudio_core::intake::EXTENSIONS)
                                .add_filter("All files", &["*"])
                                .pick_file()
                        {
                            super::batch::open_file(app, &p);
                            app.view = View::Batch;
                        }
                        if ui.add_enabled(!busy, egui::Button::new("\u{E77F}  Paste text…")).clicked() {
                            app.batch.paste = Some(String::new());
                        }
                    });
                }
                _ => {
                    if theme::primary(ui, app.writable(), "\u{E710}  New recipe…").clicked() {
                        super::batch::show_new_recipe(app);
                    }
                }
            }
        },
    );
    // Opening one recipe needs no card: a search field under the actions. The
    // Library (and its Update several…) is a click away in the sidebar.
    ui.add_space(theme::CARD_GAP);
    edit_body(app, ui);
}

fn continue_body(app: &mut StudioApp, ui: &mut Ui) {
    ui.add_space(4.0);
    if let Some(b) = &app.batch.batch {
        ui.label(RichText::new(format!("Open: {}", request_label(b))).semibold());
        ui.label(RichText::new(format!("{} of {} item(s) still to do", b.open_items(), b.items.len())).small().weak());
    }
    let mut open = None;
    for r in recent_requests(app) {
        if ui.link(format!("{}  ·  {} of {} to do", r.label, r.open, r.total)).clicked() {
            open = Some(r.dir.clone());
        }
    }
    if app.batch.batch.is_none() && app.home.recent.as_ref().is_some_and(|(_, v)| v.is_empty()) {
        ui.label(RichText::new("Nothing in progress.").weak());
    }
    if let Some(dir) = open {
        if app.batch.busy(app) {
            app.error("Finish or cancel the active run before switching requests.");
        } else {
            super::batch::open_saved(app, &dir);
            app.view = View::Batch;
            app.home.recent = None;
        }
    }
}

fn edit_body(app: &mut StudioApp, ui: &mut Ui) {
    ui.add_space(4.0);
    let resp = ui.add(egui::TextEdit::singleline(&mut app.home.edit_filter).hint_text("\u{E721}  Open a recipe by id or name…").desired_width(f32::INFINITY));
    let needle = app.home.edit_filter.trim().to_lowercase();
    if needle.is_empty() {
        return;
    }
    let matches: Vec<(String, PathBuf)> = app
        .recipes
        .iter()
        .filter(|r| r.id.to_lowercase().contains(&needle) || r.meta.display_name.to_lowercase().contains(&needle))
        .take(6)
        .map(|r| (r.id.clone(), r.path.clone()))
        .collect();
    let mut open = None;
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        open = matches.first().map(|m| m.1.clone());
    }
    for (id, path) in &matches {
        ui.horizontal(|ui| {
            if ui.link(RichText::new(id).monospace()).clicked() {
                open = Some(path.clone());
            }
            super::recipe_status_badge(app, ui, id);
        });
    }
    if matches.is_empty() {
        let text = if app.scanning() { "Loading recipes…" } else { "No recipe matches." };
        ui.label(RichText::new(text).weak());
    }
    if let Some(p) = open {
        app.request_open_recipe(p);
        app.view = View::Designer;
        app.home.edit_filter.clear();
    }
}

/// What stops work in this workspace (production, not set up, no agent skill),
/// as info bars. Returns whether anything was shown.
fn workspace_notices(app: &mut StudioApp, ui: &mut Ui) -> bool {
    let Some(ws) = &app.ws else { return false };
    let mut shown = false;
    let mut setup = false;
    match &ws.kind {
        WorkspaceKind::Production { .. } => {
            super::production_banner(app, ui);
            shown = true;
        }
        WorkspaceKind::Unconfigured => {
            theme::banner(ui, theme::amber(), |ui| {
                ui.label(RichText::new("Not set up for testing yet").semibold().color(theme::amber()));
                ui.label(
                    "This clone has no Local\\preferences.yaml yet. Setup marks it development-only (no Intune \
                     connection) and installs what the recipe harness needs.",
                );
                if ui.button("\u{E768}  Set up this clone…").clicked() {
                    setup = true;
                }
            });
            shown = true;
        }
        WorkspaceKind::Development => {}
    }
    if !app.ws_cache.has_skill {
        if shown {
            ui.add_space(GROUP_GAP - 6.0);
        }
        theme::banner(ui, theme::amber(), |ui| {
            ui.label("This folder has no .claude\\skills\\yardstick-recipes skill - pull the latest Yardstick to use the agent.");
        });
        shown = true;
    }
    if setup {
        super::setup::open(app, super::setup::Step::Yardstick);
    }
    shown
}

/// With no workspace open, Home leads with opening one.
fn no_workspace(app: &mut StudioApp, ui: &mut Ui) {
    let mut open = None;
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Open a Yardstick folder");
        if let Some(e) = &app.ws_error {
            ui.colored_label(theme::red(), e);
        }
        ui.label(RichText::new("Open an existing clone, or create a development clone below.").weak());
        ui.add_space(GROUP_GAP - 4.0);
        ui.horizontal_wrapped(|ui| {
            if theme::primary(ui, true, "\u{E838}  Open folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().set_title("Open a Yardstick folder").pick_folder()
            {
                open = Some(p);
            }
            for p in app.settings.recent_workspaces.iter() {
                if ui.link(p.display().to_string()).clicked() {
                    open = Some(p.clone());
                }
            }
        });
    });
    if let Some(p) = open {
        app.open_workspace(&p);
    }
}

/// The status bar's workspace menu: everything about the folder itself, so no
/// page has to carry a workspace card.
pub fn workspace_menu(app: &mut StudioApp, ui: &mut Ui) {
    let mut open = None;
    let mut sync = false;
    if ui.button("\u{E838}  Open folder…").clicked() {
        ui.close();
        if let Some(p) = rfd::FileDialog::new().set_title("Open a Yardstick folder").pick_folder() {
            open = Some(p);
        }
    }
    let current = app.ws.as_ref().map(|w| w.root.clone());
    let recent: Vec<PathBuf> = app.settings.recent_workspaces.iter().filter(|p| current.as_ref() != Some(*p)).cloned().collect();
    if !recent.is_empty() {
        ui.menu_button("\u{E81C}  Recent", |ui| {
            for p in recent {
                if ui.button(p.display().to_string()).clicked() {
                    open = Some(p);
                    ui.close();
                }
            }
        });
    }
    if let Some(ws) = &app.ws {
        ui.separator();
        let dev = app.recipes.iter().filter(|r| r.location == ystudio_core::workspace::RecipeLocation::Development).count();
        ui.label(RichText::new(format!("{} recipes ({dev} in development) · {} icons", app.recipes.len(), app.ws_cache.icons.len())).small().weak());
        if ui.button("\u{E72C}  Reload").clicked() {
            open = Some(ws.root.clone());
            ui.close();
        }
        if ui.button("\u{E8A7}  Show in Explorer").clicked() {
            super::open_in_explorer(&ws.root);
            ui.close();
        }
        let syncing = app.jobs.iter().any(|j| matches!(j.kind, JobKind::SyncShared) && j.proc.is_running());
        if ws.is_writable()
            && ws.sync_shared_content
            && ui
                .add_enabled(!syncing, egui::Button::new("\u{E895}  Sync icons & overrides"))
                .on_hover_text("Yardstick's shared-content sync (SharedIcons / SharedRecipeOverrides in preferences.yaml)")
                .clicked()
        {
            sync = true;
            ui.close();
        }
        if !ws.is_writable()
            && ui.button("Trust as development…").on_hover_text("Only for full installs you are certain are scratch machines.").clicked()
        {
            app.settings_ui.add_trusted(&ws.root);
            app.view = View::Settings;
            ui.close();
        }
    }
    if sync {
        app.start_shared_sync(true);
    }
    if let Some(p) = open {
        app.open_workspace(&p);
    }
}

pub(crate) fn create_body(app: &mut StudioApp, ui: &mut Ui) {
    theme::section_heading(ui, "Create a development workspace");
    ui.label(RichText::new(
        "Clones Yardstick from GitHub, copies icons from production (they are not in Git), then opens the clone \
         and sets it up for development, so tests run without an Intune connection.",
    ).weak());
    ui.add_space(GROUP_GAP - 6.0);
    egui::Grid::new("clone-grid").num_columns(2).spacing([12.0, 8.0]).min_col_width(0.0).show(ui, |ui| {
        ui.label("Repository");
        ui.add(egui::TextEdit::singleline(&mut app.home.clone_url).desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Destination");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                app.home.clone_dest = p.join("YardstickDev").display().to_string();
            }
            ui.add(egui::TextEdit::singleline(&mut app.home.clone_dest).desired_width(f32::INFINITY));
        });
        ui.end_row();
    });
}

pub(crate) fn create_footer(app: &mut StudioApp, ui: &mut Ui) {
    let dest = PathBuf::from(app.home.clone_dest.trim());
    let stale = app.home.prod_check.as_ref().is_none_or(|(d, prods, _)| *d != app.home.clone_dest || *prods != app.settings.production_paths);
    if stale {
        let is_prod = app.settings.production_paths.iter().any(|p| ystudio_core::same_path(p, &dest));
        app.home.prod_check = Some((app.home.clone_dest.clone(), app.settings.production_paths.clone(), is_prod));
    }
    let is_prod = app.home.prod_check.as_ref().is_some_and(|c| c.2);
    let cloning = app.jobs.iter().any(|j| matches!(j.kind, JobKind::Clone { .. }) && j.proc.is_running());
    ui.horizontal(|ui| {
        if ui.add_enabled(!is_prod && !cloning, egui::Button::new("\u{E896}  Clone development workspace")).clicked() {
            match git::clone(app.home.clone_url.trim(), &dest) {
                Ok(spec) => {
                    app.console_open = true;
                    app.start_job(spec, JobKind::Clone { dest });
                    app.settings.repo_url = app.home.clone_url.trim().to_string();
                    app.save_settings();
                    // The PATH may have been refreshed to find git.
                    app.settings_ui.tools = None;
                }
                Err(e) => app.error(format!("{e:#}")),
            }
        }
        if is_prod {
            ui.colored_label(theme::red(), "That is a production path.");
        }
        if cloning {
            ui.spinner();
            ui.label("Cloning…");
        }
    });
}
