//! Home: workspace status, the production guard, creating a development
//! workspace, environment checks and settings.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::AgentBackend;
use ystudio_core::{git, harness};
use ystudio_core::process::which;
use ystudio_core::settings::Settings;
use ystudio_core::workspace::WorkspaceKind;

use crate::app::{JobKind, StudioApp, View};
use crate::theme::{self, FluentText as _};

pub struct HomeState {
    pub clone_dest: String,
    pub clone_url: String,
    pub production_text: String,
    pub trusted_text: String,
    pub pwsh_text: String,
    tools: Option<Vec<(&'static str, Option<PathBuf>, &'static str)>>,
    /// Whether `clone_dest` is a production path, keyed by the inputs (canonicalizing hits the disk).
    prod_check: Option<(String, Vec<PathBuf>, bool)>,
}

impl HomeState {
    pub fn new(settings: &Settings) -> Self {
        let lines = |v: &[PathBuf]| v.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        let dest = settings
            .primary_production()
            .and_then(|p| p.parent())
            .map(|p| p.join("YardstickDev"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Intune\YardstickDev"));
        Self {
            clone_dest: dest.display().to_string(),
            clone_url: settings.repo_url.clone(),
            production_text: lines(&settings.production_paths),
            trusted_text: lines(&settings.trusted_dev_paths),
            pwsh_text: settings.powershell_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            tools: None,
            prod_check: None,
        }
    }
}

fn parse_lines(text: &str) -> Vec<PathBuf> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect()
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
        theme::card_row(
            ui,
            app,
            "home-setup",
            2,
            |app, i, ui| if i == 0 { create_body(app, ui) } else { environment_body(app, ui) },
            |app, i, ui| if i == 0 { create_footer(app, ui) } else { environment_footer(app, ui) },
        );
        ui.add_space(theme::CARD_GAP);
        settings_card(app, ui);
        ui.add_space(theme::CARD_GAP);
        theme::card(ui, |ui| super::update::card(app, ui));
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
                    app.home.trusted_text = format!("{}\n{}", app.home.trusted_text, ws.root.display()).trim().to_string();
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

/// Width of the label column in the settings form.
const LABEL_WIDTH: f32 = 200.0;

/// A form row: a fixed-width label aligned with the first line of its field.
fn form_row<R>(ui: &mut Ui, label: &str, hover: Option<&str>, field: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(LABEL_WIDTH, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(LABEL_WIDTH);
            let r = ui.label(label);
            if let Some(h) = hover {
                r.on_hover_text(h);
            }
        });
        ui.vertical(|ui| field(ui)).inner
    })
    .inner
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
                    // The PATH may have been refreshed to find git.
                    app.home.tools = None;
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

fn environment_body(app: &mut StudioApp, ui: &mut Ui) {
    theme::section_heading(ui, "Environment");
    let shell = app.pwsh();
    let tools = app.home.tools.get_or_insert_with(|| {
        vec![
            ("pwsh", which("pwsh"), "PowerShell 7 (preferred for the harness)"),
            ("powershell", which("powershell"), "Windows PowerShell 5.1"),
            ("git", which("git"), "Cloning development workspaces"),
            ("copilot", AgentBackend::CopilotCli.locate(), "GitHub Copilot CLI agent"),
            ("claude", AgentBackend::ClaudeCode.locate(), "Claude Code agent (optional)"),
            ("code", which("code"), "Open files in VS Code (optional)"),
        ]
    });
    let name_width = 120.0;
    let path_width = (ui.available_width() - name_width - 12.0).max(80.0);
    let row = |ui: &mut Ui, glyph: &str, glyph_color: egui::Color32, name: &str, hover: &str, value: RichText, full: Option<&str>| {
        ui.allocate_ui_with_layout(egui::vec2(name_width, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(name_width);
            ui.label(RichText::new(glyph).color(glyph_color));
            ui.label(name).on_hover_text(hover);
        });
        ui.allocate_ui_with_layout(egui::vec2(path_width, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let r = ui.add(egui::Label::new(value).truncate());
            if let Some(full) = full {
                r.on_hover_text(full);
            }
        });
        ui.end_row();
    };
    egui::Grid::new("env-grid").num_columns(2).spacing([12.0, 2.0]).min_col_width(0.0).show(ui, |ui| {
        for (name, path, what) in tools.iter() {
            match path {
                Some(p) => {
                    let full = p.display().to_string();
                    row(ui, "\u{E73E}", theme::green(), name, what, RichText::new(&full).small().monospace(), Some(&full));
                }
                None => row(ui, "\u{E711}", theme::grey(), name, what, RichText::new(format!("Not found - {what}")).small().weak(), None),
            }
        }
        let full = shell.display().to_string();
        row(ui, "\u{E756}", theme::grey(), "Harness shell", "The PowerShell the Studio runs the harness with", RichText::new(&full).small().monospace(), Some(&full));
    });
}

fn environment_footer(app: &mut StudioApp, ui: &mut Ui) {
    if ui
        .button("\u{E72C}  Re-check")
        .on_hover_text("Reloads PATH from Windows, so tools installed since the Studio started (e.g. Git from Company Portal) are found.")
        .clicked()
    {
        if let Err(e) = ystudio_core::process::refresh_path() {
            app.error(format!("Could not reload PATH: {e:#}"));
        }
        app.home.tools = None;
        app.refresh_git_status();
    }
}

fn settings_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Settings");
        ui.add_space(GROUP_GAP - 6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        form_row(ui, "Production folders", Some("Always read-only in the Studio; the first is the default promotion target."), |ui| {
            ui.add(egui::TextEdit::multiline(&mut app.home.production_text).desired_rows(2).desired_width(f32::INFINITY));
        });
        form_row(ui, "Trusted development folders", Some("Full installs you have confirmed are scratch (rare)."), |ui| {
            ui.add(egui::TextEdit::multiline(&mut app.home.trusted_text).desired_rows(2).desired_width(f32::INFINITY));
        });
        form_row(ui, "PowerShell executable", None, |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.home.pwsh_text).hint_text("auto (pwsh, then powershell)").desired_width(f32::INFINITY));
        });
        form_row(ui, "Theme", None, |ui| {
            ui.horizontal(|ui| {
                let before = app.settings.dark_mode;
                ui.selectable_value(&mut app.settings.dark_mode, true, "\u{E708}  Dark");
                ui.selectable_value(&mut app.settings.dark_mode, false, "\u{E706}  Light");
                if before != app.settings.dark_mode {
                    app.apply_theme(ui.ctx());
                }
            });
        });
        form_row(ui, "Editor font size", None, |ui| {
            ui.add(egui::Slider::new(&mut app.settings.editor_font_size, 10.0..=22.0));
        });
        form_row(ui, "Editors", None, |ui| {
            ui.checkbox(&mut app.settings.line_numbers, "Show line numbers");
        });
        form_row(ui, "On open and Refresh", Some("Production folders are never fetched or synced."), |ui| {
            ui.checkbox(&mut app.settings.auto_git_fetch, "git fetch, and show how far behind the clone is");
            ui.checkbox(&mut app.settings.auto_sync_shared, "Sync shared icons and recipe overrides (when syncSharedContent is on)");
        });
        ui.add_space(GROUP_GAP - 8.0);
        ui.horizontal(|ui| {
            if theme::primary(ui, true, "\u{E74E}  Apply settings").clicked() {
                let prod = parse_lines(&app.home.production_text);
                app.settings.production_paths = prod;
                app.settings.trusted_dev_paths = parse_lines(&app.home.trusted_text);
                let p = app.home.pwsh_text.trim();
                app.settings.powershell_path = (!p.is_empty()).then(|| PathBuf::from(p));
                app.settings.repo_url = app.home.clone_url.trim().to_string();
                app.save_settings();
                app.reload_workspace();
                app.notify("Settings saved.");
            }
            ui.label(RichText::new(format!("Stored in {}", Settings::default_path().display())).small().weak());
        });
    });
}
