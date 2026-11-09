//! Promote: Publish-Yardstick.ps1 from this development workspace to a
//! production folder, previewed with -WhatIf first.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::harness::{self, Consent};
use ystudio_core::results::{RecipeResult, VALIDATION_PHASES};
use ystudio_core::workspace::RecipeLocation;

use crate::app::{JobKind, Step, StepState, StudioApp, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Default)]
pub struct PromoteState {
    selected: BTreeSet<String>,
    target: Option<PathBuf>,
    modal: bool,
    ack: bool,
    previewed: Option<(BTreeSet<String>, bool)>,
    job: Option<u64>,
    /// `is_yardstick_root` result for the last target looked at.
    target_checked: Option<(PathBuf, bool)>,
    /// The open recipe last pre-selected, so unticking it sticks.
    followed: Option<String>,
}

impl PromoteState {
    /// Unlocks Publish only after a successful preview of the same selection.
    pub fn preview_finished(&mut self, job: u64, ok: bool) {
        if self.job == Some(job)
            && let Some(p) = &mut self.previewed
        {
            p.1 = ok;
        }
    }
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    theme::page_title(ui, "Promote to production");
    ui.label(RichText::new(
        "Publish-Yardstick copies the selected recipes (and their icons) from this development workspace into a \
         production folder. Preview with -WhatIf first. If validation is incomplete, review the checks before publishing anyway.",
    ).weak());
    ui.add_space(6.0);

    if app.promote.target.is_none() {
        app.promote.target = app.settings.primary_production().cloned();
    }
    ui.horizontal(|ui| {
        ui.label("Target production folder:");
        let shown = app.promote.target.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "choose…".into());
        egui::ComboBox::from_id_salt("promote-target").selected_text(shown).show_ui(ui, |ui| {
            for p in app.settings.production_paths.clone() {
                let label = p.display().to_string();
                ui.selectable_value(&mut app.promote.target, Some(p), label);
            }
        });
        if let Some(t) = &app.promote.target {
            if app.promote.target_checked.as_ref().is_none_or(|(p, _)| p != t) {
                app.promote.target_checked = Some((t.clone(), ystudio_core::workspace::is_yardstick_root(t)));
            }
            if app.promote.target_checked.as_ref().is_some_and(|(_, ok)| !ok) {
                ui.colored_label(theme::red(), "not a Yardstick folder");
            }
        }
    });
    ui.add_space(6.0);

    let dev: Vec<String> = app.recipes.iter().filter(|r| r.location == RecipeLocation::Development).map(|r| r.id.clone()).collect();
    // Start from the recipe being worked on.
    let open_id = app.recipe.as_ref().map(|r| r.id.clone());
    if open_id.is_some() && app.promote.followed != open_id {
        if let Some(id) = open_id.as_ref().filter(|id| dev.contains(id)) {
            app.promote.selected.insert(id.clone());
        }
        app.promote.followed = open_id;
    }
    egui::Panel::left("promote-list").resizable(true).default_size(420.0).show(ui, |ui| {
        ui.label(RichText::new(format!("Development recipes ({})", dev.len())).semibold());
        if dev.is_empty() {
            ui.weak("Nothing in Recipes\\Development yet.");
        }
        egui::ScrollArea::vertical().id_salt("promote-scroll").show(ui, |ui| {
            for id in &dev {
                ui.horizontal(|ui| {
                    let mut on = app.promote.selected.contains(id);
                    if ui.checkbox(&mut on, RichText::new(id).monospace()).changed() {
                        if on {
                            app.promote.selected.insert(id.clone());
                        } else {
                            app.promote.selected.remove(id);
                        }
                    }
                    if !super::recipe_status_badge(app, ui, id) {
                        theme::badge(ui, "never tested", theme::grey());
                    }
                });
            }
        });
    });

    let writable = app.writable();
    let ids: Vec<String> = app.promote.selected.iter().cloned().collect();
    let running = app.promote.job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running());
    let ready = writable && !ids.is_empty() && app.promote.target.is_some() && !running;
    let previewed_ok = app.promote.previewed.as_ref().is_some_and(|(set, ok)| *ok && *set == app.promote.selected);
    ui.horizontal(|ui| {
        if ui.add_enabled(ready, egui::Button::new("\u{E7B3}  Preview (-WhatIf)")).clicked() {
            start_publish(app, &ids, true);
        }
        if theme::primary(ui, ready && previewed_ok, RichText::new("\u{E898}  Publish…"))
            .on_disabled_hover_text("Run a successful preview of exactly this selection first.")
            .clicked()
        {
            app.refresh_runs();
            app.promote.modal = true;
            app.promote.ack = false;
        }
    });
    let unvalidated: Vec<&String> = ids.iter().filter(|id| !app.completion(id).is_complete()).collect();
    if !unvalidated.is_empty() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(format!(
                "Not complete (no passing live test of the current contents): {}. Publish will show the recorded checks and ask whether to publish anyway.",
                unvalidated.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        });
    }
    ui.add_space(4.0);
    if let Some(job) = app.promote.job.and_then(|j| app.job_mut(j)) {
        job_log(ui, &mut job.proc, "promote");
    }
}

fn start_publish(app: &mut StudioApp, ids: &[String], what_if: bool) {
    let Some(ws) = app.ws.clone() else { return };
    let Some(target) = app.promote.target.clone() else { return };
    let consent = if what_if { Consent::None } else { Consent::Confirmed };
    match harness::publish(&ws, ids, &target, what_if, consent, &app.pwsh()) {
        Ok(spec) => {
            app.promote.job = app.start_job(spec, JobKind::Publish { what_if });
            if what_if {
                app.promote.previewed = Some((app.promote.selected.clone(), false));
            } else {
                app.promote.previewed = None;
            }
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

pub fn publish_modal(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.promote.modal {
        return;
    }
    let ids: Vec<String> = app.promote.selected.iter().cloned().collect();
    let incomplete = ids.iter().any(|id| !app.completion(id).is_complete());
    let target = app.promote.target.clone().unwrap_or_default();
    let mut go = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("publish-modal")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.label(RichText::new("\u{E898}  Publish to production").heading().color(theme::amber()));
        ui.label(format!("Publish-Yardstick will copy {} recipe(s) into:", ids.len()));
        ui.label(RichText::new(target.display().to_string()).monospace().strong());
        ui.label(RichText::new(ids.join(", ")).monospace());
        ui.label("The next scheduled Yardstick run in that folder will package and upload them to Intune.");
        if incomplete {
            ui.add_space(8.0);
            ui.label(RichText::new("Validation is incomplete").strong().color(theme::amber()));
            ui.label("Some checks have failed, were skipped, or have not been recorded. Publishing anyway does not mark the recipes validated.");
            egui::ScrollArea::vertical().id_salt("publish-checks").max_height(320.0).show(ui, |ui| {
                for id in &ids {
                    ui.add_space(8.0);
                    ui.label(RichText::new(id).strong().monospace());
                    let latest = app.runs.iter().find_map(|run| {
                        run.summary.as_ref()?.results.iter().find(|r| r.id == *id).map(|result| (run, result))
                    });
                    if let Some((run, result)) = latest {
                        ui.label(format!("Latest install test: {} ({})", run.timestamp, result.verdict().label()));
                    }
                    for line in validation_checks(latest.map(|(_, result)| result)) {
                        ui.label(line);
                    }
                    if let Some(phase) = latest.and_then(|(_, result)| result.phase("Reinstall")) {
                        ui.label(format!("Reinstall: {}", phase.status));
                    }
                    let session: Vec<_> = Step::ALL
                        .iter()
                        .filter_map(|step| {
                            let status = match app.step_state(id, *step)? {
                                StepState::Running => "Running",
                                StepState::Pass => "Passed",
                                StepState::Warn => "Warnings",
                                StepState::Fail => "Failed",
                            };
                            Some(format!("{}: {status}", step.title()))
                        })
                        .collect();
                    if !session.is_empty() {
                        ui.label(RichText::new("Test Bench checks in this session").small().strong());
                        for line in session {
                            ui.label(line);
                        }
                    }
                }
            });
        } else {
            ui.checkbox(&mut app.promote.ack, "I reviewed the preview and want to publish these recipes.");
        }
        ui.horizontal(|ui| {
            if incomplete && ui.button("Go back").clicked() {
                cancel = true;
            }
            let label = if incomplete { "Publish anyway" } else { "Publish" };
            if theme::primary(ui, incomplete || app.promote.ack, RichText::new(label)).clicked() {
                go = true;
            }
            if !incomplete && ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if cancel {
        app.promote.modal = false;
    }
    if go {
        app.promote.modal = false;
        start_publish(app, &ids, false);
    }
}

fn validation_checks(result: Option<&RecipeResult>) -> Vec<String> {
    VALIDATION_PHASES
        .iter()
        .map(|name| {
            let label = match *name {
                "PostDetect" => "Post-install detection",
                "PostUninstall" => "Post-uninstall detection",
                _ => name,
            };
            match result.and_then(|r| r.phase(name)) {
                Some(phase) => {
                    let detail = phase.detail.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" - {s}")).unwrap_or_default();
                    format!("{label}: {}{detail}", phase.status)
                }
                None => format!("{label}: Not recorded"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ystudio_core::results::Phase;

    #[test]
    fn missing_tests_are_explicit_and_do_not_qualify_as_validated() {
        let checks = validation_checks(None);
        assert_eq!(checks.len(), VALIDATION_PHASES.len());
        assert!(checks.iter().all(|s| s.ends_with(": Not recorded")));
    }

    #[test]
    fn dialog_reports_skipped_failed_and_completed_checks_with_details() {
        let result = RecipeResult {
            phases: vec![
                Phase { name: Some("PostInstallDetect".into()), status: "Pass".into(), ..Default::default() },
                Phase { name: Some("Uninstall".into()), status: "Skip".into(), detail: Some("-SkipUninstall was specified".into()), ..Default::default() },
                Phase { name: Some("Residue".into()), status: "Fail".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let checks = validation_checks(Some(&result));
        assert!(checks.contains(&"Post-install detection: Pass".into()));
        assert!(checks.contains(&"Uninstall: Skip - -SkipUninstall was specified".into()));
        assert!(checks.contains(&"Residue: Fail".into()));
        assert!(checks.contains(&"Post-uninstall detection: Not recorded".into()));
    }
}
