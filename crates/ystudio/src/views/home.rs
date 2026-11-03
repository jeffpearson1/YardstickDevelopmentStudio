//! Home: workspace status, the production guard, creating a development
//! workspace, environment checks and settings.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::AgentBackend;
use ystudio_core::harness;
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

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::page_title(ui, "Yardstick Development Studio");
        ui.label(RichText::new("Design, test and agentically author Yardstick recipes - without ever touching production.").weak());
        ui.add_space(16.0);
        journey(app, ui);
        ui.add_space(16.0);
        workspace_card(app, ui);
        ui.add_space(16.0);
        ui.columns(2, |cols| {
            create_card(app, &mut cols[0]);
            environment_card(app, &mut cols[1]);
        });
        ui.add_space(16.0);
        settings_card(app, ui);
    });
}

fn journey(app: &mut StudioApp, ui: &mut Ui) {
    let steps = [
        ("1", "Design", "Step through every recipe field with guidance from the skill, or edit YAML with live lint.", View::Recipes),
        ("2", "Test", "Lint → schema → lab download → dry run → live install test, with verdicts.", View::TestBench),
        ("3", "Delegate", "Have Copilot CLI write, fix or review recipes using the yardstick-recipes skill.", View::Agent),
        ("4", "Promote", "Publish validated recipes to production with Publish-Yardstick.", View::Promote),
    ];
    ui.columns(steps.len(), |cols| {
        for (col, (n, title, text, view)) in cols.iter_mut().zip(steps) {
            theme::card(col, |ui| {
                ui.horizontal(|ui| {
                    theme::badge(ui, n, theme::accent());
                    ui.label(RichText::new(title).semibold().size(16.0));
                });
                ui.label(RichText::new(text).weak());
                if ui.button(format!("Open {title} →")).clicked() {
                    app.view = view;
                }
            });
        }
    });
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
                    if ui.small_button("\u{E838}").on_hover_text("Open in Explorer").clicked() {
                        super::open_in_explorer(&ws.root);
                    }
                });
                let dev = app.recipes.iter().filter(|r| r.location == ystudio_core::workspace::RecipeLocation::Development).count();
                ui.label(format!("{} recipes ({dev} in development), {} icons.", app.recipes.len(), app.ws_cache.icons.len()));
                match &ws.kind {
                    WorkspaceKind::Production { .. } => {
                        ui.add_space(4.0);
                        super::production_banner(app, ui);
                    }
                    WorkspaceKind::Unconfigured => {
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
                    theme::banner(ui, theme::amber(), |ui| {
                        ui.label("This folder has no .claude\\skills\\yardstick-recipes skill - pull the latest Yardstick to use the agent.");
                    });
                }
                ui.horizontal(|ui| {
                    if ui.button("\u{E72C}  Reload").clicked() {
                        open = Some(ws.root.clone());
                    }
                    if !ws.is_writable() && ui.button("Trust as development…").on_hover_text("Only for full installs you are certain are scratch machines.").clicked() {
                        app.home.trusted_text = format!("{}\n{}", app.home.trusted_text, ws.root.display()).trim().to_string();
                    }
                });
            }
            None => {
                if let Some(e) = &app.ws_error {
                    ui.colored_label(theme::red(), e);
                }
                ui.label("Open a Yardstick folder, or create a development clone below.");
            }
        }
        ui.horizontal(|ui| {
            if ui.button("\u{E838}  Open folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().set_title("Open a Yardstick folder").pick_folder()
            {
                open = Some(p);
            }
            if !app.settings.recent_workspaces.is_empty() {
                ui.label("Recent:");
                for p in app.settings.recent_workspaces.clone() {
                    if ui.link(p.display().to_string()).clicked() {
                        open = Some(p);
                    }
                }
            }
        });
        if let Some(msg) = status_msg {
            app.notify(msg);
        }
        if let Some(p) = open {
            app.open_workspace(&p);
        }
    });
}

fn create_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Create a development workspace");
        ui.label(RichText::new(
            "Clones Yardstick from GitHub, copies icons from production (they are not in Git), then opens the clone. \
             Finish by running Install-Yardstick -DevelopmentOnly so tests run without an Intune connection.",
        ).weak());
        egui::Grid::new("clone-grid").num_columns(2).show(ui, |ui| {
            ui.label("Repository");
            ui.add(egui::TextEdit::singleline(&mut app.home.clone_url).desired_width(ui.available_width()));
            ui.end_row();
            ui.label("Destination");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.home.clone_dest).desired_width(ui.available_width() - 40.0));
                if ui.button("…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    app.home.clone_dest = p.join("YardstickDev").display().to_string();
                }
            });
            ui.end_row();
        });
        let dest = PathBuf::from(app.home.clone_dest.trim());
        let stale = app.home.prod_check.as_ref().is_none_or(|(d, prods, _)| *d != app.home.clone_dest || *prods != app.settings.production_paths);
        if stale {
            let is_prod = app.settings.production_paths.iter().any(|p| ystudio_core::same_path(p, &dest));
            app.home.prod_check = Some((app.home.clone_dest.clone(), app.settings.production_paths.clone(), is_prod));
        }
        let is_prod = app.home.prod_check.as_ref().is_some_and(|c| c.2);
        let cloning = app.jobs.iter().any(|j| matches!(j.kind, JobKind::Clone { .. }) && j.proc.is_running());
        ui.add_enabled_ui(!is_prod && !cloning, |ui| {
            if ui.button("\u{E896}  Clone development workspace").clicked() {
                match harness::git_clone(app.home.clone_url.trim(), &dest) {
                    Ok(spec) => {
                        app.console_open = true;
                        app.start_job(spec, JobKind::Clone { dest });
                    }
                    Err(e) => app.error(format!("{e:#}")),
                }
            }
        });
        if is_prod {
            ui.colored_label(theme::red(), "That is a production path.");
        }
        if cloning {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Cloning…");
            });
        }
    });
}

fn environment_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Environment");
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
        let path_width = (ui.available_width() - 130.0).max(120.0);
        egui::Grid::new("env-grid").num_columns(2).striped(true).show(ui, |ui| {
            for (name, path, what) in tools.iter() {
                match path {
                    Some(p) => {
                        ui.colored_label(theme::green(), format!("\u{E73E}  {name}")).on_hover_text(*what);
                        let full = p.display().to_string();
                        ui.allocate_ui(egui::vec2(path_width, 18.0), |ui| {
                            ui.add(egui::Label::new(RichText::new(&full).small().monospace()).truncate()).on_hover_text(&full);
                        });
                    }
                    None => {
                        ui.colored_label(theme::grey(), format!("\u{E711}  {name}")).on_hover_text(*what);
                        ui.label(RichText::new(format!("not found - {what}")).small().weak());
                    }
                }
                ui.end_row();
            }
        });
        let shell = format!("Harness shell: {}", app.pwsh().display());
        ui.add(egui::Label::new(RichText::new(&shell).small()).truncate()).on_hover_text(&shell);
        if ui.small_button("Re-check").clicked() {
            app.home.tools = None;
        }
    });
}

fn settings_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Settings");
        egui::Grid::new("settings-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Production folders").on_hover_text("Always read-only in the Studio; the first is the default promotion target.");
            ui.add(egui::TextEdit::multiline(&mut app.home.production_text).desired_rows(2).desired_width(ui.available_width()));
            ui.end_row();
            ui.label("Trusted development folders").on_hover_text("Full installs you have confirmed are scratch (rare).");
            ui.add(egui::TextEdit::multiline(&mut app.home.trusted_text).desired_rows(2).desired_width(ui.available_width()));
            ui.end_row();
            ui.label("PowerShell executable");
            ui.add(egui::TextEdit::singleline(&mut app.home.pwsh_text).hint_text("auto (pwsh, then powershell)").desired_width(ui.available_width()));
            ui.end_row();
            ui.label("Theme");
            ui.horizontal(|ui| {
                let before = app.settings.dark_mode;
                ui.selectable_value(&mut app.settings.dark_mode, true, "\u{E708}  Dark");
                ui.selectable_value(&mut app.settings.dark_mode, false, "\u{E706}  Light");
                if before != app.settings.dark_mode {
                    app.apply_theme(ui.ctx());
                }
            });
            ui.end_row();
            ui.label("Editor font size");
            ui.add(egui::Slider::new(&mut app.settings.editor_font_size, 10.0..=22.0));
            ui.end_row();
        });
        if ui.button("\u{E74E}  Apply settings").clicked() {
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
}
