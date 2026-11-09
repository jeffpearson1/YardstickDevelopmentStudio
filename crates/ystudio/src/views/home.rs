//! Home: what to do next - continue a request, import one, write a new
//! recipe, edit one, or update several - plus the workspace and creating a
//! development clone.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::intake::Batch;
use ystudio_core::{git, harness};
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
        ui.label(RichText::new("Design, test and agentically author Yardstick recipes - without ever touching production.").weak());
        ui.add_space(20.0);
        if app.ws.is_some() {
            launcher(app, ui);
            ui.add_space(theme::CARD_GAP);
        }
        workspace_card(app, ui);
        ui.add_space(theme::CARD_GAP);
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

const ACTIONS: [(&str, &str, &str); 5] = [
    (theme::icon::INBOX, "Continue", "Pick up a request where you left off."),
    (theme::icon::UPLOAD, "Import requests", "A ticket export, spreadsheet, email or pasted list. The Studio finds the software in it and matches existing recipes."),
    (theme::icon::ROBOT, "New recipe", "Name the app: the agent researches, writes, tests and repairs it. Or start from a template yourself."),
    (theme::icon::EDIT, "Edit a recipe", ""),
    (theme::icon::LIBRARY, "Update several recipes", "Re-test, repair or bring a set of recipes up to date together - for example after a vendor changes its downloads."),
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
                        app.library.new_open = true;
                    }
                }
            }
        },
    );
    ui.add_space(theme::CARD_GAP);
    theme::card_row(
        ui,
        app,
        "home-actions-2",
        2,
        |app, i, ui| {
            action_title(ui, i + 3);
            if i == 0 {
                edit_body(app, ui);
            }
        },
        |app, i, ui| {
            ui.add_space(GROUP_GAP - 4.0);
            if i == 0 {
                if ui.button("Browse all recipes").clicked() {
                    app.view = View::Recipes;
                }
            } else if ui.add_enabled(app.writable(), egui::Button::new("Choose recipes…")).clicked() {
                app.library.selecting = true;
                app.view = View::Recipes;
            }
        },
    );
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
    let resp = ui.add(egui::TextEdit::singleline(&mut app.home.edit_filter).hint_text("Find a recipe by id or name…").desired_width(f32::INFINITY));
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
        ui.label(RichText::new("No recipe matches.").weak());
    }
    if let Some(p) = open {
        app.request_open_recipe(p);
        app.view = View::Designer;
        app.home.edit_filter.clear();
    }
}

fn workspace_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Workspace");
        let mut open = None;
        let mut status_msg: Option<String> = None;
        match &app.ws {
            Some(ws) => {
                ui.horizontal(|ui| {
                    let color = match ws.kind {
                        WorkspaceKind::Development => theme::green(),
                        WorkspaceKind::Unconfigured => theme::amber(),
                        WorkspaceKind::Production { .. } => theme::red(),
                    };
                    theme::badge(ui, ws.kind.label(), color);
                    ui.label(RichText::new(ws.root.display().to_string()).monospace());
                });
                let dev = app.recipes.iter().filter(|r| r.location == ystudio_core::workspace::RecipeLocation::Development).count();
                ui.label(RichText::new(format!("{} recipes ({dev} in development), {} icons.", app.recipes.len(), app.ws_cache.icons.len())).weak());
                match &ws.kind {
                    WorkspaceKind::Production { .. } => {
                        ui.add_space(GROUP_GAP - 6.0);
                        super::production_banner(app, ui);
                    }
                    WorkspaceKind::Unconfigured => {
                        ui.add_space(GROUP_GAP - 6.0);
                        theme::banner(ui, theme::amber(), |ui| {
                            ui.label(RichText::new("Not set up for testing yet").semibold().color(theme::amber()));
                            ui.label(
                                "This clone has no Local\\preferences.yaml. Run Install-Yardstick -DevelopmentOnly to install \
                                 the PowerShell modules and mark it as development-only (no Intune connection).",
                            );
                            if ui.button("\u{E768}  Run Install-Yardstick -DevelopmentOnly (new console)").clicked() {
                                status_msg = Some(
                                    match harness::install_development_only(ws, &app.pwsh()).and_then(|s| ystudio_core::process::launch_detached(&s)) {
                                        Ok(()) => "Installer opened in a new console. Reload the workspace when it finishes.".to_string(),
                                        Err(e) => format!("{e:#}"),
                                    },
                                );
                            }
                        });
                    }
                    WorkspaceKind::Development => {}
                }
                if !app.ws_cache.has_skill {
                    ui.add_space(GROUP_GAP - 6.0);
                    theme::banner(ui, theme::amber(), |ui| {
                        ui.label("This folder has no .claude\\skills\\yardstick-recipes skill - pull the latest Yardstick to use the agent.");
                    });
                }
            }
            None => {
                if let Some(e) = &app.ws_error {
                    ui.colored_label(theme::red(), e);
                }
                ui.label(RichText::new("Open a Yardstick folder, or create a development clone below.").weak());
            }
        }
        ui.add_space(GROUP_GAP - 6.0);
        if app.ws.is_some() {
            super::git_bar(app, ui);
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("\u{E838}  Open folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().set_title("Open a Yardstick folder").pick_folder()
            {
                open = Some(p);
            }
            if let Some(ws) = &app.ws {
                if ui.button("\u{E72C}  Reload").clicked() {
                    open = Some(ws.root.clone());
                }
                if ui.button("\u{E8A7}  Show in Explorer").clicked() {
                    super::open_in_explorer(&ws.root);
                }
                if !ws.is_writable() && ui.button("Trust as development…").on_hover_text("Only for full installs you are certain are scratch machines.").clicked() {
                    app.settings_ui.add_trusted(&ws.root);
                    app.view = View::Settings;
                }
            }
        });
        let current = app.ws.as_ref().map(|w| w.root.clone());
        let recent: Vec<PathBuf> = app.settings.recent_workspaces.iter().filter(|p| current.as_ref() != Some(*p)).cloned().collect();
        if !recent.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Recent").weak());
                for p in recent {
                    if ui.link(p.display().to_string()).clicked() {
                        open = Some(p);
                    }
                }
            });
        }
        if let Some(msg) = status_msg {
            app.notify(msg);
        }
        if let Some(p) = open {
            app.open_workspace(&p);
        }
    });
}

fn create_body(app: &mut StudioApp, ui: &mut Ui) {
    theme::section_heading(ui, "Create a development workspace");
    ui.label(RichText::new(
        "Clones Yardstick from GitHub, copies icons from production (they are not in Git), then opens the clone. \
         Finish by running Install-Yardstick -DevelopmentOnly so tests run without an Intune connection.",
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

fn create_footer(app: &mut StudioApp, ui: &mut Ui) {
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
