//! Promote: Publish-Yardstick.ps1 from this development workspace to a
//! production folder, previewed with -WhatIf first.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::harness::{self, Consent};
use ystudio_core::results::Verdict;
use ystudio_core::workspace::RecipeLocation;

use crate::app::{JobKind, StudioApp, job_log};
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
}

fn verdict_ok(v: Option<Verdict>) -> bool {
    matches!(v, Some(Verdict::Validated | Verdict::ValidatedWithWarnings))
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
         production folder. Preview with -WhatIf first. Only promote recipes the live install test validated.",
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
                    match app.verdicts.get(id) {
                        Some((v, ts)) => {
                            theme::badge(ui, v.label(), theme::verdict_color(*v)).on_hover_text(format!("Install test {ts}"));
                        }
                        None => {
                            theme::badge(ui, "never tested", theme::grey());
                        }
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
            app.promote.modal = true;
            app.promote.ack = false;
        }
    });
    let unvalidated: Vec<&String> = ids.iter().filter(|id| !verdict_ok(app.verdicts.get(*id).map(|v| v.0))).collect();
    if !unvalidated.is_empty() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(format!(
                "Not validated by a live install test: {}. The skill only calls a recipe validated after a full live pass.",
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
        ui.checkbox(&mut app.promote.ack, "I reviewed the preview and want to publish these recipes.");
        ui.horizontal(|ui| {
            if theme::primary(ui, app.promote.ack, RichText::new("Publish")).clicked() {
                go = true;
            }
            if ui.button("Cancel").clicked() {
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
