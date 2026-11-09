//! Batch mode: read a request list (ServiceNow export, spreadsheet, email…),
//! pull out the software to package with a quick scan or an agent, then work
//! through the items - agent runs, icons, recipe matches - and report back.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::{self, AgentTask, Snapshot};
use ystudio_core::icons;
use ystudio_core::intake::{self, Batch, BatchCheck, BatchItem, ItemStatus, RequestKind};

use crate::app::{JobKind, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Default)]
pub struct BatchState {
    pub batch: Option<Batch>,
    saved: Option<Vec<(String, PathBuf)>>,
    selected: Option<usize>,
    pub extract_job: Option<u64>,
    pub run_job: Option<u64>,
    pub queue: VecDeque<usize>,
    hide_other: bool,
    show_log: bool,
    verification: Option<crate::workflow::Options>,
    /// Text being pasted as a new request list; `Some` while the window is open.
    pub paste: Option<String>,
}

impl BatchState {
    pub fn busy(&self, app_jobs: &StudioApp) -> bool {
        crate::workflow::busy(app_jobs) || !self.queue.is_empty()
    }
}

fn status_badge(ui: &mut Ui, s: ItemStatus) {
    let (t, c) = match s {
        ItemStatus::Pending => ("pending", theme::grey()),
        ItemStatus::Queued => ("queued", theme::blue()),
        ItemStatus::Running => ("running", theme::amber()),
        ItemStatus::Done => ("done", theme::green()),
        ItemStatus::Failed => ("failed", theme::red()),
        ItemStatus::Skipped => ("skipped", theme::grey()),
    };
    theme::badge(ui, t, c);
}

/// "done" covers both "the agent wrote a recipe" and "checks passed"; say which,
/// and whether the recipe is complete (the contents that passed a live test).
fn item_status_badge(app: &StudioApp, ui: &mut Ui, it: &BatchItem) {
    if it.status != ItemStatus::Done {
        return status_badge(ui, it.status);
    }
    let ids: Vec<String> = if it.recipes.is_empty() { vec![it.recipe_id()] } else { it.recipes.clone() };
    let (text, color, hover) = if ids.iter().all(|id| app.completion(id).is_complete()) {
        ("complete · not published", theme::green(), "Passed a live install test and unchanged since. Ready to publish.")
    } else if !it.checks.is_empty() && it.checks.iter().all(|c| c.passed) {
        ("checks passed", theme::blue(), "The checks that ran passed, but no live install test of the current recipe has.")
    } else {
        ("recipe written", theme::blue(), "The agent finished. Run the checks to validate it.")
    };
    theme::badge(ui, text, color).on_hover_text(hover);
}

fn request_color(r: RequestKind) -> egui::Color32 {
    match r {
        RequestKind::New => theme::accent(),
        RequestKind::Update => theme::amber(),
        RequestKind::Existing => theme::green(),
        RequestKind::NotSoftware => theme::grey(),
    }
}

fn save(app: &mut StudioApp) {
    if let Some(b) = &app.batch.batch
        && let Err(e) = b.save()
    {
        app.error(format!("Could not save the batch: {e:#}"));
    }
}

pub fn open_file(app: &mut StudioApp, path: &Path) {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active batch/agent/test run before opening another request list.");
        return;
    }
    match intake::read_document(path) {
        Ok(doc) => {
            let n = doc.records.len();
            let format = doc.format.clone();
            let batch = Batch::new(doc, &app.recipes);
            let items = batch.items.len();
            app.batch.batch = Some(batch);
            app.batch.selected = None;
            app.batch.saved = None;
            save(app);
            app.notify(format!("Read {n} request(s) from {} ({format}); the quick scan found {items} item(s).", path.display()));
        }
        Err(e) => app.error(format!("Could not read {}: {e:#}", path.display())),
    }
}

/// Opens a request made in the Studio (see [`Batch::from_items`]) under Requests.
pub(crate) fn open_made(app: &mut StudioApp, batch: Batch) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active batch/agent/test run first.");
        return false;
    }
    app.batch.batch = Some(batch);
    app.batch.selected = None;
    app.batch.saved = None;
    save(app);
    app.view = View::Batch;
    true
}

/// A new recipe as a one-item request: the agent writes it, then the Studio
/// tests and repairs it, exactly like an item from an imported list.
pub fn start_new_recipe(app: &mut StudioApp, item: BatchItem) -> bool {
    let name = format!("new {}", item.app);
    if !open_made(app, Batch::from_items(&name, vec![item])) {
        return false;
    }
    crate::workflow::request(app, crate::workflow::Request::BatchAuthor(vec![0]));
    true
}

/// Existing recipes as one request, to re-test, repair or update together.
pub fn start_update_request(app: &mut StudioApp, ids: &[String], notes: &str) -> bool {
    let items = update_items(app, ids, notes);
    let name = if ids.len() == 1 { format!("update {}", ids[0]) } else { format!("update {} recipes", ids.len()) };
    let ok = open_made(app, Batch::from_items(&name, items));
    if ok {
        app.notify("Choose what to do with them: test them, repair failures with the agent, or have the agent update them.");
    }
    ok
}

pub(crate) fn update_items(app: &StudioApp, ids: &[String], notes: &str) -> Vec<BatchItem> {
    ids.iter()
        .map(|id| {
            let name = app.recipes.iter().find(|r| &r.id == id).map(|r| r.meta.display_name.clone()).filter(|n| !n.is_empty());
            BatchItem {
                app: name.unwrap_or_else(|| id.clone()),
                request: RequestKind::Update,
                existing_recipe: id.clone(),
                notes: notes.to_string(),
                ..Default::default()
            }
        })
        .collect()
}

/// Reads text pasted into the Studio as a request list.
pub fn paste_window(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(text) = &mut app.batch.paste else { return };
    let mut open = true;
    let mut read = false;
    let mut cancel = false;
    egui::Window::new("Paste requests").open(&mut open).collapsible(false).default_width(560.0).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
        ui.label("Paste tickets, an email, a spreadsheet selection or a plain list of software - one app per line works.");
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(text).desired_rows(14).desired_width(f32::INFINITY).hint_text("Notepad++\n7-Zip 24.08\nINC0012345 - please install QGIS for the GIS lab"));
        });
        ui.horizontal(|ui| {
            read = theme::primary(ui, !text.trim().is_empty(), "\u{E8C3}  Read requests").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if read {
        let text = app.batch.paste.take().unwrap_or_default();
        match Batch::save_pasted(&text) {
            Ok(path) => {
                open_file(app, &path);
                app.view = View::Batch;
            }
            Err(e) => {
                app.batch.paste = Some(text);
                app.error(format!("{e:#}"));
            }
        }
    } else if !open || cancel {
        app.batch.paste = None;
    }
}

pub fn open_saved(app: &mut StudioApp, dir: &Path) {
    match Batch::load(dir) {
        Ok(mut b) => {
            // A run interrupted by closing the Studio is not running any more.
            for it in &mut b.items {
                if matches!(it.status, ItemStatus::Running | ItemStatus::Queued) {
                    it.status = ItemStatus::Pending;
                }
            }
            app.batch.batch = Some(b);
            app.batch.selected = None;
        }
        Err(e) => app.error(format!("Could not open the batch: {e:#}")),
    }
}

/// Recipe id the item maps to, if that recipe exists in the workspace.
fn existing<'a>(app: &'a StudioApp, it: &BatchItem) -> Option<&'a ystudio_core::workspace::RecipeEntry> {
    let ids = if it.recipes.is_empty() { vec![it.recipe_id()] } else { it.recipes.clone() };
    ids.iter().find_map(|id| app.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(id)))
}

/// Icon state for an item: (text, colour).
fn icon_state(app: &StudioApp, it: &BatchItem) -> (String, egui::Color32) {
    let has = |f: &str| app.ws_cache.icons.iter().any(|i| i.eq_ignore_ascii_case(f));
    if let Some(r) = existing(app, it)
        && !r.meta.icon_file.is_empty()
    {
        return if has(&r.meta.icon_file) { ("\u{E73E}".into(), theme::green()) } else { (format!("\u{E711}  {}", r.meta.icon_file), theme::red()) };
    }
    let guess = icons::icon_file_name(&it.recipe_id());
    if has(&guess) { (format!("ready: {guess}"), theme::blue()) } else { ("—".into(), theme::grey()) }
}

fn task_for(app: &StudioApp, b: &Batch, idx: usize) -> Option<AgentTask> {
    let it = b.items.get(idx)?;
    let mut notes = it.agent_notes(b.record(&it.source));
    if !it.vendor_url.is_empty() && agent::vendor_start_url(&it.vendor_url, &[]).is_empty() {
        notes += &format!("\nRequest-supplied fallback media: {}. Research vendor downloads first.\n", it.vendor_url);
    }
    let guess = icons::icon_file_name(&it.recipe_id());
    if app.ws_cache.icons.iter().any(|i| i.eq_ignore_ascii_case(&guess)) {
        notes += &format!("\nAn icon is already saved as Icons\\{guess}; set iconFile: {guess}.\n");
    }
    match it.request {
        RequestKind::NotSoftware => None,
        RequestKind::Update | RequestKind::Existing if !it.existing_recipe.is_empty() => {
            Some(AgentTask::Update { id: it.existing_recipe.clone(), notes })
        }
        _ => Some(AgentTask::Create {
            app: it.app.clone(),
            id_hint: if it.suggested_id.is_empty() { ystudio_core::recipe::suggest_id(&it.app) } else { it.suggested_id.clone() },
            vendor_url: agent::vendor_start_url(&it.vendor_url, &it.urls),
            notes,
        }),
    }
}

fn any_agent_running(app: &StudioApp) -> bool {
    crate::workflow::busy(app)
}

pub fn start_authoring(app: &mut StudioApp, indices: Vec<usize>, options: crate::workflow::Options) {
    let Some(b) = &mut app.batch.batch else {
        app.error("Open a batch before starting authoring.");
        return;
    };
    app.batch.queue = indices.into();
    app.batch.verification = Some(options);
    for &idx in &app.batch.queue {
        b.items[idx].status = ItemStatus::Queued;
        b.items[idx].checks.clear();
    }
    app.batch.show_log = true;
    save(app);
    start_next(app);
}

/// Starts the next queued item, if any.
pub fn start_next(app: &mut StudioApp) {
    if any_agent_running(app) {
        return;
    }
    let Some(ws) = app.ws.clone() else {
        app.batch.queue.clear();
        return;
    };
    while let Some(idx) = app.batch.queue.pop_front() {
        let Some(b) = app.batch.batch.as_ref() else { break };
        let Some(task) = task_for(app, b, idx) else { continue };
        let batch_id = b.id.clone();
        match agent::build_command(&ws, app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &task) {
            Ok(spec) => {
                let before = Snapshot::take(&ws);
                let verification = app.batch.verification.clone().and_then(|options| crate::workflow::task_id(&task)
                    .map(|recipe_id| crate::workflow::Verification { recipe_id, options }));
                let job = app.start_job(spec, JobKind::Agent { before, batch_item: Some((batch_id, idx)), verification });
                let it = &mut app.batch.batch.as_mut().unwrap().items[idx];
                if job.is_some() {
                    it.status = ItemStatus::Running;
                    it.status_note = format!("{} started", app.settings.agent_backend.label());
                    app.batch.run_job = job;
                } else {
                    it.status = ItemStatus::Failed;
                    it.status_note = "could not start the agent".into();
                }
                save(app);
                if job.is_some() {
                    return;
                }
            }
            Err(e) => {
                app.batch.queue.clear();
                app.error(format!("{e:#}"));
                break;
            }
        }
    }
    reset_queued(app);
    app.batch.verification = None;
    save(app);
}

fn reset_queued(app: &mut StudioApp) {
    if let Some(b) = &mut app.batch.batch {
        for it in &mut b.items {
            if it.status == ItemStatus::Queued {
                it.status = ItemStatus::Pending;
            }
        }
    }
}

pub fn stop_queue(app: &mut StudioApp) {
    app.batch.queue.clear();
    app.batch.verification = None;
    reset_queued(app);
    save(app);
}

fn with_batch(app: &mut StudioApp, batch_id: &str, f: impl FnOnce(&mut Batch)) {
    if let Some(b) = app.batch.batch.as_mut().filter(|b| b.id == batch_id) {
        f(b);
        save(app);
        return;
    }
    // The user switched batches meanwhile; update it on disk.
    let dir = Batch::root().join(batch_id);
    match Batch::load(&dir) {
        Ok(mut b) => {
            f(&mut b);
            if let Err(e) = b.save() {
                app.error(format!("Could not save batch {batch_id}: {e:#}"));
            }
        }
        Err(e) => app.error(format!("Could not load batch {batch_id}: {e:#}")),
    }
}

/// Called by the app when an agent run for a batch item ends.
pub fn agent_finished(app: &mut StudioApp, batch_id: &str, idx: usize, ok: bool, verifying: bool) {
    let mut ids = app.agent.reported.clone();
    for c in &app.agent.changes {
        let p = c.path();
        let is_recipe = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
            && app.recipes.iter().any(|r| ystudio_core::same_path(&r.path, p));
        if is_recipe
            && let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned())
            && !ids.iter().any(|i| i.eq_ignore_ascii_case(&stem))
        {
            ids.push(stem);
        }
    }
    let changed = app.agent.changes.len();
    with_batch(app, batch_id, |b| {
        if let Some(it) = b.items.get_mut(idx) {
            it.status = if !ok { ItemStatus::Failed } else if verifying { ItemStatus::Running } else { ItemStatus::Done };
            it.recipes = ids;
            it.status_note = format!("{}; {changed} file(s) changed", if ok { "agent finished" } else { "agent exited with an error" });
        }
    });
    if app.batch.run_job.is_some_and(|j| app.job(j).is_some_and(|j| !j.proc.is_running())) {
        app.batch.run_job = None;
    }
    if !verifying { start_next(app); }
}

pub fn check_finished(app: &mut StudioApp, batch_id: &str, idx: usize, check: BatchCheck) {
    with_batch(app, batch_id, |b| {
        if let Some(it) = b.items.get_mut(idx) {
            it.checks.retain(|c| !c.recipe.eq_ignore_ascii_case(&check.recipe));
            it.status_note = check.note.clone();
            it.checks.push(check);
            it.status = if it.checks.iter().all(|c| c.passed) { ItemStatus::Done } else { ItemStatus::Failed };
        }
    });
}

pub fn checks_started(app: &mut StudioApp, targets: &[crate::workflow::Target]) {
    for target in targets {
        for (batch_id, idx) in &target.items {
            with_batch(app, batch_id, |b| {
                if let Some(it) = b.items.get_mut(*idx) {
                    it.checks.retain(|c| !c.recipe.eq_ignore_ascii_case(&target.id));
                    it.checks.push(BatchCheck {
                        recipe: target.id.clone(), passed: false, note: "Testing not completed".into(),
                        timestamp: intake::timestamp(), summary_dir: None, live: false,
                    });
                    it.status = ItemStatus::Running;
                    it.status_note = "queued for harness checks".into();
                }
            });
        }
    }
}

/// Shows the running check, and where it runs, on each item it covers.
pub fn checks_progress(app: &mut StudioApp, items: &[(String, usize)], note: &str) {
    for (batch_id, idx) in items {
        with_batch(app, batch_id, |b| {
            if let Some(it) = b.items.get_mut(*idx) {
                it.status_note = note.to_string();
            }
        });
    }
}

fn test_targets(batch: &Batch) -> Vec<crate::workflow::Target> {
    let mut targets: Vec<crate::workflow::Target> = Vec::new();
    for (idx, item) in batch.items.iter().enumerate().filter(|(_, i)| i.include && i.request != RequestKind::NotSoftware) {
        let ids = if item.recipes.is_empty() { vec![item.recipe_id()] } else { item.recipes.clone() };
        for id in ids {
            if let Some(target) = targets.iter_mut().find(|t| t.id.eq_ignore_ascii_case(&id)) {
                if !target.items.contains(&(batch.id.clone(), idx)) { target.items.push((batch.id.clone(), idx)); }
            } else {
                targets.push(crate::workflow::Target { id, items: vec![(batch.id.clone(), idx)] });
            }
        }
    }
    targets
}

/// Included recipes that exist but lack a check confirming them for every
/// item they serve; with live checks on, only a passed live test confirms.
/// Items without a recipe yet are left to *Run agent*.
fn repair_targets(app: &StudioApp, batch: &Batch) -> Vec<crate::workflow::Target> {
    let live = app.workflow.options.live;
    let confirmed = |t: &crate::workflow::Target| t.items.iter().all(|(_, idx)| {
        batch.items[*idx].checks.iter().any(|c| c.recipe.eq_ignore_ascii_case(&t.id) && c.confirms(live))
    });
    test_targets(batch)
        .into_iter()
        .filter(|t| app.recipes.iter().any(|r| r.id.eq_ignore_ascii_case(&t.id)) && !confirmed(t))
        .collect()
}

/// Called by the app when an extraction agent ends.
pub fn extract_finished(app: &mut StudioApp, batch_id: &str, ok: bool, backend: &str) {
    let recipes = app.recipes.clone();
    let mut result = Err(anyhow::anyhow!("batch not found"));
    with_batch(app, batch_id, |b| result = b.load_agent_items(&b.dir(), &recipes, backend));
    match result {
        Ok(n) => {
            app.batch.selected = None;
            app.notify(format!("{backend} extracted {n} item(s)."));
        }
        Err(e) => app.error(format!("Extraction {}: {e:#}", if ok { "produced no usable output" } else { "failed" })),
    }
}

/// Called by the Icon finder after saving an icon for a batch item.
pub fn icon_saved(app: &mut StudioApp, batch_id: &str, idx: usize, file: &str) {
    with_batch(app, batch_id, |b| {
        if let Some(it) = b.items.get_mut(idx) {
            it.status_note = format!("icon saved as Icons\\{file}");
        }
    });
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        theme::page_title(ui, "Requests");
        ui.label(RichText::new("new recipes, imported request lists and recipe updates - written, tested and repaired together").weak());
    });
    // Dropped request lists open as a new batch.
    let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
    if let Some(p) = dropped.into_iter().find(|p| {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| intake::EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
    }) {
        open_file(app, &p);
    }

    toolbar(app, ui);
    ui.separator();
    if app.batch.batch.is_none() {
        start_page(app, ui);
        return;
    }
    if app.batch.selected.is_some() {
        egui::Panel::right("batch-detail").resizable(true).default_size(380.0).show(ui, |ui| detail(app, ui));
    }
    let show_log = app.batch.show_log;
    let log_job = if app.workflow.busy() { app.workflow.last_job } else { app.batch.run_job.or(app.batch.extract_job) };
    // The console already shows this job; a second copy only takes space.
    let in_console = app.console_open && app.console_job.is_some() && app.console_job == log_job;
    if show_log && !in_console && let Some(id) = log_job {
        egui::Panel::bottom("batch-log").resizable(true).default_size(220.0).show(ui, |ui| {
            if let Some(job) = app.job_mut(id) {
                job_log(ui, &mut job.proc, "batch");
            }
        });
    }
    items_table(app, ui);
}

fn toolbar(app: &mut StudioApp, ui: &mut Ui) {
    let busy = app.batch.busy(app);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!busy, egui::Button::new("\u{E838}  Open request list…")).clicked()
            && let Some(p) = rfd::FileDialog::new().add_filter("Request lists", intake::EXTENSIONS).add_filter("All files", &["*"]).pick_file()
        {
            open_file(app, &p);
        }
        if ui.add_enabled(!busy, egui::Button::new("\u{E77F}  Paste text…")).clicked() {
            app.batch.paste = Some(String::new());
        }
        if ui.add_enabled(!busy && app.writable(), egui::Button::new("\u{E710}  New recipe…")).clicked() {
            app.library.new_open = true;
        }
        let saved = app.batch.saved.get_or_insert_with(Batch::list).clone();
        let current = app.batch.batch.as_ref().map(|b| b.id.clone());
        let current_label = app.batch.batch.as_ref().map(super::home::request_label);
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("batch-saved")
                .selected_text(current_label.unwrap_or_else(|| format!("Earlier requests ({})", saved.len())))
                .height(400.0)
                .show_ui(ui, |ui| {
                    for (name, dir) in &saved {
                        if ui.selectable_label(current.as_deref() == Some(name.as_str()), name).clicked() {
                            open_saved(app, dir);
                        }
                    }
                    if saved.is_empty() {
                        ui.label(RichText::new("none yet").weak());
                    }
                });
        });
        if ui.small_button("\u{E72C}").on_hover_text("Rescan earlier requests").clicked() {
            app.batch.saved = None;
        }
        if let Some(b) = &app.batch.batch {
            let dir = b.dir();
            if ui.button("\u{E8C8}  Copy report").on_hover_text("Markdown table to paste back into the tickets").clicked() {
                let r = b.report();
                ui.ctx().copy_text(r);
                app.notify("Report copied to the clipboard.");
            }
            if ui.button("\u{E8B7}  Request folder").clicked() {
                super::open_in_explorer(&dir);
            }
        }
    });
}

fn start_page(app: &mut StudioApp, ui: &mut Ui) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("\u{E8C3}  Drop a request list here, open one, or paste text.").heading());
        ui.add_space(6.0);
        ui.label(format!("Understands {}.", intake::EXTENSIONS.join(", ")));
        ui.label(RichText::new("ServiceNow / Jira exports, spreadsheets, CSVs, emails or plain notes all work.").weak());
        ui.add_space(14.0);
    });
    theme::card(ui, |ui| {
        ui.label(RichText::new("How it works").semibold());
        ui.label("1. The Studio reads the file and strips the noise (HTML, signatures, approval boilerplate).");
        ui.label("2. A quick scan lists the software it can see and matches it against existing recipes.");
        ui.label("3. For messy tickets, “Extract with agent” has Copilot / Claude read every request and return a clean list.");
        ui.label("4. Review the list, then queue agent runs that write (or update) one recipe per item and find icons.");
        ui.label("5. Studio tests and repairs each recipe. Use Test selected to recheck them all, then copy the report.");
    });
    if app.ws.is_none() {
        ui.label(RichText::new("Tip: open a Yardstick folder first so items can be matched to existing recipes.").small().weak());
    }
}

fn items_table(app: &mut StudioApp, ui: &mut Ui) {
    let busy = app.batch.busy(app);
    let can_agent = app.writable() && app.ws_cache.has_skill;
    let can_test = app.writable();
    let running_extract = app.batch.extract_job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running());
    let running_items = !app.workflow.running() && (app.batch.run_job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running()) || !app.batch.queue.is_empty());
    let queued = app.batch.queue.len();
    let backend = app.settings.agent_backend.label();
    let b = app.batch.batch.as_ref().unwrap();
    let total = b.items.len();
    let software = b.items.iter().filter(|i| i.request != RequestKind::NotSoftware).count();
    let included = b.items.iter().filter(|i| i.include && i.request != RequestKind::NotSoftware).count();
    let pending = b.items.iter().filter(|i| i.include && i.request != RequestKind::NotSoftware && i.status != ItemStatus::Done).count();
    let repairable = repair_targets(app, b).len();
    let file = b.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let summary = format!("{} request(s) · {software} software item(s) · extracted by {}", b.records.len(), b.extracted_by);
    // Requests made in the Studio are already a clean list.
    let extractable = b.extracted_by != "Studio";
    let format = b.format.clone();

    // Header.
    let mut extract = false;
    let mut queue = false;
    let mut stop = false;
    let mut cancel = false;
    let mut test = false;
    let mut repair = false;
    let st = &mut app.batch;
    theme::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(file).semibold());
            theme::badge(ui, &format, theme::blue());
            ui.label(summary);
        });
        ui.horizontal_wrapped(|ui| {
            if extractable {
                if running_extract {
                    ui.spinner();
                    ui.label("Agent is reading the requests…");
                    if ui.button("\u{E711}  Cancel").clicked() {
                        cancel = true;
                    }
                } else if ui
                    .add_enabled(!busy, egui::Button::new("\u{E99A}  Extract with agent"))
                    .on_hover_text(format!("{backend} reads every request and returns a structured list (replaces the current items)."))
                    .clicked()
                {
                    extract = true;
                }
                ui.separator();
            }
            if running_items {
                ui.spinner();
                ui.label(format!("Working · {queued} queued"));
                if ui.button("\u{E769}  Stop after this item").clicked() {
                    stop = true;
                }
                if ui.button("\u{E711}  Cancel now").clicked() {
                    stop = true;
                    cancel = true;
                }
            } else {
                let resp = theme::primary(ui, can_agent && pending > 0 && !busy, RichText::new(format!("\u{E768}  Run agent on {pending} selected")));
                if !can_agent {
                    resp.on_hover_text("Needs a writable development folder with the yardstick-recipes skill.");
                } else if resp.clicked() {
                    queue = true;
                }
            }
            if ui.add_enabled(can_test && included > 0 && !busy, egui::Button::new("Test selected")).clicked() {
                test = true;
            }
            let resp = ui.add_enabled(can_agent && repairable > 0 && !busy, egui::Button::new(format!("\u{E90F}  Repair {repairable} with agent")));
            repair = resp.clicked();
            if !can_agent {
                resp.on_disabled_hover_text("Needs a writable development folder with the yardstick-recipes skill.");
            } else if repairable == 0 {
                resp.on_disabled_hover_text("Every included item's recipe has a passing check. Items without a recipe yet need Run agent.");
            } else {
                resp.on_hover_text(
                    "Tests the included recipes that no check has confirmed working yet and hands each failure to the agent to fix, \
                     re-testing after every fix. Recipes that pass are not changed. Items without a recipe yet need Run agent.",
                );
            }
            ui.separator();
            ui.checkbox(&mut st.hide_other, "Hide non-software");
            ui.checkbox(&mut st.show_log, "Show log");
        });
    });
    ui.add_enabled_ui(!busy, |ui| {
        ui.horizontal(|ui| {
            ui.label("Agent");
            super::settings::agent_picker(app, ui, "requests");
        });
        crate::workflow::configure(&mut app.workflow.options, ui, true);
    });
    crate::workflow::progress(app, ui);
    if extract {
        start_extract(app);
    }
    if queue {
        let b = app.batch.batch.as_ref().unwrap();
        let indices = b
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.include && i.request != RequestKind::NotSoftware && i.status != ItemStatus::Done)
            .map(|(n, _)| n)
            .collect();
        crate::workflow::request(app, crate::workflow::Request::BatchAuthor(indices));
    }
    if repair {
        let targets = repair_targets(app, app.batch.batch.as_ref().unwrap());
        app.batch.show_log = true;
        crate::workflow::request(app, crate::workflow::Request::BatchRepair(targets));
    }
    if test {
        let targets = test_targets(app.batch.batch.as_ref().unwrap());
        app.batch.show_log = true;
        crate::workflow::request(app, crate::workflow::Request::BatchTests(targets));
    }
    if stop {
        crate::workflow::stop(app, false);
    }
    if cancel {
        crate::workflow::stop(app, true);
        for id in [app.batch.run_job, app.batch.extract_job].into_iter().flatten() {
            if let Some(j) = app.job_mut(id) {
                j.proc.cancel();
            }
        }
    }

    // Rows.
    let hide = app.batch.hide_other;
    let rows: Vec<(usize, BatchItem, (String, egui::Color32), bool)> = app
        .batch
        .batch
        .as_ref()
        .unwrap()
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| !(hide && i.request == RequestKind::NotSoftware))
        .map(|(n, i)| (n, i.clone(), icon_state(app, i), existing(app, i).is_some()))
        .collect();
    let mut toggles = Vec::new();
    let mut select = None;
    let mut icon_for = None;
    let mut open = None;
    egui::ScrollArea::both().id_salt("batch-items").auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("batch-grid").striped(true).num_columns(8).spacing([12.0, 6.0]).min_row_height(24.0).show(ui, |ui| {
            for h in ["", "Ticket", "Software", "Version", "Request", "Status", "Icon", "Recipe"] {
                ui.label(RichText::new(h).semibold());
            }
            ui.end_row();
            for (n, it, (icon_text, icon_color), exists) in &rows {
                let mut inc = it.include;
                if ui.add_enabled(it.request != RequestKind::NotSoftware && !busy, egui::Checkbox::without_text(&mut inc)).changed() {
                    toggles.push((*n, inc));
                }
                let sel = app.batch.selected == Some(*n);
                if ui.add(egui::Button::selectable(sel, RichText::new(&it.source).monospace())).clicked() {
                    select = Some(*n);
                }
                let name = if it.app.is_empty() { "(nothing to package)".to_string() } else { it.app.clone() };
                let name = if it.request == RequestKind::NotSoftware { RichText::new(name).weak() } else { RichText::new(name).semibold() };
                if ui.add(egui::Button::selectable(sel, name)).on_hover_text(&it.notes).clicked() {
                    select = Some(*n);
                }
                ui.label(&it.version);
                ui.label(RichText::new(it.request.label()).color(request_color(it.request)));
                ui.horizontal(|ui| {
                    item_status_badge(app, ui, it);
                    if !it.status_note.is_empty() {
                        // Long blocker notes stay readable on hover without widening the table.
                        let short: String = it.status_note.chars().take(90).collect();
                        let short = if short.len() < it.status_note.len() { format!("{short}…") } else { short };
                        ui.label(RichText::new(short).small().weak()).on_hover_text(&it.status_note);
                    }
                });
                ui.horizontal(|ui| {
                    if it.request != RequestKind::NotSoftware {
                        if ui.small_button("\u{E91B}").on_hover_text("Find an icon").clicked() {
                            icon_for = Some(*n);
                        }
                        ui.label(RichText::new(icon_text).small().color(*icon_color));
                    }
                });
                ui.horizontal(|ui| {
                    let id = if it.recipes.is_empty() { it.recipe_id() } else { it.recipes.join(", ") };
                    if it.request == RequestKind::NotSoftware {
                        ui.label("");
                    } else if *exists {
                        if ui.link(RichText::new(&id).monospace()).on_hover_text("Open in the Designer").clicked() {
                            open = Some(id.split(", ").next().unwrap_or_default().to_string());
                        }
                    } else {
                        ui.label(RichText::new(id).monospace().weak()).on_hover_text("Suggested id - no recipe yet");
                    }
                });
                ui.end_row();
            }
        });
        if rows.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new(if total == 0 { "No software found. Try “Extract with agent”." } else { "Everything is hidden by the filter." }).weak());
        }
    });
    if !toggles.is_empty() {
        let b = app.batch.batch.as_mut().unwrap();
        for (n, v) in toggles {
            b.items[n].include = v;
        }
        save(app);
    }
    if let Some(n) = select {
        app.batch.selected = Some(n);
    }
    if let Some(n) = icon_for {
        find_icon(app, n);
    }
    if let Some(id) = open
        && app.open_recipe_by_id(&id)
    {
        app.view = View::Designer;
    }
}

fn start_extract(app: &mut StudioApp) {
    let Some(b) = &app.batch.batch else { return };
    let dir = b.dir();
    let batch_id = b.id.clone();
    if let Err(e) = b.prepare_agent(&dir, &app.recipes) {
        app.error(format!("Could not prepare the batch: {e:#}"));
        return;
    }
    match agent::build_extract_command(app.settings.agent_backend, &app.settings.agent_model, &dir) {
        Ok(spec) => {
            app.batch.extract_job = app.start_job(spec, JobKind::BatchExtract { batch_id, backend: app.settings.agent_backend.label().to_string() });
            app.batch.show_log = true;
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

fn find_icon(app: &mut StudioApp, n: usize) {
    let Some(it) = app.batch.batch.as_ref().and_then(|b| b.items.get(n)).cloned() else { return };
    let batch_id = app.batch.batch.as_ref().unwrap().id.clone();
    let entry = existing(app, &it).cloned();
    match &entry {
        Some(e) => super::icons::prepare_for_recipe(app, &e.id),
        None => {
            let url = if it.vendor_url.is_empty() { it.urls.iter().find(|u| !u.contains("sharepoint") && !u.contains("onedrive")).cloned().unwrap_or_default() } else { it.vendor_url.clone() };
            app.icons.prepare(&it.app, &it.publisher, &url, &icons::icon_file_name(&it.recipe_id()));
        }
    }
    // The finder always searches by what the requester called it.
    if !it.app.is_empty() {
        app.icons.name = it.app.clone();
        app.icons.auto_search = true;
    }
    app.icons.batch_item = Some((batch_id, n));
    app.view = View::Icons;
}

fn detail(app: &mut StudioApp, ui: &mut Ui) {
    let Some(n) = app.batch.selected else { return };
    let busy = app.batch.busy(app);
    let can_agent = app.writable() && app.ws_cache.has_skill && !busy;
    let ids: Vec<String> = app.recipes.iter().map(|r| r.id.clone()).collect();
    let Some(b) = app.batch.batch.as_mut() else { return };
    let Some(it) = b.items.get(n).cloned() else {
        app.batch.selected = None;
        return;
    };
    let record = b.record(&it.source).cloned();
    let mut edit = it.clone();
    let mut run = false;
    let mut icon = false;
    let mut matches_for = None;
    let mut close = false;
    ui.add_enabled_ui(!busy, |ui| {
    egui::ScrollArea::vertical().id_salt("batch-detail-scroll").show(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_heading(ui, if edit.source.is_empty() { "Item" } else { &edit.source });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("\u{E711}").on_hover_text("Close").clicked() {
                    close = true;
                }
            });
            });
        });
        egui::Grid::new("batch-item-form").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            let w = ui.available_width().max(160.0);
            ui.label("Software");
            if ui.add(egui::TextEdit::singleline(&mut edit.app).desired_width(w)).lost_focus() {
                matches_for = Some(edit.app.clone());
            }
            ui.end_row();
            ui.label("Publisher");
            ui.add(egui::TextEdit::singleline(&mut edit.publisher).desired_width(w));
            ui.end_row();
            ui.label("Version");
            ui.add(egui::TextEdit::singleline(&mut edit.version).desired_width(w));
            ui.end_row();
            ui.label("Request");
            egui::ComboBox::from_id_salt("batch-req").selected_text(edit.request.label()).show_ui(ui, |ui| {
                for k in RequestKind::ALL {
                    ui.selectable_value(&mut edit.request, k, k.label());
                }
            });
            ui.end_row();
            ui.label("Existing recipe");
            egui::ComboBox::from_id_salt("batch-existing")
                .selected_text(if edit.existing_recipe.is_empty() { "none".to_string() } else { edit.existing_recipe.clone() })
                .height(400.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut edit.existing_recipe, String::new(), "none");
                    for id in &ids {
                        ui.selectable_value(&mut edit.existing_recipe, id.clone(), id);
                    }
                });
            ui.end_row();
            ui.label("New recipe id");
            ui.add(egui::TextEdit::singleline(&mut edit.suggested_id).hint_text(ystudio_core::recipe::suggest_id(&edit.app)).desired_width(w));
            ui.end_row();
            ui.label("Vendor page");
            ui.add(egui::TextEdit::singleline(&mut edit.vendor_url).desired_width(w));
            ui.end_row();
        });
        if !edit.urls.is_empty() {
            ui.label(RichText::new("Links in the request").semibold());
            for u in &edit.urls {
                ui.hyperlink_to(RichText::new(u).small(), u);
            }
        }
        ui.label("Notes for the agent");
        ui.add(egui::TextEdit::multiline(&mut edit.notes).desired_rows(3).desired_width(f32::INFINITY));
        if edit.confidence > 0.0 {
            ui.label(RichText::new(format!("Extractor confidence {:.0}%", edit.confidence * 100.0)).small().weak());
        }
        ui.horizontal_wrapped(|ui| {
            let can_run = can_agent && edit.request != RequestKind::NotSoftware;
            if ui.add_enabled(can_run, egui::Button::new("\u{E99A}  Run agent on this")).clicked() {
                run = true;
            }
            if ui.add_enabled(edit.request != RequestKind::NotSoftware, egui::Button::new("\u{E91B}  Find icon")).clicked() {
                icon = true;
            }
            if edit.status == ItemStatus::Skipped {
                if ui.button("Unskip").clicked() {
                    edit.status = ItemStatus::Pending;
                }
            } else if !matches!(edit.status, ItemStatus::Running | ItemStatus::Queued) && ui.button("Skip").clicked() {
                edit.status = ItemStatus::Skipped;
                edit.include = false;
            }
        });
        if let Some(r) = &record {
            ui.add_space(8.0);
            theme::section_heading(ui, "Original request");
            ui.label(RichText::new(&r.title).semibold());
            for (k, v) in r.fields.iter().take(12) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(k).small().weak());
                    ui.label(RichText::new(v).small());
                });
            }
            let mut body = r.body.clone();
            ui.add(egui::TextEdit::multiline(&mut body).desired_rows(12).desired_width(f32::INFINITY).interactive(false));
        }
    });
    if let Some(name) = matches_for
        && name != it.app
        && edit.existing_recipe.is_empty()
        && let Some(m) = intake::match_recipes(&name, &app.recipes, 1).into_iter().next()
        && m.score >= 0.9
    {
        edit.existing_recipe = m.id;
    }
    if edit != it {
        if edit.app != it.app || edit.version != it.version || edit.existing_recipe != it.existing_recipe
            || edit.suggested_id != it.suggested_id || edit.vendor_url != it.vendor_url
            || edit.notes != it.notes || edit.publisher != it.publisher
        {
            edit.recipes.clear();
            edit.checks.clear();
            edit.status = ItemStatus::Pending;
            edit.status_note.clear();
        }
        app.batch.batch.as_mut().unwrap().items[n] = edit;
        save(app);
    }
    if close {
        app.batch.selected = None;
    }
    if run {
        crate::workflow::request(app, crate::workflow::Request::BatchAuthor(vec![n]));
    }
    if icon {
        find_icon(app, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_testing_deduplicates_recipes_and_preserves_ticket_associations() {
        let mut batch = Batch::new(intake::Document { path: PathBuf::from("requests.txt"), format: "text".into(), records: Vec::new() }, &[]);
        batch.items = vec![
            BatchItem { app: "One".into(), include: true, recipes: vec!["shared".into(), "other".into()], ..Default::default() },
            BatchItem { app: "Two".into(), include: true, recipes: vec!["SHARED".into()], ..Default::default() },
            BatchItem { app: "Excluded".into(), include: false, recipes: vec!["excluded".into()], ..Default::default() },
            BatchItem { app: "Meeting".into(), include: true, request: RequestKind::NotSoftware, ..Default::default() },
        ];
        let targets = test_targets(&batch);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, "shared");
        assert_eq!(targets[0].items, vec![(batch.id.clone(), 0), (batch.id.clone(), 1)]);
        assert_eq!(targets[1].id, "other");
    }

    #[test]
    fn repair_picks_existing_recipes_without_a_confirming_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\nRuntimeRoot = '.'\nRecipesPath = 'Recipes'\nDevelopmentRecipes = 'Recipes\\Development'\n}\n").unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# never executed").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        for id in ["good", "broken", "safeonly"] {
            std::fs::write(root.join(format!(r"Recipes\Development\{id}.yaml")), format!("id: {id}\n")).unwrap();
        }
        std::fs::write(root.join(r"Recipes\shipped.yaml"), "id: shipped\n").unwrap();
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        let mut app = StudioApp::isolated_for_tests(ws);

        let check = |recipe: &str, passed, live, note: &str| BatchCheck {
            recipe: recipe.into(), passed, note: note.into(), timestamp: intake::timestamp(), summary_dir: None, live,
        };
        let item = |id: &str, checks| BatchItem { app: id.into(), include: true, recipes: vec![id.into()], checks, ..Default::default() };
        let mut batch = Batch::new(intake::Document { path: PathBuf::from("requests.txt"), format: "text".into(), records: Vec::new() }, &[]);
        batch.items = vec![
            item("good", vec![check("good", true, true, "Validated")]),
            item("broken", vec![check("broken", false, true, "Checks failed")]),
            item("safeonly", vec![check("safeonly", true, false, "Safe checks passed; live test still required")]),
            item("shipped", Vec::new()),
            item("notyet", Vec::new()),
            // Shares a confirmed recipe, but this ticket's own check never ran.
            BatchItem { app: "Good again".into(), include: true, recipes: vec!["good".into()], ..Default::default() },
        ];
        let ids = |app: &StudioApp, batch: &Batch| repair_targets(app, batch).into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(&app, &batch), ["good", "broken", "safeonly", "shipped"]);
        batch.items.pop();
        assert_eq!(ids(&app, &batch), ["broken", "safeonly", "shipped"]);
        app.workflow.options.live = false;
        assert_eq!(ids(&app, &batch), ["broken", "shipped"]);
    }
}
