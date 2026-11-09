//! Home: workspace status, the production guard, and creating a development
//! workspace.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
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
        journey(app, ui);
        ui.add_space(theme::CARD_GAP);
        workspace_card(app, ui);
        ui.add_space(theme::CARD_GAP);
        theme::card(ui, |ui| {
            create_body(app, ui);
            ui.add_space(GROUP_GAP - 6.0);
            create_footer(app, ui);
        });
        ui.add_space(8.0);
    });
}

const STEPS: [(&str, &str, &str, View); 4] = [
    ("1", "Design", "Step through every recipe field with guidance from the skill, or edit YAML with live lint.", View::Recipes),
    ("2", "Test", "Lint → schema → lab download → dry run → live install test, with verdicts.", View::TestBench),
    ("3", "Delegate", "Have Copilot CLI write, fix or review recipes using the yardstick-recipes skill.", View::Agent),
    ("4", "Promote", "Publish validated recipes to production with Publish-Yardstick.", View::Promote),
];

fn journey(app: &mut StudioApp, ui: &mut Ui) {
    theme::card_row(
        ui,
        app,
        "home-journey",
        STEPS.len(),
        |_, i, ui| {
            let (n, title, text, _) = STEPS[i];
            ui.horizontal(|ui| {
                theme::badge(ui, n, theme::accent());
                ui.label(RichText::new(title).size(16.0).semibold());
            });
            ui.add_space(2.0);
            ui.label(RichText::new(text).weak());
        },
        |app, i, ui| {
            let (_, title, _, view) = STEPS[i];
            ui.add_space(GROUP_GAP - 4.0);
            if ui.button(format!("Open {title}")).clicked() {
                app.view = view;
            }
        },
    );
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
