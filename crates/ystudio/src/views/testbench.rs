//! Test Bench: the skill's testing ladder for the open recipe, from instant
//! lint to a consented live install test, with verdicts.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::harness::InstallTestOptions;
use ystudio_core::recipe::RecipeDoc;
use ystudio_core::workspace::WorkspaceKind;

use crate::app::{Step, StepState, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

const SAFE_LADDER: [Step; 3] = [Step::Lint, Step::Schema, Step::DryRun];

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_recipe(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    super::designer::header(app, ui, "Test Bench");
    ui.separator();

    let writable = app.writable();
    let id = app.recipe.as_ref().unwrap().id.clone();
    let busy = app.bench.last_job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running());

    if matches!(app.ws.as_ref().map(|w| &w.kind), Some(WorkspaceKind::Unconfigured)) {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label("This clone has not been set up with Install-Yardstick -DevelopmentOnly. Schema and lab runs may fail until the modules are installed (see Home).");
        });
    }

    egui::Panel::left("bench-steps").resizable(true).default_size(420.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("bench-steps-scroll").show(ui, |ui| {
            ui.horizontal(|ui| {
                if theme::primary(ui, writable && !busy, RichText::new("\u{E768}  Run safe checks"))
                    .on_hover_text("Lint → schema → dry run (-WhatIf). Nothing is installed.")
                    .clicked()
                {
                    app.run_steps(&SAFE_LADDER);
                }
                if busy
                    && let Some(job) = app.bench.last_job.and_then(|j| app.job_mut(j))
                    && ui.button("\u{E71A}  Stop").clicked()
                {
                    job.proc.cancel();
                    app.bench.queue.clear();
                }
            });
            ui.add_space(6.0);
            for step in Step::ALL {
                let state = app.step_state(&id, step);
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (icon, color) = match state {
                            None => ("\u{EA3A}", theme::grey()),
                            Some(StepState::Running) => ("\u{F16A}", theme::blue()),
                            Some(StepState::Pass) => ("\u{E73E}", theme::green()),
                            Some(StepState::Warn) => ("\u{E7BA}", theme::amber()),
                            Some(StepState::Fail) => ("\u{E711}", theme::red()),
                        };
                        ui.label(RichText::new(icon).color(color).size(18.0));
                        let mut title = RichText::new(step.title()).semibold();
                        if step == Step::Live {
                            title = title.color(theme::red());
                        }
                        ui.label(title);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let enabled = !busy && (writable || step == Step::Lint);
                            let label = if step == Step::Live { "Run live…" } else { "Run" };
                            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                if step == Step::Live {
                                    app.bench.live_modal = true;
                                    app.bench.live_ack = false;
                                } else {
                                    app.bench.queue.clear();
                                    app.bench.queue_recipe = Some(id.clone());
                                    app.run_step(step);
                                }
                            }
                        });
                    });
                    ui.label(RichText::new(step.detail()).small().weak());
                });
                ui.add_space(2.0);
            }
            ui.add_space(6.0);
            ui.label(RichText::new(
                "A recipe is Validated only when every phase passes in one live run. A dry run proves the scripts, \
                 download and detection inputs, but not the installer itself.",
            ).small().weak());
        });
    });

    // Results and log.
    let result = app
        .bench
        .last_run
        .as_ref()
        .and_then(|run| run.summary.as_ref().and_then(|s| s.results.iter().find(|r| r.id == id).cloned()).map(|r| (run.clone(), r)));
    if let Some((run, result)) = &result {
        theme::card(ui, |ui| {
            let v = result.verdict();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Latest install test").semibold());
                theme::badge(ui, v.label(), theme::verdict_color(v));
                ui.label(RichText::new(&run.timestamp).small().monospace());
                if let Some(ver) = &result.version {
                    ui.label(format!("version {ver}"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("\u{E838}  Run folder").clicked() {
                        super::open_in_explorer(&run.dir);
                    }
                    if ui.small_button("\u{E9D2}  Open in Runs").clicked() {
                        app.runs_ui.select_dir(&run.dir);
                        app.view = View::Runs;
                    }
                    if matches!(v, ystudio_core::results::Verdict::Failed | ystudio_core::results::Verdict::Incomplete)
                        && ui.small_button("\u{E99A}  Ask the agent to fix").clicked()
                    {
                        let mut evidence = run.summary_md().unwrap_or_default();
                        if let Some(t) = run.transcript() {
                            let tail: Vec<&str> = t.lines().rev().take(150).collect();
                            evidence += "\n\n--- transcript tail ---\n";
                            evidence += &tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                        }
                        app.agent.prepare_fix(&id, evidence);
                        app.view = View::Agent;
                    }
                });
            });
            super::runs::phases_table(ui, result, "bench-phases");
        });
    }

    let last = app.bench.last_job;
    match last.and_then(|j| app.job_mut(j)) {
        Some(job) => {
            ui.label(RichText::new(&job.proc.spec.label).semibold());
            job_log(ui, &mut job.proc, "bench");
        }
        None => {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Run the safe checks to start.").weak());
            });
        }
    }
}

/// The consent dialog in front of a live install test.
pub fn live_modal(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.bench.live_modal {
        return;
    }
    let Some(r) = &app.recipe else {
        app.bench.live_modal = false;
        return;
    };
    let doc = RecipeDoc::parse(&r.text).unwrap_or_default();
    let field = |k: &str| doc.get_str(k).filter(|s| !s.trim().is_empty());
    let name = field("displayName").unwrap_or_else(|| r.id.clone());
    let mut start = false;
    let mut cancel = false;
    let opts = &mut app.bench.live_options;
    egui::Modal::new(egui::Id::unique("live-modal")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.label(RichText::new("\u{E7BA}  Live install test").heading().color(theme::red()));
        ui.label(format!(
            "This downloads {name}, INSTALLS it on this machine, checks detection, UNINSTALLS it and looks for residue."
        ));
        ui.add_space(6.0);
        egui::Grid::new("live-grid").num_columns(2).show(ui, |ui| {
            ui.label("Context");
            ui.label(field("installExperience").unwrap_or_else(|| "system (default) - one UAC prompt".into()));
            ui.end_row();
            ui.label("Install");
            ui.label(RichText::new(field("installScript").or_else(|| field("powerShellInstallScript").map(|_| "powerShellInstallScript".into())).unwrap_or_default()).monospace());
            ui.end_row();
            ui.label("Uninstall");
            ui.label(RichText::new(field("uninstallScript").or_else(|| field("powerShellUninstallScript").map(|_| "powerShellUninstallScript".into())).unwrap_or_default()).monospace());
            ui.end_row();
        });
        ui.separator();
        ui.checkbox(&mut opts.include_reinstall, "Also test reinstall");
        ui.checkbox(&mut opts.skip_uninstall, "Skip uninstall (leaves the app installed - not a full validation)");
        ui.checkbox(&mut opts.force, RichText::new("-Force: continue even if the app is already installed (may uninstall YOUR copy)").color(theme::amber()));
        ui.horizontal(|ui| {
            ui.label("Install timeout (min)");
            ui.add(egui::DragValue::new(&mut opts.install_timeout_minutes).range(1..=240));
            ui.label("Detection settle (s)");
            ui.add(egui::DragValue::new(&mut opts.detection_settle_seconds).range(0..=900));
        });
        if ui.small_button("Reset to recommended").clicked() {
            *opts = InstallTestOptions::live();
        }
        ui.separator();
        ui.checkbox(&mut app.bench.live_ack, format!("I consent to installing and uninstalling {name} on this machine."));
        ui.horizontal(|ui| {
            if theme::primary(ui, app.bench.live_ack, RichText::new("Start live test")).clicked() {
                start = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if cancel {
        app.bench.live_modal = false;
    }
    if start {
        app.bench.live_modal = false;
        let id = app.recipe.as_ref().unwrap().id.clone();
        app.bench.queue.clear();
        app.bench.queue_recipe = Some(id);
        app.run_step(Step::Live);
    }
}
