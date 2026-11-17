//! Plugins in Settings: what is installed, install from a package with a
//! trust prompt, turn tabs on or off, each plugin's settings, uninstall.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::plugin::{self, Checked, Gate, Health, Package, SettingKind};
use ystudio_core::update::{self, Release};

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
    rx: Option<Receiver<Msg>>,
    /// The newest release of each plugin the last check found.
    pub latest: BTreeMap<String, Release>,
    /// A check has finished at least once this session.
    checked: bool,
    checking: bool,
    /// A startup check: stay quiet unless an update turned up.
    quiet: bool,
    /// The plugin being downloaded, and the bytes so far.
    downloading: Option<(String, Arc<AtomicU64>)>,
    error: Option<String>,
}

enum Msg {
    Checked(Checked),
    Downloaded(String, Release, Box<anyhow::Result<(Package, PathBuf)>>),
}

impl ManagerState {
    /// The release that would update an installed plugin, if there is one.
    pub fn update_for(&self, id: &str, installed: &str) -> Option<&Release> {
        self.latest.get(id).filter(|r| update::is_newer(&r.version, installed))
    }

    /// Releases of plugins that are not installed.
    fn available<'a>(&'a self, installed: &'a [plugin::Installed]) -> impl Iterator<Item = (&'a String, &'a Release)> {
        self.latest.iter().filter(move |(id, _)| !installed.iter().any(|p| &p.manifest.id == *id))
    }
}

/// Looks for new plugin releases in the background: the Studio's own and each
/// installed plugin's source.
pub fn start_check(app: &mut StudioApp, quiet: bool) {
    if app.plugins_ui.checking {
        return;
    }
    let installed: Vec<(String, Option<plugin::Source>)> = app.plugins.registry.plugins.iter().map(|p| (p.manifest.id.clone(), p.manifest.source.clone())).collect();
    let (tx, rx) = channel();
    let waker = app.waker.clone();
    std::thread::spawn(move || {
        let _ = tx.send(Msg::Checked(plugin::check_catalog(&installed)));
        waker();
    });
    let st = &mut app.plugins_ui;
    st.rx = Some(rx);
    st.checking = true;
    st.quiet = quiet;
    st.error = None;
}

/// Downloads a release, checks it, and opens the trust prompt for it.
fn start_download(app: &mut StudioApp, id: &str, release: Release) {
    if app.plugins_ui.downloading.is_some() {
        return;
    }
    let (tx, rx) = channel();
    let progress = Arc::new(AtomicU64::new(0));
    let (id_owned, rel, p, waker) = (id.to_string(), release, progress.clone(), app.waker.clone());
    std::thread::spawn(move || {
        let result = plugin::download_package(&id_owned, &rel, &update::download_dir().join("plugins"), &p);
        let _ = tx.send(Msg::Downloaded(id_owned, rel, Box::new(result)));
        waker();
    });
    let st = &mut app.plugins_ui;
    st.rx = Some(rx);
    st.downloading = Some((id.to_string(), progress));
    st.error = None;
}

/// Drains finished checks and downloads. Called every frame.
pub fn poll(app: &mut StudioApp) {
    let Some(msg) = app.plugins_ui.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { return };
    app.plugins_ui.rx = None;
    match msg {
        Msg::Checked(checked) => {
            let quiet = std::mem::take(&mut app.plugins_ui.quiet);
            app.plugins_ui.checking = false;
            app.plugins_ui.checked = true;
            app.plugins_ui.latest = checked.latest;
            let updates: Vec<String> =
                app.plugins.registry.plugins.iter().filter(|p| app.plugins_ui.update_for(&p.manifest.id, &p.manifest.version).is_some()).map(|p| p.manifest.name.clone()).collect();
            if !updates.is_empty() {
                app.notify(format!("Plugin update available: {} - see Settings > Plugins.", updates.join(", ")));
            } else if !checked.errors.is_empty() && !quiet {
                app.error(checked.errors.join(" "));
            }
            if !checked.errors.is_empty() {
                app.plugins_ui.error = Some(checked.errors.join(" "));
            }
        }
        Msg::Downloaded(id, release, result) => {
            app.plugins_ui.downloading = None;
            match *result {
                Ok((pkg, _file)) => {
                    let source = if release.html_url.is_empty() { release.asset_url.clone() } else { release.html_url.clone() };
                    app.plugins_ui.trust = Some(Trust { pkg, source, ack: false, error: None });
                }
                Err(e) => {
                    app.plugins_ui.error = Some(format!("{id}: {e:#}"));
                    app.error(format!("Could not download {id}: {e:#}"));
                }
            }
        }
    }
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
        available_list(app, ui, &plugins);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let checking = app.plugins_ui.checking;
            if ui.add_enabled(!checking, egui::Button::new("\u{E72C}  Check for updates")).clicked() {
                start_check(app, false);
            }
            if checking {
                ui.spinner();
            } else if app.plugins_ui.checked && app.plugins_ui.error.is_none() && !app.plugins_ui.latest.is_empty() {
                ui.label(RichText::new("Checked").weak());
            }
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
        if let Some(e) = &app.plugins_ui.error {
            ui.label(RichText::new(e).small().color(theme::amber()));
        }
    });
}

/// Plugins the Studio's releases offer that are not installed yet.
fn available_list(app: &mut StudioApp, ui: &mut Ui, installed: &[plugin::Installed]) {
    let offers: Vec<(String, Release)> = app.plugins_ui.available(installed).map(|(id, r)| (id.clone(), r.clone())).collect();
    if offers.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Available").semibold());
    for (id, release) in offers {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&id).semibold());
            ui.label(RichText::new(&release.version).weak());
            let busy = app.plugins_ui.downloading.is_some();
            let resp = ui.add_enabled(!busy, egui::Button::new("Install…"));
            if resp.clicked() {
                start_download(app, &id, release.clone());
            }
            if app.plugins_ui.downloading.as_ref().is_some_and(|(d, _)| *d == id) {
                ui.spinner();
            }
        });
        let about = release_summary(&release.notes, 300);
        if !about.is_empty() {
            ui.label(RichText::new(about).weak());
        }
        ui.add_space(4.0);
    }
}

/// The first paragraph of release notes as plain text: markdown marks removed, one line,
/// cut at a word boundary after about `max` characters.
fn release_summary(notes: &str, max: usize) -> String {
    let paragraph: Vec<&str> = notes.lines().map(str::trim).skip_while(|l| l.is_empty()).take_while(|l| !l.is_empty()).collect();
    let plain: String = paragraph.join(" ").chars().filter(|c| !matches!(c, '*' | '`' | '_')).collect();
    let plain = plain.trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.chars().count() <= max {
        return plain;
    }
    let cut: String = plain.chars().take(max).collect();
    let end = cut.rfind(' ').unwrap_or(cut.len());
    format!("{}…", cut[..end].trim_end_matches([',', ';', ':', '.']))
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
        if let Some(release) = app.plugins_ui.update_for(&id, &m.version).cloned() {
            theme::badge(ui, format!("Update {}", release.version), theme::blue()).on_hover_text(release.notes.chars().take(400).collect::<String>());
            let busy = app.plugins_ui.downloading.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Update…")).clicked() {
                start_download(app, &id, release);
            }
            if app.plugins_ui.downloading.as_ref().is_some_and(|(d, _)| *d == id) {
                ui.spinner();
            }
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
    let mut clone_setting = None;
    egui::Grid::new(("plugin-settings-grid", id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        for def in &p.manifest.settings {
            let private_ready = def.clone_repository && crate::views::plugin_page::has_private_workspace(app, id);
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
                    if def.clone_repository {
                        let cloning = app.jobs.iter().any(|j| {
                            j.proc.is_running() && matches!(&j.kind, crate::app::JobKind::PluginClone { plugin, setting, .. } if plugin == id && setting == &def.key)
                        });
                        if cloning {
                            ui.spinner();
                            ui.label("Cloning…");
                        } else if private_ready {
                            theme::badge(ui, "Private copy ready", theme::green());
                        } else if ui.button("Clone private copy").on_hover_text("Creates a Studio-managed Yardstick clone used only by this plugin.").clicked() {
                            clone_setting = Some(def.key.clone());
                        }
                    }
                }
            });
            ui.end_row();
        }
    });
    if save {
        app.save_settings();
    }
    if let Some(setting) = clone_setting {
        let dest = app
            .plugins
            .dir
            .parent()
            .unwrap_or(&app.plugins.dir)
            .join("plugin-workspaces")
            .join(id)
            .join("YardstickDev");
        if ystudio_core::git::is_repo(&dest) {
            app.settings.plugins.entry(id.to_string()).or_default().values.insert(setting, dest.display().to_string());
            app.save_settings();
            app.notify(format!("{} is using its private Yardstick clone.", p.manifest.name));
        } else {
            match ystudio_core::git::clone(&app.settings.repo_url, &dest) {
                Ok(spec) => {
                    app.start_job(spec, crate::app::JobKind::PluginClone { plugin: id.to_string(), setting, dest });
                }
                Err(e) => app.error(format!("Could not start the private clone: {e:#}")),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::plugin_page::tests::install_sample;
    use crate::workflow::tests::{fake_app, render};

    fn release(version: &str) -> Release {
        Release {
            version: version.into(),
            notes: "Adds a Products table.\nMore detail.".into(),
            html_url: format!("https://github.com/o/r/releases/tag/plugin-sample-plugin-v{version}"),
            asset_name: format!("sample-plugin-{version}.zip"),
            asset_url: "https://example.invalid/sample.zip".into(),
            size: 1,
            sha256: Some("a".repeat(64)),
        }
    }

    fn send(app: &mut StudioApp, msg: Msg) {
        let (tx, rx) = channel();
        app.plugins_ui.rx = Some(rx);
        tx.send(msg).unwrap();
        poll(app);
    }

    #[test]
    fn available_plugins_show_a_readable_description() {
        let notes = "**SCCM App Migration** browses the applications in a Configuration Manager site and migrates one to Intune. It reads the site through the SMS Provider over CIM and `never` changes it.\n\nNeeds Studio 0.6.0.\n\n## 1.0.0\n- more";
        let short = release_summary(notes, 90);
        assert_eq!(short, "SCCM App Migration browses the applications in a Configuration Manager site and migrates…");
        let full = release_summary(notes, 400);
        assert!(full.starts_with("SCCM App Migration browses") && full.ends_with("never changes it."), "{full}");
        assert!(!full.contains("Needs Studio") && !full.contains('*') && !full.contains('`'));
        assert_eq!(release_summary("\n\n# Title line\nsecond line", 100), "Title line second line");
        assert_eq!(release_summary("", 100), "");
    }

    #[test]
    fn checks_show_updates_and_installable_plugins() {
        let (dir, mut app) = fake_app();
        install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        app.plugins_ui.checking = true;
        let latest = BTreeMap::from([("sample-plugin".to_string(), release("1.1.0")), ("oneoff-packager".to_string(), release("1.0.0"))]);
        send(&mut app, Msg::Checked(Checked { latest, errors: Vec::new() }));
        assert!(!app.plugins_ui.checking && app.plugins_ui.checked);
        assert_eq!(app.plugins_ui.update_for("sample-plugin", "1.0.0").map(|r| r.version.as_str()), Some("1.1.0"));
        assert!(app.plugins_ui.update_for("sample-plugin", "1.1.0").is_none(), "already current");
        assert!(app.plugins_ui.update_for("oneoff-packager", "1.0.0").is_none(), "not installed, so offered rather than updated");
        assert!(app.status.text.contains("Plugin update available: Sample Plugin"), "{}", app.status.text);
        let installed = app.plugins.registry.plugins.clone();
        assert_eq!(app.plugins_ui.available(&installed).map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["oneoff-packager"]);

        app.plugins_ui.latest.get_mut("oneoff-packager").unwrap().notes = "**One-Off Packager** packages software requests that are not good candidates for a Yardstick recipe. An agent triages the ticket and drafts the recipe; everything that downloads, installs, packages or touches Intune is a separate step.\n\nNeeds Studio 0.6.0.".into();
        app.view = View::Settings;
        render(&mut app, "plugin-updates", 1315, 1900);
    }

    #[test]
    fn startup_checks_stay_quiet_about_problems() {
        let (_dir, mut app) = fake_app();
        app.plugins_ui.quiet = true;
        send(&mut app, Msg::Checked(Checked { latest: BTreeMap::new(), errors: vec!["Checking o/r: offline".into()] }));
        assert!(!app.status.error, "a quiet check does not interrupt");
        assert!(app.plugins_ui.error.as_deref().is_some_and(|e| e.contains("offline")), "but Settings can show it");

        app.plugins_ui.quiet = false;
        send(&mut app, Msg::Checked(Checked { latest: BTreeMap::new(), errors: vec!["Checking o/r: offline".into()] }));
        assert!(app.status.error && app.status.text.contains("offline"));
    }

    #[test]
    fn a_downloaded_package_goes_to_the_trust_prompt() {
        let (dir, mut app) = fake_app();
        install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let root = app.plugins.get("sample-plugin").unwrap().root.clone();
        let pkg = Package::from_bytes(&plugin::zip_dir(&root).unwrap()).unwrap();
        app.plugins_ui.downloading = Some(("sample-plugin".into(), Arc::new(AtomicU64::new(0))));
        send(&mut app, Msg::Downloaded("sample-plugin".into(), release("1.1.0"), Box::new(Ok((pkg, dir.path().join("x.zip"))))));
        assert!(app.plugins_ui.downloading.is_none());
        let trust = app.plugins_ui.trust.as_ref().expect("the trust prompt opens");
        assert!(trust.source.ends_with("plugin-sample-plugin-v1.1.0") && !trust.ack, "nothing is installed until the user trusts it");
        render(&mut app, "plugin-update-trust", 1315, 900);

        // A failed download is reported and installs nothing.
        app.plugins_ui.trust = None;
        send(&mut app, Msg::Downloaded("sample-plugin".into(), release("1.1.0"), Box::new(Err(anyhow::anyhow!("failed verification")))));
        assert!(app.plugins_ui.trust.is_none() && app.status.error && app.status.text.contains("failed verification"));
    }
}
