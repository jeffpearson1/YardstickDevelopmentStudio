//! Settings → Tenant validation (optional): the switch, the tenant this clone
//! may use, the test groups, what happens to the test app afterwards, and a
//! read-only connection check. Nothing here writes to the tenant; running a
//! recipe through Yardstick is a separate, per-run-confirmed step.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::settings::TenantAfterRun;
use ystudio_core::tenant::run::{self, Restored, RunOptions, RunResult, TenantConsent, TenantLedger, TenantStatus};
use ystudio_core::tenant::{self, CheckLevel, PreflightReport};

use super::settings::{GROUP_GAP, form_row};
use crate::app::{JobKind, StudioApp};
use crate::theme::{self, FluentText as _};

#[derive(Default)]
pub struct TenantState {
    /// The test-group editor's text, read from the settings on first use.
    groups_text: Option<String>,
    check: CheckState,
    /// The installer was opened in a console and has not been reloaded since.
    connect_started: bool,
    /// What each recipe's last tenant run found, from `_studio\tenant-validated.json`.
    ledger: TenantLedger,
    /// The ledger's verdicts against the recipes' current contents, by lower-case id.
    status: BTreeMap<String, TenantStatus>,
    /// The most recent tenant run job, for its log.
    pub job: Option<u64>,
    /// The per-run consent dialog, while it is open.
    modal: Option<RunModal>,
    /// The last run's result for each recipe, this session.
    last: BTreeMap<String, RunResult>,
    /// The confirmation in front of removing a test app from Intune.
    remove_modal: Option<RemoveModal>,
    /// The most recent removal job.
    remove_job: Option<u64>,
    /// A confirmed run (recipe id, -Force) waiting for the connection check that
    /// resolves its test groups to ids.
    pending_run: Option<(String, bool)>,
}

/// The confirmation for removing one test app.
struct RemoveModal {
    id: String,
    app_name: String,
    app_id: String,
}

/// The consent dialog for one tenant run.
struct RunModal {
    id: String,
    /// The recipe's display name.
    name: String,
    ack: bool,
    force: bool,
}

#[derive(Default)]
enum CheckState {
    #[default]
    Idle,
    Running,
    Done(Box<PreflightReport>),
    Failed(String),
}

/// How the open workspace relates to a tenant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloneState {
    NoWorkspace,
    Connected,
    /// Production: never connectable from here.
    Production,
    /// A development-only or unconfigured clone.
    NotConnected,
}

fn clone_state(app: &StudioApp) -> CloneState {
    match &app.ws {
        None => CloneState::NoWorkspace,
        Some(ws) if ws.tenant_connected() => CloneState::Connected,
        Some(ws) if !ws.is_writable() => CloneState::Production,
        Some(_) => CloneState::NotConnected,
    }
}

pub fn card(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Tenant validation (optional)");
        ui.label(
            RichText::new(
                "Run a recipe through Yardstick on your own Intune tenant before you publish it. It is off by default, \
                 never part of the normal checks, and nothing is uploaded until you start a run and confirm it.",
            )
            .weak(),
        );
        ui.add_space(GROUP_GAP - 6.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        let r = ui
            .checkbox(&mut app.settings.tenant.enabled, "Allow running a recipe on my tenant before publishing")
            .on_hover_text("Shows a Tenant step on a recipe's Test tab. Publishing never requires it.");
        if r.changed() {
            app.save_settings();
        }
        if !app.settings.tenant.enabled {
            return;
        }
        ui.add_space(4.0);
        connection_row(app, ui);
        settings_rows(app, ui);
        ui.add_space(GROUP_GAP - 6.0);
        check_row(app, ui);
    });
}

fn connection_row(app: &mut StudioApp, ui: &mut Ui) {
    let state = clone_state(app);
    let mut connect = false;
    let mut reload = false;
    let started = app.tenant_ui.connect_started;
    form_row(ui, "This clone", Some("Tenant runs need a development clone that is a full Yardstick instance."), |ui| {
        match state {
            CloneState::NoWorkspace => {
                ui.label(RichText::new("No workspace is open. Open a development clone on the Home page.").weak());
            }
            CloneState::Production => {
                ui.label(RichText::new("The open folder is production, which the Studio never runs recipes in. Open a development clone.").color(theme::amber()));
            }
            CloneState::Connected => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("\u{E73E}").color(theme::green()));
                    ui.label("Connected: a full Yardstick instance that you have trusted as a development clone.");
                });
            }
            CloneState::NotConnected => {
                ui.label(
                    RichText::new("Not connected. This clone is development-only or not set up yet, so Yardstick cannot sign in to Intune from it.")
                        .color(theme::amber()),
                );
                ui.label(
                    RichText::new(
                        "Connecting opens Yardstick's installer in a console and lists this clone under Trusted development folders. \
                         When it asks about a scheduled task, answer No: a task would deploy every recipe in this clone.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    connect = ui.button("\u{E768}  Connect this clone to Intune…").clicked();
                    if started {
                        reload = ui.button("\u{E72C}  I have finished: reload").clicked();
                    }
                });
            }
        }
    });
    if connect {
        connect_clone(app);
    }
    if reload {
        app.reload_workspace();
        app.tenant_ui.connect_started = false;
        if app.ws.as_ref().is_some_and(|w| w.tenant_connected()) {
            app.notify("This clone is connected to Intune.");
        } else {
            app.error("The clone is still development-only. Run the installer again and answer the questions.");
        }
    }
}

fn settings_rows(app: &mut StudioApp, ui: &mut Ui) {
    let mut save = false;
    form_row(ui, "Tenant id", Some("The Entra tenant this clone may use. The stored Intune credential must belong to it."), |ui| {
        let t = &mut app.settings.tenant;
        let r = ui.add(egui::TextEdit::singleline(&mut t.tenant_id).hint_text("00000000-0000-0000-0000-000000000000").desired_width(f32::INFINITY));
        if r.lost_focus() {
            t.tenant_id = t.tenant_id.trim().to_string();
            save = true;
        }
        if !t.tenant_id.trim().is_empty() && !tenant::is_tenant_id(t.tenant_id.trim()) {
            ui.label(RichText::new("That is not a tenant id (a GUID).").small().color(theme::red()));
        }
    });
    form_row(ui, "Tenant name", Some("Optional. When set, the organization's display name must match it too."), |ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut app.settings.tenant.tenant_name).hint_text("optional").desired_width(f32::INFINITY));
        if r.lost_focus() {
            app.settings.tenant.tenant_name = app.settings.tenant.tenant_name.trim().to_string();
            save = true;
        }
    });
    form_row(ui, "Test groups", Some("Entra groups (display name or object id), one per line, that receive the test app."), |ui| {
        let text = app.tenant_ui.groups_text.get_or_insert_with(|| app.settings.tenant.test_groups.join("\n"));
        let r = ui.add(egui::TextEdit::multiline(text).desired_rows(2).hint_text("e.g. Intune App Testers").desired_width(f32::INFINITY));
        if r.lost_focus() {
            app.settings.tenant.test_groups = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
            save = true;
        }
        if let Some(problem) = app.settings.tenant.test_groups.iter().find_map(|g| tenant::group_problem(g)) {
            ui.label(RichText::new(problem).small().color(theme::red()));
        }
    });
    form_row(ui, "After a run", Some("What happens to the app a tenant run creates."), |ui| {
        for choice in TenantAfterRun::ALL {
            if ui.radio_value(&mut app.settings.tenant.after_run, choice, choice.title()).changed() {
                save = true;
            }
        }
        if app.settings.tenant.after_run == TenantAfterRun::Remove
            && ui.checkbox(&mut app.settings.tenant.ask_before_remove, "Ask me before removing it").changed()
        {
            save = true;
        }
        let note = match app.settings.tenant.after_run {
            TenantAfterRun::LeaveInTestGroup => "The app stays in Intune, assigned only to the test groups, so you can try it on a device.",
            TenantAfterRun::LeaveUnassigned => "The app stays in Intune and no device receives it.",
            TenantAfterRun::Remove => "Only the app that run created is removed, never another app.",
        };
        ui.label(RichText::new(note).small().weak());
    });
    if save {
        app.save_settings();
    }
}

fn check_row(app: &mut StudioApp, ui: &mut Ui) {
    let readiness = match &app.ws {
        Some(ws) => tenant::ensure_ready(ws, &app.settings.tenant, "check the tenant connection").map_err(|e| format!("{e:#}")),
        None => Err("Open a development clone first.".into()),
    };
    let running = matches!(app.tenant_ui.check, CheckState::Running);
    let idle = !crate::workflow::busy(app) && !running;
    let mut start = false;
    ui.horizontal(|ui| {
        let r = theme::primary(ui, readiness.is_ok() && idle, "\u{E895}  Check connection")
            .on_hover_text("Signs in with the clone's Intune credential and reads the tenant and test groups. It changes nothing.");
        start = r.clicked() && readiness.is_ok() && idle;
        if running {
            ui.spinner();
            ui.label("Checking…");
        }
    });
    if let Err(why) = &readiness {
        ui.label(RichText::new(why).small().weak());
    }
    if start {
        start_check(app);
    }
    match &app.tenant_ui.check {
        CheckState::Idle | CheckState::Running => {}
        CheckState::Failed(e) => {
            ui.label(RichText::new(e).color(theme::red()));
        }
        CheckState::Done(report) => {
            for check in &report.checks {
                let (glyph, color) = match check.level {
                    CheckLevel::Pass => ("\u{E73E}", theme::green()),
                    CheckLevel::Warn => ("\u{E7BA}", theme::amber()),
                    CheckLevel::Fail => ("\u{E711}", theme::red()),
                    CheckLevel::Skip => ("\u{E738}", theme::grey()),
                };
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(glyph).color(color));
                    ui.add(egui::Label::new(&check.detail).wrap());
                });
            }
            let (text, color) = if report.passed() { ("Ready for a tenant run.", theme::green()) } else { ("Not ready: fix the items marked above.", theme::red()) };
            ui.label(RichText::new(text).semibold().color(color));
        }
    }
}

fn start_check(app: &mut StudioApp) {
    let Some(ws) = app.ws.clone() else { return };
    match tenant::preflight_spec(&ws, &app.settings.tenant, &app.pwsh(), &tenant::work_dir()) {
        Ok(run) => {
            let result = run.result.clone();
            if app.start_background_job(run.spec, JobKind::TenantCheck { result }).is_some() {
                app.tenant_ui.check = CheckState::Running;
            }
        }
        Err(e) => app.tenant_ui.check = CheckState::Failed(format!("{e:#}")),
    }
}

/// The connection check finished: read its report.
pub fn check_finished(app: &mut StudioApp, ok: bool, result: &Path) {
    app.tenant_ui.check = match PreflightReport::read(result) {
        Ok(report) => CheckState::Done(Box::new(report)),
        Err(_) if !ok => CheckState::Failed("The check stopped before it could report. Its output is in the console.".into()),
        Err(e) => CheckState::Failed(format!("{e:#}")),
    };
    let _ = std::fs::remove_file(result);
    let pending = app.tenant_ui.pending_run.take();
    let (passed, summary, groups) = match &app.tenant_ui.check {
        CheckState::Done(report) => (report.passed(), report.summary(), report.groups.clone()),
        CheckState::Failed(why) => (false, why.clone(), Vec::new()),
        _ => (false, String::new(), Vec::new()),
    };
    match pending {
        // A confirmed run was waiting for this check.
        Some((id, force)) if passed => start_run(app, &id, force, &groups),
        Some(_) => app.error(format!("The tenant connection check did not pass, so nothing was uploaded: {summary}")),
        None if passed => app.notify(summary),
        None => app.error(summary),
    }
}

/// Lists the open clone as trusted, then opens Yardstick's installer to make it
/// a full instance. Trust comes first, or the Studio would class the connected
/// clone as production and lock it the moment the installer finishes.
fn connect_clone(app: &mut StudioApp) {
    let Some(ws) = app.ws.clone() else { return };
    let spec = match tenant::connect_spec(&ws, &app.pwsh()) {
        Ok(spec) => spec,
        Err(e) => return app.error(format!("{e:#}")),
    };
    if !app.settings.trusted_dev_paths.iter().any(|p| ystudio_core::same_path(p, &ws.root)) {
        app.settings.trusted_dev_paths.push(ws.root.clone());
        app.save_settings();
        app.settings_ui.sync(&app.settings);
    }
    match ystudio_core::process::launch_detached(&spec) {
        Ok(()) => {
            app.tenant_ui.connect_started = true;
            app.notify("The installer opened in a new console. Reload the clone here when it finishes.");
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

// ---- running a recipe on the tenant -------------------------------------------------

/// Re-reads the tenant ledger and judges each recipe against what it says now.
pub fn refresh_status(app: &mut StudioApp) {
    let Some(ws) = &app.ws else {
        app.tenant_ui.status.clear();
        return;
    };
    let ledger = TenantLedger::load(&ws.layout.harness_artifacts);
    let status = app.recipes.iter().filter(|r| ledger.get(&r.id).is_some()).map(|r| (r.id.to_lowercase(), ledger.status(&r.id, &r.path))).collect();
    app.tenant_ui.ledger = ledger;
    app.tenant_ui.status = status;
}

/// What the last tenant run of `id` found, against the recipe as it is now.
pub fn status<'a>(app: &'a StudioApp, id: &str) -> Option<&'a TenantStatus> {
    app.tenant_ui.status.get(&id.to_lowercase())
}

/// A badge for the recipe header: nothing when the feature is off or the recipe
/// has never run on the tenant. Informational; nothing depends on it.
pub fn status_badge(app: &StudioApp, ui: &mut Ui, id: &str) {
    if !app.settings.tenant.enabled {
        return;
    }
    match status(app, id) {
        None | Some(TenantStatus::NotRun) => {}
        Some(TenantStatus::Passed(r)) => {
            theme::badge(ui, "Tenant: passed", theme::green())
                .on_hover_text(format!("Ran on the tenant as {} {} on {}, and is unchanged since.", r.app_name, r.version, r.tested));
        }
        Some(TenantStatus::Failed(r)) => {
            theme::badge(ui, "Tenant: failed", theme::red()).on_hover_text(format!("The last tenant run found a problem: {}", r.problem));
        }
        Some(TenantStatus::Changed(r)) => {
            theme::badge(ui, "Changed since tenant run", theme::amber()).on_hover_text(format!("Ran on the tenant on {}, but has been edited since.", r.tested));
        }
        Some(TenantStatus::Removed(r)) => {
            theme::badge(ui, "Tenant: removed", theme::grey()).on_hover_text(format!("Passed on the tenant on {}; the test app was removed afterwards.", r.tested));
        }
    }
}

/// One line per recipe about its tenant run, for the Publish dialog. Empty when
/// the feature is off. Purely informational.
pub fn publish_lines(app: &StudioApp, ids: &[String]) -> Vec<(String, String)> {
    if !app.settings.tenant.enabled {
        return Vec::new();
    }
    ids.iter()
        .map(|id| {
            let text = match status(app, id) {
                None | Some(TenantStatus::NotRun) => "Not run on the tenant.".to_string(),
                Some(TenantStatus::Passed(r)) => format!("Passed on the tenant on {}.", r.tested),
                Some(TenantStatus::Failed(r)) => format!("The last tenant run found a problem: {}", r.problem),
                Some(TenantStatus::Changed(r)) => format!("Ran on the tenant on {}, but has changed since.", r.tested),
                Some(TenantStatus::Removed(r)) => format!("Passed on the tenant on {}; the test app was removed.", r.tested),
            };
            (id.clone(), text)
        })
        .collect()
}

fn running(app: &StudioApp) -> bool {
    app.tenant_ui.job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running())
}

/// Whether a tenant run of the open recipe could start now, and if not, why.
fn run_readiness(app: &StudioApp) -> Result<(), String> {
    let Some(ws) = &app.ws else { return Err("Open a development clone first.".into()) };
    tenant::ensure_ready(ws, &app.settings.tenant, "run a recipe on the tenant").map_err(|e| format!("{e:#}"))?;
    let Some(r) = &app.recipe else { return Err("Open a recipe first.".into()) };
    if r.location != ystudio_core::workspace::RecipeLocation::Development {
        return Err("Only a development recipe can be run on the tenant. Copy it to Development first.".into());
    }
    if r.dirty() {
        return Err("Save the recipe first (Ctrl+S): a tenant run uses the saved file.".into());
    }
    if crate::workflow::busy(app) || !app.batch.queue.is_empty() {
        return Err("Finish the active run first.".into());
    }
    Ok(())
}

/// The Tenant card on a recipe's Test tab. Outside the chained checks on purpose.
pub fn bench_card(app: &mut StudioApp, ui: &mut Ui, id: &str) {
    if !app.settings.tenant.enabled {
        return;
    }
    let readiness = run_readiness(app);
    let active = running(app);
    let mut open = false;
    let mut stop = false;
    // A row of the Test tab's checklist, set apart by a rule: optional, and
    // never part of Run all checks.
    ui.separator();
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(RichText::new("\u{E753}").size(16.0).color(theme::blue()));
        ui.label("Tenant (optional)").on_hover_text(
            "Uploads this recipe to your Intune tenant as a test app through a real Yardstick run. \
             It is never part of Run all checks, and Publish does not need it.",
        );
        status_badge(app, ui, id);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if active {
                stop = ui.button("\u{E71A}  Stop").on_hover_text("Cancelling mid-upload can leave a partly uploaded app in Intune.").clicked();
            } else {
                open = ui.add_enabled(readiness.is_ok(), egui::Button::new("Run on tenant…")).clicked();
            }
        });
    });
    if let Err(why) = &readiness
        && !active
    {
        ui.label(RichText::new(why).small().color(theme::amber()));
    }
    if stop && let Some(job) = app.tenant_ui.job.and_then(|j| app.job_mut(j)) {
        job.proc.cancel();
    }
    if open {
        open_run_modal(app);
    }
}

fn open_run_modal(app: &mut StudioApp) {
    let Some(r) = &app.recipe else { return };
    let name = ystudio_core::recipe::RecipeDoc::parse(&r.text)
        .ok()
        .and_then(|d| d.get_str("displayName"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| r.id.clone());
    app.tenant_ui.modal = Some(RunModal { id: r.id.clone(), name, ack: false, force: false });
}

/// The consent dialog in front of a tenant run. Selecting the option is not consent:
/// this exact run has to be confirmed.
pub fn run_modal(app: &mut StudioApp, ctx: &egui::Context) {
    if app.tenant_ui.modal.is_none() {
        return;
    }
    let readiness = run_readiness(app);
    let t = app.settings.tenant.clone();
    let mut start = false;
    let mut cancel = false;
    let Some(m) = app.tenant_ui.modal.as_mut() else { return };
    egui::Modal::new(egui::Id::unique("tenant-run-modal")).show(ctx, |ui| {
        ui.set_max_width(580.0);
        ui.label(RichText::new("\u{E7BA}  Run on your tenant").heading().color(theme::amber()));
        ui.label(format!(
            "This runs Yardstick for {} and UPLOADS it to your Intune tenant as a test app. Nothing is emailed and nothing is deleted.",
            m.name
        ));
        ui.add_space(6.0);
        let tenant_label = if t.tenant_name.trim().is_empty() { t.tenant_id.trim().to_string() } else { format!("{} ({})", t.tenant_name.trim(), t.tenant_id.trim()) };
        let assigned = match t.after_run {
            TenantAfterRun::LeaveInTestGroup => t.test_groups.iter().map(|g| g.trim()).filter(|g| !g.is_empty()).collect::<Vec<_>>().join(", "),
            _ => "nobody (no assignments)".into(),
        };
        egui::Grid::new("tenant-run-grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (label, value) in [
                ("Tenant", tenant_label),
                ("App in Intune", run::test_display_name(&m.name)),
                ("Assigned to", assigned),
                ("Afterwards", match (t.after_run, t.ask_before_remove) {
                    (TenantAfterRun::Remove, true) => "You will be asked, then the test app is removed.",
                    (TenantAfterRun::Remove, false) => "The test app is removed as soon as the run ends.",
                    _ => "The app stays in Intune until you remove it.",
                }.to_string()),
            ] {
                ui.label(label);
                ui.add(egui::Label::new(RichText::new(value).semibold()).wrap());
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "The test app has its own name, so it cannot change a real app's versions or assignments. \
                 Studio temporarily adds an entry for this recipe to Local\\recipeOverrides.yaml, and puts the file back when the run ends.",
            )
            .small()
            .weak(),
        );
        ui.separator();
        ui.checkbox(&mut m.force, "Upload even if Intune already has this version of the test app (-Force)");
        let who = if t.tenant_name.trim().is_empty() { t.tenant_id.trim() } else { t.tenant_name.trim() };
        ui.checkbox(&mut m.ack, format!("I consent to uploading {} to {who} for testing.", m.name));
        if let Err(why) = &readiness {
            ui.label(RichText::new(why).small().color(theme::red()));
        }
        ui.horizontal(|ui| {
            if theme::primary(ui, m.ack && readiness.is_ok(), RichText::new("Run on tenant")).clicked() {
                start = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if cancel {
        app.tenant_ui.modal = None;
    }
    if start && let Some(m) = app.tenant_ui.modal.take() {
        // Yardstick assigns test groups by id, so the read-only connection check
        // runs first and resolves them; the run starts when it passes.
        app.tenant_ui.pending_run = Some((m.id, m.force));
        start_check(app);
        match &app.tenant_ui.check {
            CheckState::Running => app.notify("Checking the tenant connection and test groups before uploading…"),
            CheckState::Failed(why) => {
                let why = format!("The tenant connection check could not start, so nothing was uploaded: {why}");
                app.tenant_ui.pending_run = None;
                app.error(why);
            }
            _ => app.tenant_ui.pending_run = None,
        }
    }
}

fn start_run(app: &mut StudioApp, id: &str, force: bool, groups: &[tenant::GroupInfo]) {
    let Some(ws) = app.ws.clone() else { return };
    let prepared = match run::run_spec(&ws, &app.settings.tenant, id, RunOptions { force_reupload: force }, groups, TenantConsent::Confirmed, &app.pwsh(), &tenant::work_dir()) {
        Ok(p) => p,
        Err(e) => return app.error(format!("{e:#}")),
    };
    let dir = prepared.dir.clone();
    let kind = JobKind::TenantRun { recipe_id: id.to_string(), dir: dir.clone(), result: prepared.result.clone() };
    match app.start_job(prepared.spec, kind) {
        Some(job) => {
            app.tenant_ui.job = Some(job);
            app.tenant_ui.last.remove(&id.to_lowercase());
            app.console_open = true;
            app.notify(format!("Running {id} on the tenant as {}…", prepared.app_name));
        }
        None => {
            // The process never started: put the overrides back straight away.
            if let Err(e) = run::restore_overrides(&dir) {
                app.error(format!("Could not restore Local\\recipeOverrides.yaml: {e:#}"));
            }
        }
    }
}

/// A tenant run ended (or was cancelled): put the overrides back, read what it
/// found and remember it.
pub fn run_finished(app: &mut StudioApp, recipe_id: &str, dir: &Path, result_path: &Path, ok: bool, started: SystemTime) {
    match run::restore_overrides(dir) {
        Ok(Restored::LeftAlone(why)) => app.error(why),
        Ok(_) => {}
        Err(e) => app.error(format!("Could not restore Local\\recipeOverrides.yaml: {e:#}")),
    }
    let key = recipe_id.to_lowercase();
    let result = match RunResult::read(result_path) {
        Ok(result) => {
            let recipe = app.ws.as_ref().and_then(|w| w.find_recipe(recipe_id));
            if let Some(recipe) = recipe {
                let tenant_id = app.settings.tenant.tenant_id.trim().to_string();
                let saved = app
                    .tenant_ui
                    .ledger
                    .record_run(recipe_id, &recipe, started, &tenant_id, &result, &result.started_utc)
                    .and_then(|recorded| if recorded { app.tenant_ui.ledger.save() } else { Ok(()) });
                if let Err(e) = saved {
                    app.error(format!("Could not record the tenant run of {recipe_id}: {e:#}"));
                }
            }
            result
        }
        Err(e) => RunResult {
            status: "Failed".into(),
            recipe_id: recipe_id.to_string(),
            problems: vec![if ok {
                format!("{e:#}")
            } else {
                format!("The run stopped before it could report. If it was cancelled, check Intune for a partly uploaded app named '{}…'.", run::TEST_PREFIX)
            }],
            ..Default::default()
        },
    };
    let _ = std::fs::remove_file(result_path);
    if result.refused() {
        app.error(format!("Nothing was uploaded: {}", result.summary()));
    } else if result.passed() {
        app.notify(format!("{recipe_id} ran on the tenant: {}", result.summary()));
    } else {
        app.error(format!("The tenant run of {recipe_id} found a problem: {}", result.summary()));
    }
    app.tenant_ui.last.insert(key, result);
    refresh_status(app);
    // "Remove it after the run": clean up whatever app the run created, pass or fail.
    if app.settings.tenant.after_run == TenantAfterRun::Remove && let Some(record) = app.tenant_ui.ledger.get(recipe_id).filter(|r| !r.app_id.is_empty() && !r.removed).cloned() {
        if app.settings.tenant.ask_before_remove {
            app.tenant_ui.remove_modal = Some(RemoveModal { id: recipe_id.to_string(), app_name: record.app_name, app_id: record.app_id });
        } else {
            start_remove(app, recipe_id);
        }
    }
}

/// Starts removing the test app the last run of `id` recorded. Callers have the
/// user's confirmation: the removal dialog, or the "remove after the run"
/// choice they confirmed in the run dialog.
fn start_remove(app: &mut StudioApp, id: &str) {
    let Some(ws) = app.ws.clone() else { return };
    let Some(record) = app.tenant_ui.ledger.get(id).cloned() else { return app.error(format!("No tenant run of {id} is recorded, so there is nothing to remove.")) };
    let prepared = match run::remove_spec(&ws, &app.settings.tenant, &record, TenantConsent::Confirmed, &app.pwsh(), &tenant::work_dir()) {
        Ok(p) => p,
        Err(e) => return app.error(format!("{e:#}")),
    };
    let kind = JobKind::TenantRemove { recipe_id: id.to_string(), result: prepared.result };
    if let Some(job) = app.start_job(prepared.spec, kind) {
        app.tenant_ui.remove_job = Some(job);
        app.console_open = true;
        app.notify(format!("Removing {} from Intune…", record.app_name));
    }
}

/// A removal ended: record it, or say why nothing was removed.
pub fn remove_finished(app: &mut StudioApp, recipe_id: &str, ok: bool, result_path: &Path) {
    let result = run::RemoveResult::read(result_path);
    let _ = std::fs::remove_file(result_path);
    match result {
        Ok(r) if r.removed() => {
            if app.tenant_ui.ledger.mark_removed(recipe_id)
                && let Err(e) = app.tenant_ui.ledger.save()
            {
                app.error(format!("The test app was removed, but recording it failed: {e:#}"));
            }
            refresh_status(app);
            app.notify(r.summary());
        }
        Ok(r) => app.error(format!("Nothing was removed: {}", r.summary())),
        Err(e) if ok => app.error(format!("{e:#}")),
        Err(_) => app.error("The removal stopped before it could report. Check Intune for the test app."),
    }
}

/// The confirmation in front of removing a test app.
pub fn remove_dialog(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(m) = &app.tenant_ui.remove_modal else { return };
    let (id, app_name, app_id) = (m.id.clone(), m.app_name.clone(), m.app_id.clone());
    let who = {
        let t = &app.settings.tenant;
        if t.tenant_name.trim().is_empty() { t.tenant_id.trim().to_string() } else { t.tenant_name.trim().to_string() }
    };
    let busy = crate::workflow::busy(app);
    let mut remove = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("tenant-remove-modal")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.label(RichText::new("\u{E74D}  Remove the test app?").heading().color(theme::red()));
        ui.label(format!("This deletes {app_name} from {who}. It removes only that app and nothing else; its recipe is not touched."));
        ui.label(RichText::new(format!("Intune app id {app_id}")).small().monospace().weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            remove = ui.add_enabled(!busy, egui::Button::new(RichText::new("Remove from Intune").color(theme::red()))).clicked();
            cancel = ui.button("Keep it").clicked();
        });
    });
    if cancel {
        app.tenant_ui.remove_modal = None;
    }
    if remove {
        app.tenant_ui.remove_modal = None;
        start_remove(app, &id);
    }
}

/// The latest tenant run of `id`: what it found, and the app it left behind.
pub fn result_card(app: &mut StudioApp, ui: &mut Ui, id: &str) {
    if !app.settings.tenant.enabled {
        return;
    }
    let key = id.to_lowercase();
    let last = app.tenant_ui.last.get(&key);
    let record = app.tenant_ui.ledger.get(id);
    if last.is_none() && record.is_none() {
        return;
    }
    // A test app that is still in Intune can be removed, if nothing else is running.
    let removable = record.filter(|r| !r.app_id.is_empty() && !r.removed).map(|r| (r.app_name.clone(), r.app_id.clone()));
    let can_remove = removable.is_some() && !crate::workflow::busy(app) && app.ws.as_ref().is_some_and(|w| tenant::ensure_ready(w, &app.settings.tenant, "remove a test app").is_ok());
    let removed = record.is_some_and(|r| r.removed);
    let mut remove = false;
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Latest tenant run").semibold());
            match (last, record) {
                (Some(r), _) if r.refused() => theme::badge(ui, "Not run", theme::grey()),
                (Some(r), _) if r.passed() => theme::badge(ui, "Passed", theme::green()),
                (Some(_), _) => theme::badge(ui, "Failed", theme::red()),
                (None, Some(r)) if r.passed => theme::badge(ui, "Passed", theme::green()),
                (None, _) => theme::badge(ui, "Failed", theme::red()),
            };
            if removed {
                theme::badge(ui, "Test app removed", theme::grey());
            }
            if removable.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    remove = ui
                        .add_enabled(can_remove, egui::Button::new("\u{E74D}  Remove from tenant…"))
                        .on_hover_text("Deletes this test app from Intune. Only the app this run created can be removed.")
                        .clicked();
                });
            }
        });
        let (name, version, app_id, groups, problems): (String, String, String, Vec<String>, Vec<String>) = match (last, record) {
            (Some(r), _) => {
                let a = r.app.clone().unwrap_or_default();
                let groups = a.assignments.iter().map(|g| g.group_name.clone().or_else(|| g.group_id.clone()).unwrap_or_else(|| g.target_type.clone())).collect();
                (a.display_name, a.display_version, a.id, groups, r.problems.clone())
            }
            (None, Some(r)) => (r.app_name.clone(), r.version.clone(), r.app_id.clone(), r.groups.clone(), (!r.problem.is_empty()).then(|| r.problem.clone()).into_iter().collect()),
            (None, None) => return,
        };
        if !name.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&name).semibold());
                if !version.is_empty() {
                    ui.label(format!("version {version}"));
                }
            });
            if !app_id.is_empty() {
                ui.label(RichText::new(format!("Intune app id {app_id}")).small().monospace().weak());
            }
            let assigned = if groups.is_empty() { "Assigned to nobody.".to_string() } else { format!("Assigned to {}.", groups.join(", ")) };
            ui.label(assigned);
        }
        for p in &problems {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("\u{E711}").color(theme::red()));
                ui.add(egui::Label::new(p).wrap());
            });
        }
    });
    if remove && let Some((app_name, app_id)) = removable {
        app.tenant_ui.remove_modal = Some(RemoveModal { id: id.to_string(), app_name, app_id });
    }
}

/// The most recent job for the Test tab's log: a check, a tenant run or a removal.
pub fn latest_job(app: &StudioApp, harness_job: Option<u64>) -> Option<u64> {
    [harness_job, app.tenant_ui.job, app.tenant_ui.remove_job].into_iter().flatten().max()
}

#[cfg(test)]
impl TenantState {
    pub(crate) fn show_report_for_tests(&mut self, report: PreflightReport) {
        self.check = CheckState::Done(Box::new(report));
    }

    pub(crate) fn failure_for_tests(&self) -> Option<&str> {
        match &self.check {
            CheckState::Failed(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ystudio_core::tenant::PreflightReport;
    use ystudio_core::workspace::Workspace;

    use crate::app::View;
    use crate::workflow::tests::{fake_app, render};

    const REPORT: &str = r#"{"ok": false, "tenant": {"id": "t", "name": "Contoso"}, "groups": [],
        "checks": [
          {"id": "preferences", "level": "Pass", "detail": "Local/preferences.yaml was read."},
          {"id": "shared-sync", "level": "Fail", "detail": "syncSharedContent is on. A tenant run stages a temporary entry in Local/recipeOverrides.yaml, and that file syncs both ways with the shared overrides. Turn syncSharedContent off for this clone."},
          {"id": "group:Testers", "level": "Warn", "detail": "'Testers' is a dynamic group, so who receives the app can change on its own."},
          {"id": "groups", "level": "Skip", "detail": "No test group is configured."}]}"#;

    #[test]
    fn the_card_renders_off_on_and_connected() {
        let (dir, mut app) = fake_app();
        app.view = View::Settings;
        // Off by default: one checkbox and nothing else.
        assert!(!app.settings.tenant.enabled);
        render(&mut app, "tenant-off", 1400, 1100);
        // On, but this clone is not connected.
        app.settings.tenant.enabled = true;
        render(&mut app, "tenant-on-not-connected", 1400, 1100);
        render(&mut app, "tenant-on-narrow", 900, 600);
        // Connected, with a report that has every kind of line.
        std::fs::create_dir_all(dir.path().join("Local")).unwrap();
        std::fs::write(dir.path().join("Local").join("preferences.yaml"), "defaultScopeTags: []\n").unwrap();
        app.ws = Some(Workspace::open(dir.path(), &[], &[dir.path().to_path_buf()]).unwrap());
        assert!(app.ws.as_ref().unwrap().tenant_connected());
        app.settings.tenant.tenant_id = "11111111-2222-3333-4444-555555555555".into();
        app.settings.tenant.test_groups = vec!["Testers".into()];
        app.settings.tenant.after_run = ystudio_core::settings::TenantAfterRun::Remove;
        app.tenant_ui.show_report_for_tests(serde_json::from_str::<PreflightReport>(REPORT).unwrap());
        render(&mut app, "tenant-connected-report", 1400, 1300);
        render(&mut app, "tenant-connected-narrow", 900, 600);
    }

    #[test]
    fn a_check_is_refused_before_any_process_starts_when_the_gate_says_no() {
        let (_dir, mut app) = fake_app();
        app.settings.tenant.enabled = true;
        app.settings.tenant.tenant_id = "11111111-2222-3333-4444-555555555555".into();
        // fake_app's clone has no preferences, so it is not connected.
        super::start_check(&mut app);
        assert!(app.tenant_ui.failure_for_tests().is_some_and(|e| e.contains("not a trusted development clone")));
        assert!(app.jobs.is_empty(), "no job may start when the gate refuses");
    }

    const PASSED: &str = r##"{"status": "Passed", "recipeId": "app", "exitCode": 0, "startedUtc": "2026-11-23T10:00:00Z",
        "tenant": {"id": "t", "name": "Contoso"}, "groups": [{"id": "g1", "name": "Testers"}],
        "app": {"id": "0f9c1c4e-1111-4222-8333-444455556666", "displayName": "YDS TEST - App", "displayVersion": "1.0", "publishingState": "published",
                "createdDateTime": "2026-11-23T10:05:00Z",
                "assignments": [{"intent": "available", "targetType": "#microsoft.graph.groupAssignmentTarget", "groupId": "g1", "groupName": "Testers"}]},
        "problems": []}"##;

    /// Makes `fake_app`'s clone a trusted full instance with the feature switched on.
    fn connect(dir: &tempfile::TempDir, app: &mut crate::app::StudioApp) {
        std::fs::create_dir_all(dir.path().join("Local")).unwrap();
        std::fs::write(dir.path().join("Local").join("preferences.yaml"), "defaultScopeTags: []\n").unwrap();
        app.ws = Some(Workspace::open(dir.path(), &[], &[dir.path().to_path_buf()]).unwrap());
        app.settings.tenant.enabled = true;
        app.settings.tenant.tenant_id = "11111111-2222-3333-4444-555555555555".into();
        app.settings.tenant.test_groups = vec!["Testers".into()];
        app.refresh_recipes();
    }

    /// Tenant runs share the process-wide work folder, so tests that start one
    /// (and check that nothing is left to recover) take turns.
    static RUNS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn one_run_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        RUNS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    const TESTERS_ID: &str = "0b0e2f1c-7b1a-4e33-9d43-6f0f6f1a2b3c";

    /// What the connection check reports for `connect`'s test group.
    fn testers() -> Vec<ystudio_core::tenant::GroupInfo> {
        vec![ystudio_core::tenant::GroupInfo { input: "Testers".into(), id: TESTERS_ID.into(), name: "Testers".into() }]
    }

    fn finish(app: &mut crate::app::StudioApp, job: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while app.job(job).unwrap().proc.is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        app.poll_for_tests();
        assert!(!app.job(job).unwrap().proc.is_running(), "the job did not finish");
    }

    /// The real run script, in a clone with no Yardstick modules: a guard refuses
    /// before anything reaches the tenant, which is exactly what must be safe.
    #[test]
    fn a_run_that_a_guard_stops_uploads_nothing_and_restores_the_overrides() {
        let _turn = one_run_at_a_time();
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        let overrides = dir.path().join("Local").join("recipeOverrides.yaml");
        let original = "# mine\r\nother:\r\n  scopeTags: [Labs]\r\n";
        std::fs::write(&overrides, original).unwrap();
        // A Yardstick.ps1 that would leave a trace if the run ever reached it.
        std::fs::write(dir.path().join("Yardstick.ps1"), "Set-Content -LiteralPath (Join-Path $PSScriptRoot 'ran.txt') -Value ran").unwrap();

        super::start_run(&mut app, "app", false, &testers());
        let job = app.tenant_ui.job.expect("the run should have started");
        assert!(crate::workflow::busy(&app), "a tenant run blocks other runs");
        // While it runs, the overrides hold the temporary test entry.
        finish(&mut app, job);

        assert!(!dir.path().join("ran.txt").exists(), "Yardstick must not run when a guard fails");
        assert_eq!(std::fs::read(&overrides).unwrap(), original.as_bytes(), "the overrides are put back byte for byte");
        let last = &app.tenant_ui.last["app"];
        assert!(!last.passed() && last.app.is_none());
        // It reports why it stopped, rather than "stopped before it could report".
        assert!(!last.problems.is_empty() && !last.problems[0].contains("could report"), "{:?}", last.problems);
        assert!(last.refused() || last.status == "Failed", "{}", last.status);
        assert!(app.tenant_ui.ledger.get("app").is_none() || !app.tenant_ui.ledger.get("app").unwrap().passed);
        assert!(!crate::workflow::busy(&app), "the lock is released when it ends");
        assert!(ystudio_core::tenant::run::recover_interrupted(&ystudio_core::tenant::work_dir()).is_empty(), "nothing is left to recover");
    }

    /// A confirmed run waits for the connection check, and starts only when it
    /// passes, with the group ids it resolved rather than the names in Settings.
    #[test]
    fn a_confirmed_run_waits_for_the_check_and_stages_group_ids() {
        let _turn = one_run_at_a_time();
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        let overrides = dir.path().join("Local").join("recipeOverrides.yaml");
        let report = dir.path().join("check.json");
        // A failed check: nothing starts and the pending run is dropped.
        app.tenant_ui.pending_run = Some(("app".into(), false));
        std::fs::write(&report, r#"{"ok": false, "tenant": null, "groups": [], "checks": [{"id": "groups", "level": "Fail", "detail": "0 group(s) match 'Testers'"}]}"#).unwrap();
        super::check_finished(&mut app, true, &report);
        assert!(app.jobs.is_empty() && !overrides.exists() && app.tenant_ui.pending_run.is_none());
        // A passed check: the run starts with the resolved id staged.
        app.tenant_ui.pending_run = Some(("app".into(), false));
        let passed = format!(
            r#"{{"ok": true, "tenant": {{"id": "11111111-2222-3333-4444-555555555555", "name": "Contoso"}}, "groups": [{{"input": "Testers", "id": "{TESTERS_ID}", "name": "Testers"}}], "checks": []}}"#
        );
        std::fs::write(&report, passed).unwrap();
        super::check_finished(&mut app, true, &report);
        let job = app.tenant_ui.job.expect("the run should have started after the check");
        let staged = std::fs::read_to_string(&overrides).unwrap();
        assert!(staged.contains(TESTERS_ID) && !staged.contains("- Testers"), "{staged}");
        finish(&mut app, job);
        assert!(!overrides.exists(), "the overrides are put back");
    }

    #[test]
    fn a_run_is_refused_without_consent_before_anything_changes() {
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        let overrides = dir.path().join("Local").join("recipeOverrides.yaml");
        // Feature off: start_run goes through the same gate and starts nothing.
        app.settings.tenant.enabled = false;
        super::start_run(&mut app, "app", false, &testers());
        assert!(app.jobs.is_empty() && !overrides.exists());
        // Production: the same.
        app.settings.tenant.enabled = true;
        app.ws = Some(Workspace::open(dir.path(), &[dir.path().to_path_buf()], &[]).unwrap());
        super::start_run(&mut app, "app", false, &testers());
        assert!(app.jobs.is_empty() && !overrides.exists());
    }

    #[test]
    fn the_dialog_the_card_and_the_result_render() {
        use ystudio_core::tenant::run::RunResult;
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        let recipe = dir.path().join(r"Recipes\Development\app.yaml");
        app.open_recipe(&recipe);
        app.view = View::TestBench;
        render(&mut app, "tenant-bench-card", 1400, 900);
        // The consent dialog.
        super::open_run_modal(&mut app);
        render(&mut app, "tenant-run-dialog", 1400, 900);
        render(&mut app, "tenant-run-dialog-narrow", 900, 600);
        app.tenant_ui.modal = None;
        // A passed run, then a failed one.
        let passed: RunResult = serde_json::from_str(PASSED).unwrap();
        app.tenant_ui.last.insert("app".into(), passed);
        render(&mut app, "tenant-result-passed", 1400, 900);
        let failed: RunResult = serde_json::from_str(r#"{"status": "Failed", "problems": ["The app has an assignment outside the test groups. Check it in Intune before leaving it.", "Yardstick exited with code 40."]}"#).unwrap();
        app.tenant_ui.last.insert("app".into(), failed);
        render(&mut app, "tenant-result-failed", 1400, 900);
        // An unsaved recipe cannot be run: the card says why.
        app.recipe.as_mut().unwrap().text.push_str("# edit\n");
        assert!(super::run_readiness(&app).unwrap_err().contains("Save the recipe"));
        render(&mut app, "tenant-bench-card-dirty", 1400, 900);
    }

    /// A finished run's result, written where the job would have left it.
    fn finished_run(app: &mut crate::app::StudioApp, dir: &tempfile::TempDir) {
        let run_dir = dir.path().join("run-dir");
        std::fs::create_dir_all(&run_dir).unwrap();
        let result = run_dir.join("result.json");
        std::fs::write(&result, PASSED).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        super::run_finished(app, "app", &run_dir, &result, true, later);
    }

    #[test]
    fn remove_after_the_run_asks_first_or_goes_straight_to_the_removal() {
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        app.settings.tenant.after_run = ystudio_core::settings::TenantAfterRun::Remove;
        // Asking: the dialog opens, nothing starts.
        app.settings.tenant.ask_before_remove = true;
        finished_run(&mut app, &dir);
        assert!(app.tenant_ui.ledger.get("app").is_some_and(|r| r.passed && !r.app_id.is_empty()));
        let modal = app.tenant_ui.remove_modal.as_ref().expect("asks before removing");
        assert_eq!((modal.app_name.as_str(), modal.id.as_str()), ("YDS TEST - App", "app"));
        assert!(app.jobs.is_empty());
        render(&mut app, "tenant-remove-dialog", 1400, 900);
        app.tenant_ui.remove_modal = None;
        // Not asking: the removal starts at once. This clone has no Yardstick modules,
        // so the script's own checks stop it before anything is sent to Intune.
        app.settings.tenant.ask_before_remove = false;
        finished_run(&mut app, &dir);
        let job = app.tenant_ui.remove_job.expect("removal started");
        assert!(crate::workflow::busy(&app), "a removal blocks other runs");
        finish(&mut app, job);
        assert!(!app.tenant_ui.ledger.get("app").unwrap().removed, "a refused removal removes nothing");
        assert!(app.tenant_ui.remove_modal.is_none());
    }

    #[test]
    fn a_leave_it_run_never_offers_removal_and_a_removal_is_recorded() {
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        app.settings.tenant.after_run = ystudio_core::settings::TenantAfterRun::LeaveInTestGroup;
        finished_run(&mut app, &dir);
        assert!(app.tenant_ui.remove_modal.is_none() && app.jobs.is_empty());
        // The result card offers it by hand.
        app.open_recipe(&dir.path().join(r"Recipes\Development\app.yaml"));
        app.view = View::TestBench;
        render(&mut app, "tenant-result-removable", 1400, 900);
        // A "Removed" report marks the ledger and the badge.
        let report = dir.path().join("removed.json");
        std::fs::write(&report, r#"{"status": "Removed", "appId": "0f9c1c4e-1111-4222-8333-444455556666", "appName": "YDS TEST - App", "problems": []}"#).unwrap();
        super::remove_finished(&mut app, "app", true, &report);
        assert!(app.tenant_ui.ledger.get("app").unwrap().removed);
        assert!(matches!(super::status(&app, "app"), Some(ystudio_core::tenant::run::TenantStatus::Removed(_))));
        render(&mut app, "tenant-result-removed", 1400, 900);
        // A refusal is reported and changes nothing.
        let mut other = fake_app();
        connect(&other.0, &mut other.1);
        finished_run(&mut other.1, &other.0);
        let refused = other.0.path().join("refused.json");
        std::fs::write(&refused, r#"{"status": "Refused", "problems": ["The app is now called 'x', which is not a test app, so it was not removed."]}"#).unwrap();
        super::remove_finished(&mut other.1, "app", false, &refused);
        assert!(!other.1.tenant_ui.ledger.get("app").unwrap().removed);
    }

    #[test]
    fn publish_lists_tenant_state_only_when_the_feature_is_on() {
        let (dir, mut app) = fake_app();
        connect(&dir, &mut app);
        let ids = vec!["app".to_string()];
        assert_eq!(super::publish_lines(&app, &ids)[0].1, "Not run on the tenant.");
        finished_run(&mut app, &dir);
        assert!(super::publish_lines(&app, &ids)[0].1.starts_with("Passed on the tenant"));
        app.settings.tenant.enabled = false;
        assert!(super::publish_lines(&app, &ids).is_empty());
    }

    #[test]
    fn a_tenant_check_blocks_other_runs_while_it_is_active() {
        use crate::app::JobKind;
        let (dir, mut app) = fake_app();
        assert!(!crate::workflow::busy(&app));
        let spec = ystudio_core::process::ProcessSpec::new("wait", "cmd", dir.path()).args(["/d", "/c", "ping -n 3 127.0.0.1 >nul"]);
        let id = app.start_job(spec, JobKind::TenantCheck { result: dir.path().join("r.json") }).unwrap();
        assert!(crate::workflow::busy(&app));
        app.job_mut(id).unwrap().proc.cancel();
    }
}
