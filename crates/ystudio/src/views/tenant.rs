//! Settings → Tenant validation (optional): the switch, the tenant this clone
//! may use, the test groups, what happens to the test app afterwards, and a
//! read-only connection check. Nothing here writes to the tenant; running a
//! recipe through Yardstick is a separate, per-run-confirmed step.

use std::path::Path;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::settings::TenantAfterRun;
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
    if let CheckState::Done(report) = &app.tenant_ui.check {
        let summary = report.summary();
        if report.passed() {
            app.notify(summary);
        } else {
            app.error(summary);
        }
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
