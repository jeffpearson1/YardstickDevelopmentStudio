//! Runs: the harness's install-test evidence and lab runs.

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkViewer;
use ystudio_core::results::{self, RecipeResult};

use crate::app::{StudioApp, View};
use crate::theme::{self, FluentText as _};

#[derive(Default, PartialEq, Clone, Copy)]
enum Tab {
    #[default]
    Results,
    Summary,
    Transcript,
    Lab,
}

#[derive(Default)]
pub struct RunsState {
    selected: Option<PathBuf>,
    /// The open recipe the selection last followed.
    followed: Option<String>,
    tab: Tab,
    loaded: bool,
}

impl RunsState {
    pub fn select_dir(&mut self, dir: &Path) {
        self.selected = Some(dir.to_path_buf());
        self.tab = Tab::Results;
    }
}

pub fn phases_table(ui: &mut Ui, r: &RecipeResult, salt: &str) {
    egui::Grid::new((salt, &r.id)).striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
        ui.label(RichText::new("Phase").semibold());
        ui.label(RichText::new("Status").semibold());
        ui.label(RichText::new("Time").semibold());
        ui.label(RichText::new("Detail").semibold());
        ui.end_row();
        for p in &r.phases {
            ui.label(p.label());
            ui.label(RichText::new(&p.status).color(theme::status_color(&p.status)).semibold());
            ui.label(p.duration_sec.map(|d| format!("{d:.0}s")).unwrap_or_default());
            ui.label(RichText::new(p.detail.clone().unwrap_or_default()).small());
            ui.end_row();
        }
    });
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    if !app.runs_ui.loaded {
        app.refresh_runs();
        app.runs_ui.loaded = true;
    }
    ui.horizontal(|ui| {
        theme::page_title(ui, "Runs");
        if ui.button("\u{E72C}  Refresh").clicked() {
            app.refresh_runs();
        }
        if let Some(ws) = &app.ws
            && ui.button("\u{E838}  Artifacts folder").clicked()
        {
            super::open_in_explorer(&ws.layout.harness_artifacts);
        }
    });
    egui::CollapsingHeader::new("Agent credits by model").id_salt("runs-usage").default_open(true).show(ui, |ui| super::usage::card(app, ui));
    ui.separator();

    egui::Panel::left("runs-list").resizable(true).default_size(330.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("runs-list-scroll").show(ui, |ui| {
            if app.runs.is_empty() {
                ui.weak("No install tests yet. Run a dry run from the Test Bench.");
            }
            for run in &app.runs {
                let selected = app.runs_ui.selected.as_ref() == Some(&run.dir);
                let resp = ui.selectable_label(selected, RichText::new(&run.timestamp).monospace());
                if resp.clicked() {
                    app.runs_ui.selected = Some(run.dir.clone());
                }
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    match &run.summary {
                        Some(s) => {
                            for r in &s.results {
                                let v = r.verdict();
                                theme::badge(ui, &r.id, theme::verdict_color(v)).on_hover_text(v.label());
                            }
                        }
                        None => {
                            ui.label(RichText::new(run.error.clone().unwrap_or_default()).small().color(theme::amber()));
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });
    });

    // Follow the open recipe: show its newest run when it changes.
    let open_id = app.recipe.as_ref().map(|r| r.id.clone());
    if open_id.is_some() && app.runs_ui.followed != open_id {
        let id = open_id.as_deref().unwrap();
        if let Some(run) = app.runs.iter().find(|run| run.summary.as_ref().is_some_and(|s| s.results.iter().any(|r| r.id == id))) {
            app.runs_ui.selected = Some(run.dir.clone());
        }
        app.runs_ui.followed = open_id;
    }
    if app.runs_ui.selected.is_none() {
        app.runs_ui.selected = app.runs.first().map(|r| r.dir.clone());
    }
    let Some(run) = app.runs_ui.selected.as_ref().and_then(|d| app.runs.iter().find(|r| &r.dir == d)).cloned() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(&run.timestamp).strong().monospace());
        if let Some(s) = &run.summary {
            if let Some(m) = &s.machine {
                ui.label(format!("on {m}"));
            }
            if s.elevated == Some(true) {
                theme::badge(ui, "elevated", theme::blue());
            }
        }
        if let Some(e) = &run.error {
            ui.colored_label(theme::amber(), e);
        }
        if ui.small_button("\u{E838}").clicked() {
            super::open_in_explorer(&run.dir);
        }
    });
    ui.horizontal(|ui| {
        let tab = &mut app.runs_ui.tab;
        ui.selectable_value(tab, Tab::Results, "Results");
        ui.selectable_value(tab, Tab::Summary, "summary.md");
        ui.selectable_value(tab, Tab::Transcript, "Transcript");
        ui.selectable_value(tab, Tab::Lab, "Lab runs");
    });
    ui.separator();
    let mut open_id = None;
    let mut fix = None;
    egui::ScrollArea::both().id_salt("run-detail").auto_shrink(false).show(ui, |ui| match app.runs_ui.tab {
        Tab::Results => {
            for r in run.summary.iter().flat_map(|s| &s.results) {
                theme::card(ui, |ui| {
                    let v = r.verdict();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&r.id).strong().monospace().size(16.0));
                        theme::badge(ui, v.label(), theme::verdict_color(v));
                        if let Some(ver) = &r.version {
                            ui.label(format!("v{ver}"));
                        }
                        if let Some(ctx) = &r.context {
                            ui.label(RichText::new(ctx).small().weak());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Open recipe").clicked() {
                                open_id = Some(r.id.clone());
                            }
                            if v == results::Verdict::Failed && ui.small_button("\u{E99A}  Fix with agent").clicked() {
                                fix = Some(r.id.clone());
                            }
                        });
                    });
                    egui::Grid::new(("meta", &r.id)).num_columns(2).show(ui, |ui| {
                        for (k, val) in [
                            ("File", &r.file_name),
                            ("SHA256", &r.sha256),
                            ("Signature", &r.signature_status),
                            ("Signer", &r.signer),
                            ("ProductCode", &r.product_code),
                            ("Install", &r.install_command),
                            ("Uninstall", &r.uninstall_command),
                        ] {
                            if let Some(val) = val.as_ref().filter(|s| !s.is_empty()) {
                                ui.label(RichText::new(k).weak());
                                ui.label(RichText::new(val).monospace().small());
                                ui.end_row();
                            }
                        }
                    });
                    phases_table(ui, r, "runs-phases");
                });
                ui.add_space(6.0);
            }
        }
        Tab::Summary => match run.summary_md() {
            Some(md) => {
                CommonMarkViewer::new().show(ui, &mut app.md_cache, &md);
            }
            None => {
                ui.weak("No summary.md in this run.");
            }
        },
        Tab::Transcript => match run.transcript() {
            Some(t) => {
                ui.add(egui::Label::new(RichText::new(t).monospace().small()).wrap_mode(egui::TextWrapMode::Extend));
            }
            None => {
                ui.weak("No transcript in this run.");
            }
        },
        Tab::Lab => {
            let Some(ws) = &app.ws else { return };
            for id in run.ids() {
                ui.label(RichText::new(&id).semibold());
                let labs = results::list_lab_runs(&ws.layout.harness_artifacts, &id);
                if labs.is_empty() {
                    ui.weak("  no lab runs");
                }
                for l in labs {
                    ui.horizontal(|ui| {
                        if ui.link(&l.timestamp).clicked() {
                            super::open_in_explorer(&l.dir);
                        }
                        ui.label(RichText::new(l.files.join(", ")).small().weak());
                    });
                }
            }
        }
    });
    if let Some(id) = open_id
        && app.open_recipe_by_id(&id)
    {
        app.view = View::Designer;
    }
    if let Some(id) = fix {
        let mut evidence = run.summary_md().unwrap_or_default();
        if let Some(t) = run.transcript() {
            let tail: Vec<&str> = t.lines().rev().take(150).collect();
            evidence += "\n\n--- transcript tail ---\n";
            evidence += &tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        }
        // The Agent tab works on the open recipe; if another has unsaved
        // edits, the save prompt comes first and the user can ask again.
        if app.open_recipe_by_id(&id) && app.recipe.as_ref().is_some_and(|r| r.id == id) {
            app.agent.prepare_fix(&id, evidence);
            app.view = View::Agent;
        }
    }
}
