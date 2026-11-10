//! Settings: workspace safety, tools and agent, appearance, refresh behaviour
//! and updates. Most settings apply and save as they change; the folder lists
//! reload the workspace, so they wait for Apply.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::AgentBackend;
use ystudio_core::process::which;
use ystudio_core::settings::Settings;

use crate::app::StudioApp;
use crate::theme;

pub struct SettingsState {
    production_text: String,
    trusted_text: String,
    pwsh_text: String,
    pub tools: Option<Vec<(&'static str, Option<PathBuf>, &'static str)>>,
    /// Models each agent CLI accepts, listed in the background.
    models: HashMap<AgentBackend, Models>,
    /// "Other…" was picked: the model is typed in.
    custom_model: bool,
}

enum Models {
    Loading(Receiver<Result<Vec<String>, String>>),
    Ready(Vec<String>),
    Failed(String),
}

impl SettingsState {
    pub fn new(settings: &Settings) -> Self {
        let mut s = Self {
            production_text: String::new(),
            trusted_text: String::new(),
            pwsh_text: String::new(),
            tools: None,
            models: HashMap::new(),
            custom_model: false,
        };
        s.sync(settings);
        s
    }

    /// Re-reads the text fields after the settings changed elsewhere.
    pub fn sync(&mut self, settings: &Settings) {
        let lines = |v: &[PathBuf]| v.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        self.production_text = lines(&settings.production_paths);
        self.trusted_text = lines(&settings.trusted_dev_paths);
        self.pwsh_text = settings.powershell_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    }

    /// Proposes trusting `root`; the user confirms with Apply.
    pub fn add_trusted(&mut self, root: &std::path::Path) {
        self.trusted_text = format!("{}\n{}", self.trusted_text, root.display()).trim().to_string();
    }

    /// The models `backend` accepts; starts listing them on first use.
    fn models(&mut self, backend: AgentBackend, waker: &ystudio_core::process::Waker) -> &Models {
        if let Some(Models::Loading(rx)) = self.models.get(&backend)
            && let Ok(result) = rx.try_recv()
        {
            self.models.insert(backend, result.map_or_else(Models::Failed, Models::Ready));
        }
        self.models.entry(backend).or_insert_with(|| {
            let (tx, rx) = channel();
            let waker = waker.clone();
            std::thread::spawn(move || {
                let _ = tx.send(backend.models().map_err(|e| format!("{e:#}")));
                waker();
            });
            Models::Loading(rx)
        })
    }

    fn folders_dirty(&self, settings: &Settings) -> bool {
        parse_lines(&self.production_text) != settings.production_paths || parse_lines(&self.trusted_text) != settings.trusted_dev_paths
    }
}

fn parse_lines(text: &str) -> Vec<PathBuf> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect()
}

/// Vertical rhythm inside a card: heading to content, and content to actions.
const GROUP_GAP: f32 = 12.0;
/// Width of the label column.
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

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::page_title(ui, "Settings");
        ui.label(RichText::new(format!("Changes are saved as you make them, in {}.", Settings::default_path().display())).weak());
        ui.add_space(16.0);
        theme::card(ui, |ui| super::update::card(app, ui));
        ui.add_space(theme::CARD_GAP);
        folders_card(app, ui);
        ui.add_space(theme::CARD_GAP);
        tools_card(app, ui);
        ui.add_space(theme::CARD_GAP);
        appearance_card(app, ui);
        ui.add_space(theme::CARD_GAP);
        theme::card(ui, |ui| {
            theme::section_heading(ui, "Setup");
            ui.label("Walks through the workspace, the tools and Yardstick's development-only setup again.");
            ui.add_space(6.0);
            if ui.button("\u{E768}  Run setup again…").clicked() {
                super::setup::open(app, super::setup::Step::Workspace);
            }
        });
        ui.add_space(8.0);
    });
}

fn folders_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Production safety");
        ui.add_space(GROUP_GAP - 6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        let st = &mut app.settings_ui;
        form_row(ui, "Production folders", Some("Always read-only in the Studio; one per line."), |ui| {
            ui.add(egui::TextEdit::multiline(&mut st.production_text).desired_rows(2).desired_width(f32::INFINITY));
        });
        form_row(ui, "Trusted development folders", Some("Full installs you have confirmed are scratch (rare); one per line."), |ui| {
            ui.add(egui::TextEdit::multiline(&mut st.trusted_text).desired_rows(2).desired_width(f32::INFINITY));
        });
        let dirty = st.folders_dirty(&app.settings);
        ui.horizontal(|ui| {
            if theme::primary(ui, dirty, "\u{E74E}  Apply").on_hover_text("Saves the folders and reloads the workspace.").clicked() {
                app.settings.production_paths = parse_lines(&app.settings_ui.production_text);
                app.settings.trusted_dev_paths = parse_lines(&app.settings_ui.trusted_text);
                app.save_settings();
                app.reload_workspace();
                app.notify("Folders saved.");
            }
            if dirty && ui.button("Discard").clicked() {
                app.settings_ui.sync(&app.settings);
            }
        });
    });
}

fn tools_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Tools");
        ui.add_space(GROUP_GAP - 6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        let mut save = false;
        form_row(ui, "Agent and model", Some("The coding-agent CLI that writes and repairs recipes, and the model it uses."), |ui| {
            agent_picker(app, ui, "settings");
        });
        form_row(ui, "PowerShell", Some("The PowerShell the harness runs with; blank picks pwsh 7, then Windows PowerShell."), |ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut app.settings_ui.pwsh_text).hint_text("automatic").desired_width(f32::INFINITY));
            if r.lost_focus() {
                let p = app.settings_ui.pwsh_text.trim();
                app.settings.powershell_path = (!p.is_empty()).then(|| PathBuf::from(p));
                save = true;
            }
        });
        if save {
            app.save_settings();
        }
        ui.add_space(GROUP_GAP - 6.0);
        environment(app, ui);
    });
}

fn environment(app: &mut StudioApp, ui: &mut Ui) {
    let shell = app.pwsh();
    let tools = app.settings_ui.tools.get_or_insert_with(|| {
        vec![
            ("pwsh", which("pwsh"), "PowerShell 7 (preferred for the harness)"),
            ("powershell", which("powershell"), "Windows PowerShell 5.1"),
            ("git", which("git"), "Cloning and updating development workspaces"),
            ("gh", which("gh"), "Opening pull requests (optional)"),
            ("copilot", AgentBackend::CopilotCli.locate(), "GitHub Copilot CLI agent"),
            ("claude", AgentBackend::ClaudeCode.locate(), "Claude Code agent (optional)"),
            ("code", which("code"), "Open files in VS Code (optional)"),
        ]
    });
    let name_width = LABEL_WIDTH - 12.0;
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
    ui.add_space(GROUP_GAP - 6.0);
    if ui
        .button("\u{E72C}  Re-check")
        .on_hover_text("Reloads PATH from Windows and looks in the usual install folders, so tools installed since the Studio started are found.")
        .clicked()
    {
        if let Err(e) = ystudio_core::process::refresh_path() {
            app.error(format!("Could not reload PATH: {e:#}"));
        }
        app.settings_ui.tools = None;
        app.agent.backends_found = None;
        app.refresh_git_status();
    }
}

fn appearance_card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Appearance and behaviour");
        ui.add_space(GROUP_GAP - 6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        let mut save = false;
        form_row(ui, "Theme", None, |ui| {
            ui.horizontal(|ui| {
                let before = app.settings.dark_mode;
                ui.selectable_value(&mut app.settings.dark_mode, true, "\u{E708}  Dark");
                ui.selectable_value(&mut app.settings.dark_mode, false, "\u{E706}  Light");
                if before != app.settings.dark_mode {
                    app.apply_theme(ui.ctx());
                    save = true;
                }
            });
        });
        form_row(ui, "Editor font size", None, |ui| {
            let r = ui.add(egui::Slider::new(&mut app.settings.editor_font_size, 10.0..=22.0));
            save |= r.drag_stopped() || (r.changed() && !r.dragged());
        });
        form_row(ui, "Editors", None, |ui| {
            save |= ui.checkbox(&mut app.settings.line_numbers, "Show line numbers").changed();
        });
        form_row(ui, "On open and Refresh", Some("Production folders are never fetched or synced."), |ui| {
            save |= ui.checkbox(&mut app.settings.auto_git_fetch, "git fetch, and show how far behind the clone is").changed();
            save |= ui.checkbox(&mut app.settings.auto_sync_shared, "Sync shared icons and recipe overrides (when syncSharedContent is on)").changed();
        });
        if save {
            app.save_settings();
        }
    });
}

/// Agent CLI and model, saved as they change. The model list comes from the
/// selected CLI; "Other…" takes any model id the CLI accepts.
pub fn agent_picker(app: &mut StudioApp, ui: &mut Ui, salt: &str) {
    let found = *app.agent.backends_found.get_or_insert_with(|| AgentBackend::ALL.map(|b| b.locate().is_some()));
    let mut save = false;
    ui.horizontal_wrapped(|ui| {
        let before = app.settings.agent_backend;
        let label = |b: AgentBackend, f: bool| if f { b.label().to_string() } else { format!("{} (not found)", b.label()) };
        let current = AgentBackend::ALL.iter().zip(found).find(|(b, _)| **b == before).map(|(b, f)| label(*b, f)).unwrap_or_default();
        egui::ComboBox::from_id_salt(("agent-backend", salt)).selected_text(current).show_ui(ui, |ui| {
            for (b, f) in AgentBackend::ALL.into_iter().zip(found) {
                ui.selectable_value(&mut app.settings.agent_backend, b, label(b, f));
            }
        });
        if app.settings.agent_backend != before {
            // Model ids belong to one CLI.
            app.settings.agent_model.clear();
            app.settings_ui.custom_model = false;
            save = true;
        }
        let backend = app.settings.agent_backend;
        let available = AgentBackend::ALL.iter().zip(found).any(|(b, f)| *b == backend && f);
        let waker = app.waker.clone();
        let models = match available.then(|| app.settings_ui.models(backend, &waker)) {
            Some(Models::Ready(list)) => list.clone(),
            Some(Models::Loading(_)) => {
                ui.spinner().on_hover_text("Asking the CLI which models it offers…");
                Vec::new()
            }
            Some(Models::Failed(e)) => {
                ui.label(RichText::new("\u{E7BA}").color(theme::amber())).on_hover_text(format!("Could not list models: {e}"));
                Vec::new()
            }
            None => Vec::new(),
        };
        let model = app.settings.agent_model.trim().to_string();
        let custom = app.settings_ui.custom_model || (!model.is_empty() && !models.contains(&model));
        let shown = if custom { "Other…".to_string() } else if model.is_empty() { "CLI default".to_string() } else { model.clone() };
        egui::ComboBox::from_id_salt(("agent-model", salt)).selected_text(shown).height(400.0).show_ui(ui, |ui| {
            if ui.selectable_label(!custom && model.is_empty(), "CLI default").clicked() {
                app.settings.agent_model.clear();
                app.settings_ui.custom_model = false;
                save = true;
            }
            for m in &models {
                if ui.selectable_label(!custom && &model == m, m).clicked() {
                    app.settings.agent_model = m.clone();
                    app.settings_ui.custom_model = false;
                    save = true;
                }
            }
            if ui.selectable_label(custom, "Other…").clicked() {
                app.settings_ui.custom_model = true;
            }
        });
        if custom {
            save |= ui
                .add(egui::TextEdit::singleline(&mut app.settings.agent_model).hint_text("model id").desired_width(160.0))
                .lost_focus();
        }
    });
    if save {
        app.save_settings();
    }
}
