//! Publish: each selected development recipe becomes a pull request on
//! GitHub (see `ystudio_core::promote`). Production pulls merged changes
//! itself; the Studio never writes to a production folder.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::process::{ProcessSpec, RunningProcess, Stream};
use ystudio_core::promote::{self, Outcome, Repo};
use ystudio_core::results::{RecipeResult, VALIDATION_PHASES};
use ystudio_core::validation::Completion;
use ystudio_core::workspace::RecipeLocation;

use crate::app::{JobKind, Step, StepState, StudioApp, job_log};
use crate::theme::{self, FluentText as _};

/// Each recipe's id and what promoting it produced.
type Results = Vec<(String, Result<Outcome, String>)>;

#[derive(Default)]
pub struct PromoteState {
    pub(crate) selected: BTreeSet<String>,
    /// The confirmation dialog is open.
    pub(crate) modal: bool,
    job: Option<u64>,
    /// Each recipe's outcome, when the background run finishes.
    rx: Option<Receiver<Results>>,
    /// Pull requests and compare pages waiting for "open in the browser?".
    pub(crate) prompts: Vec<(String, Outcome)>,
    /// The upstream repository and `gh`, read once per workspace.
    pub(crate) repo: Option<(PathBuf, Result<Repo, String>, Option<PathBuf>)>,
    /// The open recipe last pre-selected, so unticking it sticks.
    followed: Option<String>,
}

impl PromoteState {
    pub fn running(&self) -> bool {
        self.rx.is_some()
    }
}

/// The repository, re-read when the workspace changes.
fn repo(app: &mut StudioApp) -> Option<(Result<Repo, String>, Option<PathBuf>)> {
    let root = app.ws.as_ref()?.root.clone();
    if app.promote.repo.as_ref().is_none_or(|(r, ..)| *r != root) {
        let repo = promote::repo(app.ws.as_ref()?).map_err(|e| format!("{e:#}"));
        let gh = ystudio_core::process::find_program("gh", &[]);
        app.promote.repo = Some((root, repo, gh));
    }
    app.promote.repo.as_ref().map(|(_, r, gh)| (r.clone(), gh.clone()))
}

/// What the commit and pull request say about testing.
fn evidence(app: &StudioApp, id: &str) -> String {
    match app.completion(id) {
        Completion::Complete(r) => format!(
            "Validated: a live install test passed{} on {} with exactly this recipe (SHA-256 {}).",
            if r.warnings { " with warnings" } else { "" },
            r.tested,
            r.sha256
        ),
        Completion::Changed(r) => format!("Not validated: a live install test passed on {}, but the recipe has changed since.", r.tested),
        Completion::NotValidated => match app.verdicts.get(id) {
            Some((v, ts)) => format!("Not validated: the latest install test ({ts}) was: {}.", v.label()),
            None => "Not validated: no install test has been recorded.".to_string(),
        },
    }
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    theme::page_title(ui, "Publish");
    ui.label(RichText::new(
        "Each selected recipe goes to GitHub as a pull request. Production picks it up once the pull request is merged.",
    ).weak());
    ui.add_space(6.0);
    let repo = repo(app);
    let share = app.ws.as_ref().and_then(|w| w.shared_icons.clone());
    theme::card(ui, |ui| {
        match &repo {
            Some((Ok(r), gh)) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Repository");
                    ui.hyperlink_to(RichText::new(&r.web_url).monospace(), &r.web_url);
                    ui.label(RichText::new(format!("→ {}", r.base)).monospace());
                });
                match gh {
                    Some(_) => ui.label(RichText::new("gh is installed: pull requests are opened for you.").small().weak()),
                    None => ui.label(RichText::new("gh is not installed: the Studio pushes each branch and offers GitHub's page to open the pull request.").small().weak()),
                };
            }
            Some((Err(e), _)) => {
                ui.colored_label(theme::red(), format!("\u{E711}  {e}"));
            }
            None => {}
        }
        match &share {
            Some(s) => ui.label(RichText::new(format!("Icons are copied to the shared icon folder {}.", s.display())).small().weak()),
            None => ui.colored_label(
                theme::amber(),
                "No shared icon folder (SharedIcons) is set in Local\\preferences.yaml, so icons will not reach production.",
            ),
        };
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
    app.promote.selected.retain(|id| dev.contains(id));
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

    let ids: Vec<String> = app.promote.selected.iter().cloned().collect();
    let running = app.promote.running();
    let repo_ok = matches!(repo, Some((Ok(_), _)));
    let ready = app.writable() && !ids.is_empty() && repo_ok && !running;
    ui.horizontal(|ui| {
        let label = match ids.len() {
            0 => "\u{E898}  Open pull requests…".to_string(),
            1 => "\u{E898}  Open pull request…".to_string(),
            n => format!("\u{E898}  Open {n} pull requests…"),
        };
        if theme::primary(ui, ready, label).on_disabled_hover_text("Select development recipes in a writable clone of the GitHub repository.").clicked() {
            app.refresh_runs();
            app.promote.modal = true;
        }
        if running {
            ui.spinner();
            ui.label("Pushing…");
        }
    });
    let incomplete: Vec<&String> = ids.iter().filter(|id| !app.completion(id).is_complete()).collect();
    if !incomplete.is_empty() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(format!(
                "Not complete (no passing live test of the current contents): {}. You will see the recorded checks and can open the pull request anyway; it says it is not validated.",
                incomplete.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        });
    }
    ui.add_space(4.0);
    // The console already shows this job; a second copy only takes space.
    let in_console = app.console_open && app.console_job.is_some() && app.console_job == app.promote.job;
    if !in_console && let Some(job) = app.promote.job.and_then(|j| app.job_mut(j)) {
        job_log(ui, &mut job.proc, "promote");
    }
}

/// Pushes a branch per recipe in the background, streaming git's output to the console.
pub(crate) fn start(app: &mut StudioApp, ids: &[String]) {
    let Some(ws) = app.ws.clone() else { return };
    let Some((Ok(repo), gh)) = repo(app) else { return };
    let mut work = Vec::new();
    for id in ids {
        match promote::plan(&ws, id) {
            Ok(plan) => work.push((plan, evidence(app, id))),
            Err(e) => app.error(format!("{id}: {e:#}")),
        }
    }
    if work.is_empty() {
        return;
    }
    let label = format!("Pull requests · {}", ids.join(", "));
    let spec = ProcessSpec::new(label, "git", &ws.root);
    let (proc, feed) = RunningProcess::remote(spec, "branch, push and open pull requests", Some(app.waker.clone()), Box::new(|| {}));
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut results = Vec::new();
        for (plan, evidence) in work {
            feed.line(Stream::Studio, format!("--- {} → {}", plan.id, plan.dest.display()));
            let result = promote::open_pull_request(&ws, &plan, &repo, gh.as_deref(), &evidence, &mut |l| feed.line(Stream::Stdout, l.to_string()));
            match &result {
                Ok(o) => feed.line(Stream::Studio, format!("{}: {}", plan.id, o.url())),
                Err(e) => feed.line(Stream::Stderr, format!("{}: {e:#}", plan.id)),
            }
            results.push((plan.id.clone(), result.map_err(|e| format!("{e:#}"))));
        }
        let ok = results.iter().all(|(_, r)| r.is_ok());
        let _ = tx.send(results);
        feed.exited(Some(if ok { 0 } else { 1 }));
    });
    app.promote.rx = Some(rx);
    app.promote.job = Some(app.add_job(proc, JobKind::PullRequests));
    app.console_open = true;
}

/// Collects finished promotions into "open in the browser?" prompts.
pub(crate) fn poll(app: &mut StudioApp) {
    let Some(results) = app.promote.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { return };
    app.promote.rx = None;
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for (id, result) in results {
        match result {
            Ok(outcome) => {
                done.push(id.clone());
                app.promote.prompts.push((id, outcome));
            }
            Err(e) => failed.push(format!("{id}: {e}")),
        }
    }
    app.promote.selected.retain(|id| !done.contains(id));
    if !failed.is_empty() {
        app.error(format!("Could not publish {}", failed.join("; ")));
    }
}

/// The confirmation before pushing, and the browser prompts after.
pub fn modals(app: &mut StudioApp, ctx: &egui::Context) {
    poll(app);
    confirm_modal(app, ctx);
    browser_prompt(app, ctx);
}

fn browser_prompt(app: &mut StudioApp, ctx: &egui::Context) {
    let Some((id, outcome)) = app.promote.prompts.first().cloned() else { return };
    let (mut open, mut close) = (false, false);
    egui::Modal::new(egui::Id::unique("promote-browser")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        let (title, text, button) = match &outcome {
            Outcome::PullRequest { existing: false, .. } => ("Pull request opened", format!("The pull request for {id} is open on GitHub."), "Open it in the browser"),
            Outcome::PullRequest { existing: true, .. } => ("Pull request updated", format!("{id} already had an open pull request; it now has the new commit."), "Open it in the browser"),
            Outcome::Compare { .. } => (
                "Branch pushed",
                format!("The branch for {id} is on GitHub. Open GitHub's page to create the pull request?"),
                "Open GitHub's page",
            ),
        };
        ui.label(RichText::new(format!("\u{E898}  {title}")).heading());
        ui.label(text);
        ui.label(RichText::new(outcome.url()).monospace().small());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            open = theme::primary(ui, true, button).clicked();
            if ui.button("Copy link").clicked() {
                ui.ctx().copy_text(outcome.url().to_string());
            }
            close = ui.button("Not now").clicked();
        });
    });
    if open {
        ctx.open_url(egui::OpenUrl::new_tab(outcome.url()));
    }
    if open || close {
        app.promote.prompts.remove(0);
    }
}

fn confirm_modal(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.promote.modal {
        return;
    }
    let ids: Vec<String> = app.promote.selected.iter().cloned().collect();
    let incomplete = ids.iter().any(|id| !app.completion(id).is_complete());
    let base = app.promote.repo.as_ref().and_then(|(_, r, _)| r.as_ref().ok()).map(|r| r.base.clone()).unwrap_or_else(|| "main".into());
    let mut go = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("publish-modal")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.label(RichText::new("\u{E898}  Open pull requests").heading());
        ui.label(format!(
            "For each recipe the Studio cuts a studio/<id> branch from {base}, commits the recipe to its shipped path, pushes it and opens a pull request. Your checkout is not changed."
        ));
        ui.label(RichText::new(ids.join(", ")).monospace());
        if incomplete {
            ui.add_space(8.0);
            ui.label(RichText::new("Validation is incomplete").strong().color(theme::amber()));
            ui.label("Some recipes have no passing live test of their current contents. The pull request will say so.");
            egui::ScrollArea::vertical().id_salt("publish-checks").max_height(320.0).show(ui, |ui| {
                for id in ids.iter().filter(|id| !app.completion(id).is_complete()) {
                    ui.add_space(8.0);
                    ui.label(RichText::new(id).strong().monospace());
                    ui.label(RichText::new(evidence(app, id)).small());
                    let latest = app.runs.iter().find_map(|run| {
                        run.summary.as_ref()?.results.iter().find(|r| r.id == *id).map(|result| (run, result))
                    });
                    if let Some((run, result)) = latest {
                        ui.label(format!("Latest install test: {} ({})", run.timestamp, result.verdict().label()));
                    }
                    for line in validation_checks(latest.map(|(_, result)| result)) {
                        ui.label(line);
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
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let label = if incomplete { "Open anyway" } else { "Open pull requests" };
            go = theme::primary(ui, true, label).clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel {
        app.promote.modal = false;
    }
    if go {
        app.promote.modal = false;
        start(app, &ids);
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
    fn missing_tests_are_explicit() {
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
