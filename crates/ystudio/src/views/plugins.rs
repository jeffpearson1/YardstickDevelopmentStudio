//! Plugins in Settings: what is installed, install from a package with a
//! trust prompt, turn tabs on or off, each plugin's settings, uninstall.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::plugin::{self, Gate, Health, Package, SettingKind};

use crate::app::{StudioApp, View};
use crate::theme::{self, FluentText as _};
use crate::views::plugin_page::{dev_workspace, plugin_busy, risk_color};
use crate::views::update::CURRENT_VERSION;

#[derive(Default)]
pub struct ManagerState {
    /// A package waiting on the trust prompt.
    trust: Option<Trust>,
    /// The plugin whose settings are expanded.
    pub open_settings: Option<String>,
    confirm_uninstall: Option<String>,
}

struct Trust {
    pkg: Package,
    source: String,
    ack: bool,
    error: Option<String>,
}

pub fn card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Plugins");
        ui.label("Plugins add their own tab to the sidebar. They are packages of PowerShell steps and agent instructions; steps that install software or change the tenant always ask for your approval first.");
        ui.add_space(8.0);
        let plugins: Vec<plugin::Installed> = app.plugins.registry.plugins.clone();
        if plugins.is_empty() && app.plugins.registry.broken.is_empty() {
            ui.label(RichText::new("No plugins are installed.").weak());
        }
        for p in &plugins {
            plugin_row(app, ui, p);
            ui.add_space(6.0);
        }
        for (dir, why) in app.plugins.registry.broken.clone() {
            theme::banner(ui, theme::red(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} could not be loaded: {why}", dir.display()));
                    if ui.small_button("Open folder").clicked() {
                        super::open_in_explorer(&dir);
                    }
                });
            });
            ui.add_space(6.0);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("\u{E8B5}  Install from file…").clicked()
                && let Some(zip) = rfd::FileDialog::new().add_filter("Plugin package", &["zip"]).pick_file()
            {
                open_package(app, zip);
            }
            if ui.button("\u{E838}  Plugins folder").clicked() {
                let _ = std::fs::create_dir_all(&app.plugins.dir);
                super::open_in_explorer(&app.plugins.dir);
            }
        });
    });
}

/// Reads a package and shows the trust prompt for it.
pub fn open_package(app: &mut StudioApp, zip: PathBuf) {
    match Package::open(&zip) {
        Ok(pkg) => app.plugins_ui.trust = Some(Trust { pkg, source: zip.display().to_string(), ack: false, error: None }),
        Err(e) => app.error(format!("{} is not a usable plugin: {e:#}", zip.display())),
    }
}

fn plugin_row(app: &mut StudioApp, ui: &mut Ui, p: &plugin::Installed) {
    let id = p.manifest.id.clone();
    let m = &p.manifest;
    ui.horizontal(|ui| {
        ui.label(RichText::new(m.glyph()).family(egui::FontFamily::Name(theme::ICONS.into())).size(18.0));
        ui.label(RichText::new(&m.name).semibold());
        ui.label(RichText::new(format!("{} · {}", m.version, m.publisher)).weak());
        match &p.health {
            Health::Ok => {}
            Health::Modified(files) => {
                theme::badge(ui, "Modified", theme::red()).on_hover_text(files.join("\n"));
            }
            Health::Unrecorded => {
                theme::badge(ui, "Not installed by the Studio", theme::amber());
            }
            Health::Incompatible(why) => {
                theme::badge(ui, "Needs a newer Studio", theme::amber()).on_hover_text(why);
            }
        }
        for missing in app.plugins.registry.missing_requirements(p) {
            theme::badge(ui, missing, theme::amber());
        }
        let values = app.settings.plugins.get(&id).map(|p| p.values.clone()).unwrap_or_default();
        if plugin::resolve_settings(m, &values, dev_workspace(app).as_deref()).is_err() {
            theme::badge(ui, "Needs settings", theme::amber());
        }
    });
    if !m.description.is_empty() {
        ui.label(RichText::new(&m.description).weak());
    }
    ui.horizontal(|ui| {
        let prefs = app.settings.plugins.entry(id.clone()).or_default();
        if ui.checkbox(&mut prefs.enabled, "Show its tab").changed() {
            if !prefs.enabled && app.plugins.view_of(&id) == Some(app.view) {
                app.view = View::Settings;
            }
            app.save_settings();
        }
        let open = app.plugins_ui.open_settings.as_deref() == Some(id.as_str());
        if ui.selectable_label(open, "\u{E713}  Settings").clicked() {
            app.plugins_ui.open_settings = if open { None } else { Some(id.clone()) };
        }
        if ui.button("\u{E838}  Folder").clicked() {
            super::open_in_explorer(&p.root);
        }
        let busy = plugin_busy(app, &id);
        let resp = ui.add_enabled(!busy, egui::Button::new("\u{E74D}  Uninstall"));
        if busy {
            resp.on_disabled_hover_text("One of its steps is running.");
        } else if resp.clicked() {
            app.plugins_ui.confirm_uninstall = Some(id.clone());
        }
    });
    if app.plugins_ui.open_settings.as_deref() == Some(id.as_str()) {
        ui.indent(("plugin-settings", &id), |ui| settings_form(app, ui, &id));
    }
}

/// The plugin's settings, saved as they change. Shows what is still missing.
pub fn settings_form(app: &mut StudioApp, ui: &mut Ui, id: &str) {
    let Some(p) = app.plugins.get(id).cloned() else { return };
    let ws = dev_workspace(app);
    let mut save = false;
    egui::Grid::new(("plugin-settings-grid", id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        for def in &p.manifest.settings {
            let resp = ui.label(def.label());
            if !def.help.is_empty() {
                resp.on_hover_text(&def.help);
            }
            let hint = def.default.clone().unwrap_or_default().replace("{workspace}", &ws.as_ref().map(|w| w.display().to_string()).unwrap_or_else(|| "(open a development workspace)".into()));
            let prefs = app.settings.plugins.entry(id.to_string()).or_default();
            let value = prefs.values.entry(def.key.clone()).or_default();
            ui.horizontal(|ui| match def.kind {
                SettingKind::Bool => {
                    let mut on = if value.is_empty() { def.default.as_deref() == Some("true") } else { value == "true" };
                    if ui.checkbox(&mut on, "").changed() {
                        *value = on.to_string();
                        save = true;
                    }
                }
                SettingKind::Choice => {
                    let shown = if value.is_empty() { hint.clone() } else { value.clone() };
                    egui::ComboBox::from_id_salt(("plugin-choice", id, &def.key)).selected_text(shown).show_ui(ui, |ui| {
                        for choice in &def.choices {
                            if ui.selectable_label(value == choice, choice).clicked() {
                                *value = choice.clone();
                                save = true;
                            }
                        }
                    });
                }
                kind => {
                    let resp = ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(420.0));
                    save |= resp.lost_focus();
                    let pick = match kind {
                        SettingKind::Folder => ui.button("Browse…").clicked().then(|| rfd::FileDialog::new().pick_folder()).flatten(),
                        SettingKind::File => ui.button("Browse…").clicked().then(|| rfd::FileDialog::new().pick_file()).flatten(),
                        _ => None,
                    };
                    if let Some(path) = pick {
                        *value = path.display().to_string();
                        save = true;
                    }
                }
            });
            ui.end_row();
        }
    });
    if save {
        app.save_settings();
    }
    let values = app.settings.plugins.get(id).map(|p| p.values.clone()).unwrap_or_default();
    if let Err(problems) = plugin::resolve_settings(&p.manifest, &values, ws.as_deref()) {
        for problem in problems {
            ui.label(RichText::new(problem).color(theme::amber()));
        }
    }
}

/// The trust prompt and the uninstall confirmation. Called every frame.
pub fn modals(app: &mut StudioApp, ctx: &egui::Context) {
    trust_modal(app, ctx);
    if let Some(id) = app.plugins_ui.confirm_uninstall.clone() {
        let name = app.plugins.get(&id).map(|p| p.manifest.name.clone()).unwrap_or_else(|| id.clone());
        let (mut yes, mut no) = (false, false);
        egui::Modal::new(egui::Id::unique("plugin-uninstall")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(format!("Uninstall {name}?")).heading());
            ui.label("Its tab goes away. Its settings and work folders are kept, so reinstalling picks up where you left off.");
            ui.horizontal(|ui| {
                yes = theme::primary(ui, true, "Uninstall").clicked();
                no = ui.button("Cancel").clicked();
            });
        });
        if yes {
            match plugin::uninstall(&app.plugins.dir, &id) {
                Ok(()) => {
                    if app.plugins.view_of(&id) == Some(app.view) {
                        app.view = View::Settings;
                    }
                    app.plugins.reload();
                    app.notify(format!("Uninstalled {name}."));
                }
                Err(e) => app.error(format!("{e:#}")),
            }
        }
        if yes || no {
            app.plugins_ui.confirm_uninstall = None;
        }
    }
}

fn trust_modal(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(mut t) = app.plugins_ui.trust.take() else { return };
    let m = t.pkg.manifest.clone();
    let current = app.plugins.get(&m.id).cloned();
    let (mut install, mut cancel) = (false, false);
    egui::Modal::new(egui::Id::unique("plugin-trust")).show(ctx, |ui| {
        ui.set_width(720.0);
        let verb = match &current {
            None => format!("Install {} {}?", m.name, m.version),
            Some(c) if c.manifest.version == m.version => format!("Reinstall {} {}?", m.name, m.version),
            Some(c) => format!("Update {} from {} to {}?", m.name, c.manifest.version, m.version),
        };
        ui.label(RichText::new(verb).heading());
        if !m.description.is_empty() {
            ui.label(&m.description);
        }
        ui.add_space(6.0);
        egui::Grid::new("plugin-trust-facts").num_columns(2).show(ui, |ui| {
            ui.label(RichText::new("Publisher").weak());
            ui.label(if m.publisher.is_empty() { "(not stated)" } else { &m.publisher });
            ui.end_row();
            ui.label(RichText::new("Package").weak());
            ui.label(&t.source);
            ui.end_row();
            ui.label(RichText::new("SHA-256").weak());
            ui.label(RichText::new(&t.pkg.sha256).monospace().small());
            ui.end_row();
            ui.label(RichText::new("Contents").weak());
            ui.label(format!("{} files, {:.1} KB", t.pkg.file_names().count(), t.pkg.total_bytes() as f64 / 1024.0));
            ui.end_row();
        });
        if let Some(why) = m.incompatibility(CURRENT_VERSION) {
            ui.label(RichText::new(format!("This plugin {why}.")).color(theme::red()));
        }
        ui.add_space(6.0);
        ui.label(RichText::new("What its steps can do").semibold());
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            egui::Grid::new("plugin-trust-steps").striped(true).num_columns(3).show(ui, |ui| {
                for s in &m.steps {
                    let changed = current.as_ref().is_some_and(|c| c.manifest.step(&s.id).is_none_or(|old| old.risk != s.risk || old.gate != s.gate || old.script != s.script));
                    let label = if changed { format!("{} (new or changed)", s.label()) } else { s.label().to_string() };
                    ui.label(label);
                    theme::badge(ui, s.risk.label(), risk_color(s.risk));
                    ui.label(match &s.gate {
                        Gate::None => "runs when you click Run".to_string(),
                        Gate::Confirm => "asks you to confirm".to_string(),
                        Gate::Hash { file, .. } => format!("needs your approval of {file}"),
                    });
                    ui.end_row();
                }
            });
            if let Some(agent) = &m.agent
                && !agent.tasks.is_empty()
            {
                ui.add_space(4.0);
                ui.label(format!("Agent tasks: {}", agent.tasks.iter().map(|t| t.label.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if !m.requires.modules.is_empty() {
                ui.label(format!("PowerShell modules: {}", m.requires.modules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if !m.requires.plugins.is_empty() {
                ui.label(format!("Works with plugins: {}", m.requires.plugins.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", ")));
            }
        });
        if let Some(e) = &t.error {
            ui.label(RichText::new(e).color(theme::red()));
        }
        ui.separator();
        let who = if m.publisher.is_empty() { "its author".to_string() } else { m.publisher.clone() };
        ui.checkbox(&mut t.ack, format!("I trust {who} and want to run this plugin's scripts on this PC."));
        ui.horizontal(|ui| {
            install = theme::primary(ui, t.ack && m.incompatibility(CURRENT_VERSION).is_none(), "Install").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel {
        return;
    }
    if install {
        match plugin::install(&t.pkg, &app.plugins.dir, CURRENT_VERSION, &t.source, false) {
            Ok(inst) => {
                let prefs = app.settings.plugins.entry(m.id.clone()).or_default();
                prefs.trusted_sha256 = t.pkg.sha256.clone();
                prefs.source = t.source.clone();
                app.save_settings();
                app.plugins.reload();
                app.plugins_ui.open_settings = Some(m.id.clone());
                app.notify(format!("Installed {} {}. It has its own tab in the sidebar.", inst.manifest.name, inst.manifest.version));
                return;
            }
            Err(e) => t.error = Some(format!("{e:#}")),
        }
    }
    app.plugins_ui.trust = Some(t);
}
