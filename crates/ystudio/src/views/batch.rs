//! Batch mode: read a request list (ServiceNow export, spreadsheet, email…),
//! pull out the software to package with a quick scan or an agent, then work
//! through the items - agent runs, icons, recipe matches - and report back.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::{self, AgentTask, Snapshot};
use ystudio_core::icons;
use ystudio_core::intake::{self, Batch, BatchCheck, BatchItem, ItemAction, ItemStatus, PluginRecipeOrigin, RequestKind};
use ystudio_core::plugin::{self, RecipeWorkRecord, RecipeWorkState, WorkItem};
use ystudio_core::recipe::{self, RecipeDoc};
use ystudio_core::workspace::RecipeLocation;

use crate::app::{JobKind, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AddAction { Existing, #[default] New, Import, Paste }

#[derive(Clone)]
enum RemoveScope {
    Selected(Vec<usize>),
    Clear,
}

#[derive(Default)]
pub struct NewRecipeDraft {
    pub display: String,
    pub publisher: String,
    pub id: String,
    pub id_touched: bool,
    pub vendor: String,
    pub notes: String,
    template: usize,
    templates: Option<Vec<PathBuf>>,
}

#[derive(Default)]
pub struct BatchState {
    pub batch: Option<Batch>,
    pub saved: Option<Vec<(String, PathBuf)>>,
    pub(crate) selected: Option<usize>,
    pub extract_job: Option<u64>,
    pub run_job: Option<u64>,
    pub queue: VecDeque<usize>,
    hide_other: bool,
    pub(crate) show_log: bool,
    verification: Option<crate::workflow::Options>,
    /// Text being pasted as a new request list; `Some` while the window is open.
    pub paste: Option<String>,
    pub(crate) selected_items: BTreeSet<usize>,
    composing: bool,
    pub new_recipe: NewRecipeDraft,
    last_add: AddAction,
    dirty: bool,
    pending_remove: Option<RemoveScope>,
    pending_batch: Option<Batch>,
    save_as: Option<String>,
}

impl BatchState {
    pub fn busy(&self, app_jobs: &StudioApp) -> bool {
        crate::workflow::busy(app_jobs) || !self.queue.is_empty()
    }
}

pub fn show_new_recipe(app: &mut StudioApp) {
    app.batch.composing = true;
    app.batch.last_add = AddAction::New;
    app.view = View::Batch;
}

pub fn contains_recipe(app: &StudioApp, id: &str) -> bool {
    app.batch.batch.as_ref().is_some_and(|b| b.items.iter().any(|it| {
        it.existing_recipe.eq_ignore_ascii_case(id) || it.suggested_id.eq_ignore_ascii_case(id)
            || it.recipes.iter().any(|r| r.eq_ignore_ascii_case(id))
    }))
}

pub fn add_existing_recipe(app: &mut StudioApp, id: &str) {
    if contains_recipe(app, id) {
        app.notify(format!("{id} is already in the Recipe Builder list."));
        return;
    }
    let name = app.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(id))
        .map(|r| r.meta.display_name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_string());
    append_items(app, vec![BatchItem { app: name, existing_recipe: id.to_string(), request: RequestKind::Existing, ..Default::default() }]);
    app.notify(format!("Added {id} to Recipe Builder."));
}

fn append_items(app: &mut StudioApp, items: Vec<BatchItem>) -> Vec<usize> {
    if app.batch.batch.is_none() {
        app.batch.batch = Some(Batch::from_items("recipe builder", Vec::new()));
    }
    let batch = app.batch.batch.as_mut().unwrap();
    let mut added = Vec::new();
    let mut changed = false;
    for mut item in items {
        let key = item_key(&item);
        if let Some(existing) = batch.items.iter_mut().find(|it| !key.is_empty() && item_key(it).eq_ignore_ascii_case(&key)) {
            if !item.notes.trim().is_empty() && !existing.notes.contains(item.notes.trim()) {
                if !existing.notes.is_empty() { existing.notes.push_str("\n\n"); }
                existing.notes.push_str(item.notes.trim());
                changed = true;
            }
            for origin in item.plugin_origins {
                if !existing.plugin_origins.contains(&origin) {
                    existing.plugin_origins.push(origin);
                    changed = true;
                }
            }
            continue;
        }
        let n = batch.items.len();
        if item.source.is_empty() {
            item.source = format!("#{}", n + 1);
        }
        item.include = true;
        batch.records.push(intake::Record { reference: item.source.clone(), title: item.app.clone(), body: item.notes.clone(), fields: Vec::new() });
        batch.items.push(item);
        added.push(n);
    }
    if let Some(&n) = added.last() { app.batch.selected = Some(n); }
    if changed || !added.is_empty() {
        app.batch.dirty = true;
        save(app);
    }
    added
}

/// Adds one product split out of a plugin work item without navigating away,
/// so several rows can be routed in one pass.
pub fn add_plugin_product(app: &mut StudioApp, item: BatchItem) -> String {
    let title = item.app.clone();
    if append_items(app, vec![item]).is_empty() {
        app.notify(format!("{title} is already in Recipe Builder; it was removed from the plugin's active work."));
    } else {
        app.notify(format!("Sent {title} to Recipe Builder."));
    }
    app.batch.batch.as_ref().map(|batch| batch.id.clone()).unwrap_or_default()
}

/// Adds or links the Recipe Builder items that author recipes for products
/// which remain in a plugin work item. Returns the persistent batch id.
pub fn add_plugin_recipe_work(app: &mut StudioApp, items: Vec<BatchItem>) -> String {
    append_items(app, items);
    app.batch.batch.as_ref().map(|batch| batch.id.clone()).unwrap_or_default()
}

fn open_batch_id(app: &mut StudioApp, batch_id: &str) -> bool {
    if app.batch.batch.as_ref().is_some_and(|batch| batch.id == batch_id) {
        return true;
    }
    match Batch::load(&Batch::root().join(batch_id)) {
        Ok(batch) => {
            set_batch(app, batch);
            true
        }
        Err(e) => {
            app.error(format!("Could not open the linked Recipe Builder list: {e:#}"));
            false
        }
    }
}

pub fn open_plugin_recipe_work(app: &mut StudioApp, batch_id: &str, work_item: &Path, product_id: Option<&str>) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active Recipe Builder run first.");
        return false;
    }
    if !batch_id.is_empty() && !open_batch_id(app, batch_id) {
        return false;
    }
    let selected = app.batch.batch.as_ref().and_then(|batch| {
        batch.items.iter().position(|item| item.plugin_origins.iter().any(|origin| {
            origin.work_item == work_item && product_id.is_none_or(|id| origin.product_id == id)
        }))
    });
    let Some(selected) = selected else {
        app.error("The linked Recipe Builder item is missing. Continue the safe stages to recreate it.");
        return false;
    };
    app.batch.selected = Some(selected);
    app.view = View::Batch;
    true
}

/// Returns from a plugin work item to the exact Recipe Builder request that
/// created it. Older items whose saved index moved are found by their link.
pub fn open_plugin_handoff_source(app: &mut StudioApp, key: &str, work_item: &Path) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active Recipe Builder run first.");
        return false;
    }
    let Some((batch_id, raw_index)) = key.rsplit_once(':') else {
        app.error("This older plugin work item does not contain a Recipe Builder return link.");
        return false;
    };
    let hinted = raw_index.parse::<usize>().ok();
    if !open_batch_id(app, batch_id) {
        return false;
    }
    let selected = app.batch.batch.as_ref().and_then(|batch| {
        hinted
            .filter(|index| batch.items.get(*index).is_some_and(|item| item.handler_item.as_deref() == Some(work_item)))
            .or_else(|| batch.items.iter().position(|item| item.handler_item.as_deref() == Some(work_item)))
    });
    let Some(selected) = selected else {
        app.error("The originating Recipe Builder request is no longer in its working list.");
        return false;
    };
    app.batch.selected = Some(selected);
    app.view = View::Batch;
    true
}

/// Opens the exact Recipe Builder item that a plugin product was routed into.
pub fn open_routed_plugin_product(app: &mut StudioApp, batch_id: &str, work_item: &Path, product_id: &str) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active Recipe Builder run first.");
        return false;
    }
    if !batch_id.is_empty() && !open_batch_id(app, batch_id) {
        return false;
    }
    let selected = app.batch.batch.as_ref().and_then(|batch| {
        batch.items.iter().position(|item| {
            item.handler_item.as_deref() == Some(work_item) && item.suggested_id.eq_ignore_ascii_case(product_id)
        })
    });
    let Some(selected) = selected else {
        app.error("The routed product is not in the open Recipe Builder list.");
        return false;
    };
    app.batch.selected = Some(selected);
    app.view = View::Batch;
    true
}

fn plugin_recipe_item(app: &StudioApp, batch_id: &str, index: usize) -> Option<BatchItem> {
    if let Some(batch) = app.batch.batch.as_ref().filter(|batch| batch.id == batch_id) {
        return batch.items.get(index).cloned();
    }
    Batch::load(&Batch::root().join(batch_id)).ok()?.items.get(index).cloned()
}

fn copy_recipe_to_plugin(app: &StudioApp, origin: &PluginRecipeOrigin, item: &BatchItem) -> Result<String, String> {
    if !plugin::safe_relative(&origin.recipe_folder) || !plugin::safe_relative(&origin.icon_folder) {
        return Err("The plugin's recipe destination is not a safe relative path.".into());
    }
    if !recipe::is_valid_id(&origin.product_id) {
        return Err(format!("'{}' is not a valid recipe id.", origin.product_id));
    }
    let installed = app.plugins.get(&origin.plugin).ok_or_else(|| format!("The {} plugin is not installed.", origin.plugin))?;
    let values = app.settings.plugins.get(&origin.plugin).map(|prefs| prefs.values.clone()).unwrap_or_default();
    let workspace = app.ws.as_ref().filter(|workspace| workspace.kind.is_writable()).ok_or_else(|| "Open a writable Yardstick development workspace first.".to_string())?;
    let settings = plugin::resolve_settings(&installed.manifest, &values, Some(&workspace.root)).map_err(|problems| problems.join(" "))?;
    let root = plugin::work_root(&settings).ok_or_else(|| format!("Set up {} in Settings first.", installed.manifest.name))?;
    if !plugin::is_within(&origin.work_item, &root) {
        return Err("The linked plugin work item is outside its configured work folder.".into());
    }
    if !item.recipes.iter().any(|id| id.eq_ignore_ascii_case(&origin.product_id)) {
        return Err(format!("Recipe Builder did not report the expected '{}' recipe.", origin.product_id));
    }
    let recipe = app
        .recipes
        .iter()
        .find(|recipe| recipe.id.eq_ignore_ascii_case(&origin.product_id) && recipe.location == RecipeLocation::Development)
        .ok_or_else(|| format!("The '{}' development recipe could not be found.", origin.product_id))?;
    let recipe_dir = origin.work_item.join(&origin.recipe_folder);
    std::fs::create_dir_all(&recipe_dir).map_err(|e| format!("Could not create {}: {e}", recipe_dir.display()))?;
    let destination = recipe_dir.join(format!("{}.yaml", origin.product_id));
    let temporary = destination.with_extension("yaml.tmp");
    std::fs::copy(&recipe.path, &temporary).map_err(|e| format!("Could not copy {}: {e}", recipe.path.display()))?;
    std::fs::rename(&temporary, &destination).map_err(|e| format!("Could not save {}: {e}", destination.display()))?;

    if !recipe.meta.icon_file.is_empty() && plugin::safe_relative(&recipe.meta.icon_file) {
        let source = workspace.layout.icons.join(&recipe.meta.icon_file);
        if source.is_file() {
            let destination = origin.work_item.join(&origin.icon_folder).join(&recipe.meta.icon_file);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            std::fs::copy(&source, &destination).map_err(|e| format!("Could not copy {}: {e}", source.display()))?;
        }
    }
    Ok(format!("{} passed Recipe Builder's safe checks and was synced for One-Off packaging.", origin.product_id))
}

fn sync_plugin_recipe_work(app: &mut StudioApp, batch_id: &str, index: usize) {
    let Some(item) = plugin_recipe_item(app, batch_id, index) else { return };
    let origins = item.plugin_origins.clone();
    let mut changed = Vec::new();
    for origin in origins {
        let (state, recipe_id, note) = match item.status {
            ItemStatus::Queued | ItemStatus::Running => (RecipeWorkState::Running, String::new(), item.status_note.clone()),
            ItemStatus::Failed => (RecipeWorkState::Failed, String::new(), item.status_note.clone()),
            ItemStatus::Done if item.checks.iter().any(|check| check.recipe.eq_ignore_ascii_case(&origin.product_id) && check.passed) => {
                match copy_recipe_to_plugin(app, &origin, &item) {
                    Ok(note) => (RecipeWorkState::Ready, origin.product_id.clone(), note),
                    Err(note) => (RecipeWorkState::Failed, String::new(), note),
                }
            }
            ItemStatus::Done => (RecipeWorkState::Pending, String::new(), "Recipe written; run its safe checks in Recipe Builder.".into()),
            ItemStatus::Delegated => (RecipeWorkState::Failed, String::new(), "Linked recipe work cannot be delegated to another plugin.".into()),
            ItemStatus::Skipped => (RecipeWorkState::Failed, String::new(), "Recipe Builder work was skipped.".into()),
            ItemStatus::Pending => (RecipeWorkState::Pending, String::new(), "Ready to complete in Recipe Builder.".into()),
        };
        match WorkItem::open(&origin.work_item) {
            Ok(mut work) => {
                work.meta.recipe_work.insert(origin.product_id.clone(), RecipeWorkRecord {
                    state,
                    batch_id: batch_id.to_string(),
                    recipe_id,
                    note,
                    updated: intake::timestamp(),
                });
                match work.save() {
                    Ok(()) => changed.push((origin.plugin.clone(), origin.work_item.clone())),
                    Err(e) => app.error(format!("Could not update linked plugin recipe work: {e:#}")),
                }
            }
            Err(e) => app.error(format!("Could not open linked plugin work item: {e:#}")),
        }
    }
    for (plugin, work_item) in changed {
        crate::views::plugin_page::recipe_work_changed(app, &plugin, &work_item);
    }
}

/// Removes a not-yet-started product when a routing decision is reversed.
pub fn remove_plugin_product(app: &mut StudioApp, work_item: &Path, suggested_id: &str) -> Result<(), String> {
    let Some(batch) = app.batch.batch.as_mut() else { return Ok(()) };
    let Some(index) = batch.items.iter().position(|item| {
        item.handler_item.as_deref() == Some(work_item) && item.suggested_id == suggested_id
    }) else {
        return Ok(());
    };
    let item = &batch.items[index];
    if matches!(item.status, ItemStatus::Queued | ItemStatus::Running | ItemStatus::Done) || !item.recipes.is_empty() || !item.checks.is_empty() {
        return Err(format!("{} has already started in Recipe Builder and cannot be returned to the plugin.", item.app));
    }
    batch.items.remove(index);
    let retained_sources: BTreeSet<String> = batch.items.iter().map(|item| item.source.clone()).collect();
    batch.records.retain(|record| retained_sources.contains(&record.reference));
    app.batch.selected = None;
    app.batch.dirty = true;
    batch.save().map_err(|e| format!("Could not save Recipe Builder after returning the product: {e:#}"))
}

fn item_key(item: &BatchItem) -> String {
    if !item.existing_recipe.trim().is_empty() { item.existing_recipe.trim().to_string() }
    else if !item.suggested_id.trim().is_empty() { item.suggested_id.trim().to_string() }
    else { item.recipe_id() }
}

fn status_badge(ui: &mut Ui, s: ItemStatus) {
    let (t, c) = match s {
        ItemStatus::Pending => ("pending", theme::grey()),
        ItemStatus::Queued => ("queued", theme::blue()),
        ItemStatus::Running => ("running", theme::amber()),
        ItemStatus::Delegated => ("in plugin", theme::blue()),
        ItemStatus::Done => ("done", theme::green()),
        ItemStatus::Failed => ("failed", theme::red()),
        ItemStatus::Skipped => ("skipped", theme::grey()),
    };
    theme::badge(ui, t, c);
}

/// "done" covers both "the agent wrote a recipe" and "checks passed"; say which,
/// and whether the recipe is complete (the contents that passed a live test).
fn item_status_badge(app: &StudioApp, ui: &mut Ui, it: &BatchItem) {
    if it.status == ItemStatus::Delegated {
        let (text, color) = if it.status_note.starts_with("Needs attention") {
            ("plugin needs attention", theme::red())
        } else if it.status_note.starts_with("Waiting for you") {
            ("waiting for you", theme::amber())
        } else if it.status_note.starts_with("Running") {
            ("running in plugin", theme::blue())
        } else {
            ("in plugin", theme::blue())
        };
        theme::badge(ui, text, color).on_hover_text("This request is owned by its linked plugin work item. Open it to continue or review progress.");
        return;
    }
    // A failed item whose recipes have since passed a live test (in a working list
    // that was not open at the time) is complete; the recipe, not the old run, decides.
    let fixed_since = it.status == ItemStatus::Failed
        && it.is_recipe_work()
        && item_recipe_ids(it).iter().all(|id| app.completion(id).is_complete());
    if fixed_since {
        theme::badge(ui, "complete · not published", theme::green())
            .on_hover_text("This item failed earlier, but its recipe has since passed a live install test and is unchanged. Ready to publish.");
        return;
    }
    if it.status != ItemStatus::Done {
        return status_badge(ui, it.status);
    }
    if !it.is_recipe_work() {
        theme::badge(ui, "complete in plugin", theme::green()).on_hover_text("The linked plugin workflow completed. This request remains excluded from Recipe Builder batch actions.");
        return;
    }
    match it.action {
        ItemAction::Review => {
            theme::badge(ui, "reviewed", theme::green()).on_hover_text("The agent reviewed the recipe without changing it; its findings are in the agent's output.");
            return;
        }
        ItemAction::Freeform if it.checks.is_empty() => {
            theme::badge(ui, "agent finished", theme::blue()).on_hover_text("The agent finished the task.");
            return;
        }
        _ => {}
    }
    let ids: Vec<String> = if it.recipes.is_empty() { vec![it.recipe_id()] } else { it.recipes.clone() };
    let (text, color, hover) = if ids.iter().all(|id| app.completion(id).is_complete()) {
        ("complete · not published", theme::green(), "Passed a live install test and unchanged since. Ready to publish.")
    } else if !it.checks.is_empty() && it.checks.iter().all(|c| c.passed) {
        ("checks passed", theme::blue(), "The checks that ran passed, but no live install test of the current recipe has.")
    } else {
        let updated = matches!(it.action, ItemAction::Fix | ItemAction::Update);
        (if updated { "recipe updated" } else { "recipe written" }, theme::blue(), "The agent finished. Run the checks to validate it.")
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
            offer_batch(app, batch);
            app.notify(format!("Read {n} request(s) from {} ({format}); the quick scan found {items} item(s).", path.display()));
        }
        Err(e) => app.error(format!("Could not read {}: {e:#}", path.display())),
    }
}

fn set_batch(app: &mut StudioApp, batch: Batch) {
    app.batch.batch = Some(batch);
    app.batch.selected = None;
    app.batch.selected_items.clear();
    app.batch.saved = None;
    app.batch.composing = false;
    app.batch.dirty = false;
    save(app);
}

fn offer_batch(app: &mut StudioApp, batch: Batch) {
    if app.batch.batch.as_ref().is_some_and(|b| !b.items.is_empty()) { app.batch.pending_batch = Some(batch); }
    else { set_batch(app, batch); }
    app.view = View::Batch;
}

/// Opens a request made in the Studio (see [`Batch::from_items`]) in Recipe Builder.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn open_made(app: &mut StudioApp, batch: Batch) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active batch/agent/test run first.");
        return false;
    }
    app.batch.batch = Some(batch);
    app.batch.selected = None;
    app.batch.selected_items.clear();
    app.batch.saved = None;
    app.batch.composing = false;
    save(app);
    app.view = View::Batch;
    true
}

/// A new recipe as a one-item request: the agent writes it, then the Studio
/// tests and repairs it, exactly like an item from an imported list.
pub fn start_new_recipe(app: &mut StudioApp, item: BatchItem) -> bool {
    if app.batch.busy(app) {
        app.error("Finish or cancel the active batch/agent/test run first.");
        return false;
    }
    let added = append_items(app, vec![item]);
    let Some(&n) = added.first() else { return false };
    app.batch.composing = false;
    app.view = View::Batch;
    crate::workflow::request(app, crate::workflow::Request::BatchAuthor(vec![n]));
    true
}

/// Existing recipes as one request, to re-test, repair or update together.
pub fn start_update_request(app: &mut StudioApp, ids: &[String], notes: &str) -> bool {
    let items = update_items(app, ids, notes);
    if app.batch.busy(app) {
        app.error("Finish or cancel the active batch/agent/test run first.");
        false
    } else {
        append_items(app, items);
        app.view = View::Batch;
        app.notify("Choose what to do with them: test them, repair failures with the agent, or have the agent update them.");
        true
    }
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

fn item_contains_recipe(item: &BatchItem, id: &str) -> bool {
    item.existing_recipe.eq_ignore_ascii_case(id)
        || item.suggested_id.eq_ignore_ascii_case(id)
        || item.recipes.iter().any(|recipe| recipe.eq_ignore_ascii_case(id))
        || item.recipe_id().eq_ignore_ascii_case(id)
}

fn removal_indices(app: &StudioApp, scope: &RemoveScope) -> Vec<usize> {
    match scope {
        RemoveScope::Selected(indices) => indices.clone(),
        RemoveScope::Clear => (0..app.batch.batch.as_ref().map_or(0, |batch| batch.items.len())).collect(),
    }
}

fn open_recipe_in_scope(app: &StudioApp, scope: &RemoveScope) -> Option<String> {
    let id = app.recipe.as_ref()?.id.clone();
    let batch = app.batch.batch.as_ref()?;
    removal_indices(app, scope)
        .into_iter()
        .any(|index| batch.items.get(index).is_some_and(|item| item_contains_recipe(item, &id)))
        .then_some(id)
}

fn request_removal(app: &mut StudioApp, scope: RemoveScope) {
    // Clearing always asks for confirmation. Removing selected entries only
    // interrupts when it also affects the recipe open in the editor.
    if matches!(scope, RemoveScope::Clear) || open_recipe_in_scope(app, &scope).is_some() {
        app.batch.pending_remove = Some(scope);
    } else {
        apply_removal(app, &scope, false);
    }
}

#[cfg(test)]
pub(crate) fn prompt_clear_for_tests(app: &mut StudioApp) {
    request_removal(app, RemoveScope::Clear);
}

#[cfg(test)]
pub(crate) fn dismiss_removal_for_tests(app: &mut StudioApp) {
    app.batch.pending_remove = None;
}

fn apply_removal(app: &mut StudioApp, scope: &RemoveScope, keep_editor: bool) {
    let removed_open = open_recipe_in_scope(app, scope);
    let mut indices = removal_indices(app, scope);
    indices.sort_unstable();
    indices.dedup();
    indices.sort_unstable_by(|a, b| b.cmp(a));
    if let Some(batch) = &mut app.batch.batch {
        for index in indices {
            if index < batch.items.len() {
                batch.items.remove(index);
            }
        }
        let retained_sources: BTreeSet<String> = batch.items.iter().map(|item| item.source.clone()).collect();
        batch.records.retain(|record| retained_sources.contains(&record.reference));
    }
    app.batch.selected_items.clear();
    app.batch.selected = None;
    app.batch.dirty = true;
    save(app);

    if !keep_editor
        && let Some(id) = removed_open
    {
        let recipe_tab = matches!(app.view, View::Designer | View::Editor | View::TestBench | View::Agent)
            || (app.view == View::Icons && app.icons.batch_item.is_none() && app.icons.recipe_id.as_deref() == Some(id.as_str()));
        app.recipe = None;
        if recipe_tab {
            app.view = View::Batch;
        }
    }
}

pub fn modals(app: &mut StudioApp, ctx: &egui::Context) {
    if let Some(scope) = app.batch.pending_remove.clone() {
        let open = open_recipe_in_scope(app, &scope);
        let dirty = open.as_ref().is_some_and(|_| app.recipe.as_ref().is_some_and(|recipe| recipe.dirty()));
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("builder-remove-recipes")).show(ctx, |ui| {
            ui.set_max_width(620.0);
            let clearing = matches!(scope, RemoveScope::Clear);
            ui.label(RichText::new(if clearing { "Clear the working list?" } else { "Remove selected recipes?" }).heading());
            if let Some(id) = &open {
                if dirty {
                    ui.label(format!("'{id}' is open in the editor and has unsaved changes."));
                } else {
                    ui.label(format!("'{id}' is open in the editor and is included in this removal."));
                }
                ui.label(RichText::new("Recipe files on disk are not deleted.").small().weak());
                ui.horizontal_wrapped(|ui| {
                    if dirty && theme::primary(ui, true, "\u{E74E}  Save and remove").clicked() { choice = Some(0); }
                    if dirty && ui.button("Discard edits and remove").clicked() { choice = Some(1); }
                    if !dirty && theme::primary(ui, true, if clearing { "Clear list" } else { "Remove selected" }).clicked() { choice = Some(1); }
                    if ui.button("Keep in editor").on_hover_text("Remove the list entries but leave this recipe open in the editor").clicked() { choice = Some(2); }
                    if ui.button("Cancel").clicked() { choice = Some(3); }
                });
            } else {
                ui.label(if clearing {
                    "This removes every entry from Recipe Builder. Recipe files on disk are not deleted."
                } else {
                    "This removes the selected entries from Recipe Builder. Recipe files on disk are not deleted."
                });
                ui.horizontal(|ui| {
                    if theme::primary(ui, true, if clearing { "Clear list" } else { "Remove selected" }).clicked() { choice = Some(1); }
                    if ui.button("Cancel").clicked() { choice = Some(3); }
                });
            }
        });
        match choice {
            Some(0) => {
                if app.save_recipe() {
                    apply_removal(app, &scope, false);
                    app.batch.pending_remove = None;
                }
            }
            Some(1) => {
                apply_removal(app, &scope, false);
                app.batch.pending_remove = None;
            }
            Some(2) => {
                apply_removal(app, &scope, true);
                app.batch.pending_remove = None;
            }
            Some(3) => app.batch.pending_remove = None,
            _ => {}
        }
    }

    if app.batch.pending_batch.is_some() {
        let current = app.batch.batch.as_ref().map_or(0, |b| b.items.len());
        let incoming = app.batch.pending_batch.as_ref().map_or(0, |b| b.items.len());
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("builder-open-conflict")).show(ctx, |ui| {
            ui.set_max_width(540.0);
            ui.label(RichText::new("Add to or replace the working list?").heading());
            ui.label(format!("Recipe Builder already has {current} item(s). The list you opened contains {incoming}."));
            ui.horizontal_wrapped(|ui| {
                if theme::primary(ui, true, "Append").on_hover_text("Keep the current list and add every incoming batch item at the end.").clicked() { choice = Some(0); }
                if ui.button("Save and replace").clicked() { choice = Some(1); }
                if ui.button("Replace without saving").clicked() { choice = Some(2); }
                if ui.button("Cancel").clicked() { choice = Some(3); }
            });
            if app.batch.dirty { ui.label(RichText::new("The current list has unsaved changes.").small().color(theme::amber())); }
        });
        match choice {
            Some(0) => { let incoming = app.batch.pending_batch.take().unwrap(); append_batch(app, incoming); }
            Some(1) => {
                save(app);
                let incoming = app.batch.pending_batch.take().unwrap();
                set_batch(app, incoming);
                app.notify("Saved the previous list and opened the new one.");
            }
            Some(2) => { let incoming = app.batch.pending_batch.take().unwrap(); set_batch(app, incoming); }
            Some(3) => app.batch.pending_batch = None,
            _ => {}
        }
    }

    if let Some(name) = &mut app.batch.save_as {
        let (mut do_save, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::unique("builder-save-as")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(RichText::new("Save working list as").heading());
            ui.label("Give this Recipe Builder session a recognizable name.");
            ui.add(egui::TextEdit::singleline(name).hint_text("e.g. November application requests").desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                do_save = theme::primary(ui, !name.trim().is_empty(), "Save").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if do_save {
            let name = app.batch.save_as.take().unwrap();
            if let Some(batch) = &mut app.batch.batch {
                let stem: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
                batch.id = format!("{}-{}", intake::timestamp().replace([':', ' ', '-'], ""), stem.trim_matches('-').chars().take(40).collect::<String>());
                batch.source = PathBuf::from(name.trim());
                batch.format = "saved Recipe Builder list".into();
            }
            save(app);
            app.batch.saved = None;
            app.batch.dirty = false;
            app.notify("Recipe Builder list saved.");
        } else if cancel { app.batch.save_as = None; }
    }
}

fn append_batch(app: &mut StudioApp, incoming: Batch) {
    if app.batch.batch.is_none() { set_batch(app, incoming); return; }
    let current = app.batch.batch.as_mut().unwrap();
    let mut added = 0;
    for mut item in incoming.items {
        let old_source = item.source.clone();
        let mut source = old_source.clone();
        let mut suffix = 2;
        while current.records.iter().any(|r| r.reference == source) { source = format!("{old_source} ({suffix})"); suffix += 1; }
        if let Some(mut record) = incoming.records.iter().find(|r| r.reference == old_source).cloned() {
            record.reference = source.clone();
            current.records.push(record);
        } else {
            current.records.push(intake::Record { reference: source.clone(), title: item.app.clone(), body: item.notes.clone(), fields: Vec::new() });
        }
        item.source = source;
        current.items.push(item);
        added += 1;
    }
    app.batch.dirty = added > 0;
    app.batch.selected = current.items.len().checked_sub(1);
    save(app);
    app.notify(format!("Appended {added} recipe{} to the working list.", if added == 1 { "" } else { "s" }));
}

pub fn open_saved(app: &mut StudioApp, dir: &Path) {
    match Batch::load(dir) {
        Ok(mut b) => {
            // A run interrupted by closing the Studio is not running any more;
            // one still going (an Agent tab run, say) keeps its status.
            if !crate::workflow::works_on(app, &b.id) {
                for it in &mut b.items {
                    if matches!(it.status, ItemStatus::Running | ItemStatus::Queued) {
                        it.status = ItemStatus::Pending;
                    }
                }
            }
            offer_batch(app, b);
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

pub(crate) fn task_for(app: &StudioApp, b: &Batch, idx: usize) -> Option<AgentTask> {
    let it = b.items.get(idx)?;
    if !it.is_recipe_work() {
        return None;
    }
    // Runs started from a recipe's Agent tab repeat exactly what was asked.
    if let Some(task) = it.explicit_task() {
        return Some(task);
    }
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
        match agent::build_command(&ws, app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &task, super::settings::agent_options(app)) {
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

/// Called by the app when an agent run for a batch item ends: `Err` says why
/// it failed or what blocked it.
pub fn agent_finished(app: &mut StudioApp, batch_id: &str, idx: usize, outcome: Result<(), String>, verifying: bool) {
    let ok = outcome.is_ok();
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
            it.status_note = match &outcome {
                Ok(()) => format!("agent finished; {changed} file(s) changed"),
                Err(why) => format!("{why} ({changed} file(s) changed)"),
            };
        }
    });
    sync_plugin_recipe_work(app, batch_id, idx);
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
            let latest = check.recipe.clone();
            it.checks.push(check);
            settle_checks(it, &latest);
        }
    });
    sync_plugin_recipe_work(app, batch_id, idx);
}

/// The recipe ids an item stands for: the ones its agent reported, else its own.
fn item_recipe_ids(it: &BatchItem) -> Vec<String> {
    if it.recipes.is_empty() { vec![it.recipe_id()] } else { it.recipes.clone() }
}

/// Drops checks of recipes the item no longer names (a repair can rename or
/// replace the recipe), then derives Done/Failed from the checks that remain, so an
/// old failure cannot outlive the recipe it was about.
/// `latest` is the recipe just checked and is always kept.
fn settle_checks(it: &mut BatchItem, latest: &str) {
    let ids = item_recipe_ids(it);
    it.checks.retain(|c| c.recipe.eq_ignore_ascii_case(latest) || ids.iter().any(|id| id.eq_ignore_ascii_case(&c.recipe)));
    it.status = if it.checks.iter().all(|c| c.passed) { ItemStatus::Done } else { ItemStatus::Failed };
}

/// A live install test validated `id` (from any view). Items of the open working
/// list that use the recipe and are not mid-run record the passing check, so a
/// recipe that was fixed elsewhere stops reading "failed".
pub fn recipe_validated(app: &mut StudioApp, id: &str, timestamp: &str, summary_dir: &std::path::Path) {
    let Some(b) = app.batch.batch.as_mut() else { return };
    let batch_id = b.id.clone();
    let mut touched = Vec::new();
    for (idx, it) in b.items.iter_mut().enumerate() {
        if !it.is_recipe_work()
            || !matches!(it.status, ItemStatus::Failed | ItemStatus::Done)
            || !item_recipe_ids(it).iter().any(|r| r.eq_ignore_ascii_case(id))
        {
            continue;
        }
        it.checks.retain(|c| !c.recipe.eq_ignore_ascii_case(id));
        it.checks.push(BatchCheck {
            recipe: id.to_string(),
            passed: true,
            note: "Validated by a live install test".into(),
            timestamp: timestamp.to_string(),
            summary_dir: Some(summary_dir.to_path_buf()),
            live: true,
        });
        settle_checks(it, id);
        if it.status == ItemStatus::Done {
            it.status_note = format!("{id} validated by a live install test.");
        }
        touched.push(idx);
    }
    if touched.is_empty() {
        return;
    }
    save(app);
    for idx in touched {
        sync_plugin_recipe_work(app, &batch_id, idx);
    }
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
            sync_plugin_recipe_work(app, batch_id, *idx);
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
    for (idx, item) in batch.items.iter().enumerate().filter(|(_, i)| i.include && i.is_recipe_work() && i.request != RequestKind::NotSoftware) {
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
    let plugin_ids: Vec<String> = app.plugins.registry.plugins.iter().map(|p| p.manifest.id.clone()).collect();
    let mut result = Err(anyhow::anyhow!("batch not found"));
    with_batch(app, batch_id, |b| result = b.load_agent_items(&b.dir(), &recipes, &plugin_ids, backend));
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
    ui.horizontal_wrapped(|ui| {
        theme::page_title(ui, "Recipe Builder");
        ui.label(RichText::new("new recipes, imported request lists and recipe updates - written, tested and repaired together").weak());
    });
    // Dropped request lists open as a new batch.
    let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
    if let Some(p) = dropped.into_iter().find(|p| {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| intake::EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
    }) {
        open_file(app, &p);
    }

    egui::Panel::left("builder-working-list")
        .resizable(true)
        .default_size(250.0)
        .size_range(210.0..=380.0)
        .frame(theme::pane_frame())
        .show(ui, |ui| working_list(app, ui));
    if app.batch.composing {
        theme::pane(ui, |ui| new_recipe_form(app, ui));
        return;
    }
    theme::pane(ui, |ui| {
        toolbar(app, ui);
        ui.separator();
    });
    if app.batch.batch.is_none() {
        theme::pane(ui, |ui| start_page(app, ui));
        return;
    }
    if app.batch.selected.is_some() {
        egui::Panel::right("batch-detail")
            .resizable(true)
            .default_size(380.0)
            .frame(theme::pane_frame())
            .show(ui, |ui| detail(app, ui));
    }
    theme::pane(ui, |ui| {
        items_header(app, ui);
        let show_log = app.batch.show_log;
        let log_job = if app.workflow.busy() { app.workflow.last_job } else { app.batch.run_job.or(app.batch.extract_job) };
        // The console already shows this job; a second copy only takes space.
        let in_console = app.console_open && app.console_job.is_some() && app.console_job == log_job;
        // Below the header, and only when the table keeps room for its rows; the
        // Jobs console shows the same output in a short window.
        let room = ui.available_height() - TABLE_MIN_HEIGHT;
        if show_log && !in_console && room >= LOG_MIN_HEIGHT && let Some(id) = log_job {
            egui::Panel::bottom("batch-log").resizable(true).default_size(220.0).size_range(LOG_MIN_HEIGHT..=room).show(ui, |ui| {
                if let Some(job) = app.job_mut(id) {
                    job_log(ui, &mut job.proc, "batch");
                }
            });
        }
        items_rows(app, ui);
    });
}

/// The single-recipe tools inside the unified Recipe Builder. The current
/// recipe page remains in `app.view`, so the existing tab routing, shortcuts,
/// and per-page state continue to work while the Builder list stays visible.
pub fn show_recipe(app: &mut StudioApp, ui: &mut Ui) {
    let remaining = app.batch.batch.as_ref().map_or(0, |batch| {
        batch.items.iter().filter(|item| item.include && item.is_recipe_work() && item.request != RequestKind::NotSoftware && item.status != ItemStatus::Done).count()
    });
    let running = app.batch.busy(app);
    let mut back = false;
    ui.horizontal_wrapped(|ui| {
        theme::page_title(ui, "Recipe Builder");
        ui.label(RichText::new("edit and validate one recipe without leaving the working list").weak());
        if running {
            theme::badge(ui, "batch running", theme::blue());
        } else if remaining > 0 {
            theme::badge(ui, format!("{remaining} remaining"), theme::amber());
        }
        back = ui.button("\u{E72B}  Batch overview").on_hover_text("Return to the batch controls; this recipe and all batch progress stay open").clicked();
    });
    egui::Panel::left("builder-recipe-working-list")
        .resizable(true)
        .default_size(210.0)
        .size_range(180.0..=320.0)
        .frame(theme::pane_frame())
        .show(ui, |ui| working_list(app, ui));
    theme::pane(ui, |ui| match app.view {
        View::Designer => super::designer::show(app, ui),
        View::Editor => super::editor::show(app, ui),
        View::TestBench => super::testbench::show(app, ui),
        View::Icons => super::icons::show(app, ui),
        View::Agent => super::agent::show(app, ui),
        _ => {}
    });
    if back {
        app.view = View::Batch;
    }
}

/// Room kept for the table's header and a few rows.
const TABLE_MIN_HEIGHT: f32 = 140.0;
const LOG_MIN_HEIGHT: f32 = 90.0;

fn working_list(app: &mut StudioApp, ui: &mut Ui) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Working list").heading());
        if app.batch.dirty { theme::badge(ui, "unsaved", theme::amber()); }
    });
    ui.add_space(4.0);
    let count = app.batch.batch.as_ref().map_or(0, |b| b.items.len());
    let picked = app.batch.selected_items.len();
    let summary = if picked == 0 {
        format!("{count} recipe{}", if count == 1 { "" } else { "s" })
    } else {
        format!("{count} recipes · {picked} selected")
    };
    ui.label(RichText::new(summary).small().weak());
    ui.separator();
    let rows: Vec<(usize, BatchItem)> = app.batch.batch.as_ref().map(|b| b.items.iter().cloned().enumerate().collect()).unwrap_or_default();
    let busy = app.batch.busy(app);
    let mut active = None;
    let list_height = (ui.available_height() - 74.0).max(120.0);
    egui::ScrollArea::vertical().id_salt("builder-working-items").max_height(list_height).auto_shrink(false).show(ui, |ui| {
        for (n, item) in rows {
            let mut checked = app.batch.selected_items.contains(&n);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Checkbox::without_text(&mut checked))
                    .on_hover_text("Select this recipe for batch actions")
                    .changed()
                {
                    if checked { app.batch.selected_items.insert(n); } else { app.batch.selected_items.remove(&n); }
                }
                let title = if item.app.trim().is_empty() { item_key(&item) } else { item.app.clone() };
                let selected = !app.batch.composing && app.batch.selected == Some(n);
                if ui.add(egui::Button::selectable(selected, RichText::new(title).semibold())).clicked() { active = Some(n); }
            });
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                let id = item_key(&item);
                if !id.is_empty() { ui.label(RichText::new(id).small().monospace().weak()); }
                item_status_badge(app, ui, &item);
            });
            ui.add_space(2.0);
        }
        if count == 0 {
            ui.add_space(12.0);
            ui.label(RichText::new("Add an existing recipe, create a new one, or import a request list.").weak());
        }
    });
    if let Some(n) = active {
        app.batch.selected = Some(n);
        app.batch.composing = false;
        let id = app.batch.batch.as_ref().and_then(|b| b.items.get(n)).and_then(|item| existing(app, item)).map(|r| r.id.clone());
        if let Some(id) = id
            && app.open_recipe_by_id(&id)
        {
            app.view = View::Designer;
        }
    }
    ui.separator();
    let mut action = None;
    ui.horizontal(|ui| {
        if ui.add_enabled(!busy, egui::Button::new("\u{E710}")).on_hover_text("Repeat the last Add action").clicked() { action = Some(app.batch.last_add); }
        ui.menu_button("\u{E70D}", |ui| {
            if ui.button("Add existing recipe…").clicked() { action = Some(AddAction::Existing); ui.close(); }
            if ui.button("Create new recipe…").clicked() { action = Some(AddAction::New); ui.close(); }
            ui.separator();
            if ui.button("Import request list…").clicked() { action = Some(AddAction::Import); ui.close(); }
            if ui.button("Paste request list…").clicked() { action = Some(AddAction::Paste); ui.close(); }
        }).response.on_hover_text("Choose what to add");
        if ui.add_enabled(!busy && !app.batch.selected_items.is_empty(), egui::Button::new("\u{E738}"))
            .on_hover_text("Remove selected recipes from this list").clicked()
        {
            let indices = app.batch.selected_items.iter().copied().collect();
            request_removal(app, RemoveScope::Selected(indices));
        }
        if ui.add_enabled(!busy && count > 0, egui::Button::new("Clear list")).clicked() { request_removal(app, RemoveScope::Clear); }
    });
    if let Some(action) = action {
        app.batch.last_add = action;
        match action {
            AddAction::Existing => { app.library.adding_to_builder = true; app.view = View::Recipes; }
            AddAction::New => show_new_recipe(app),
            AddAction::Import => if let Some(path) = rfd::FileDialog::new().add_filter("Request lists", intake::EXTENSIONS).add_filter("All files", &["*"]).pick_file() { open_file(app, &path); },
            AddAction::Paste => app.batch.paste = Some(String::new()),
        }
    }
}

fn new_recipe_form(app: &mut StudioApp, ui: &mut Ui) {
    let Some(ws) = app.ws.clone() else { super::need_workspace(app, ui); return };
    let templates = app.batch.new_recipe.templates.get_or_insert_with(|| ws.templates()).clone();
    let can_agent = app.writable() && app.ws_cache.has_skill;
    let agent_name = app.settings.agent_backend.label();
    theme::section_heading(ui, "Create a recipe");
    ui.label(RichText::new("Add one application to the working list, or start from a template and edit it yourself.").weak());
    ui.add_space(12.0);
    let (mut with_agent, mut create) = (false, false);
    theme::card(ui, |ui| {
        let draft = &mut app.batch.new_recipe;
        egui::Grid::new("builder-new-recipe-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Application");
            if ui.add(egui::TextEdit::singleline(&mut draft.display).hint_text("e.g. Notepad++").desired_width(360.0)).changed() && !draft.id_touched { draft.id = recipe::suggest_id(&draft.display); }
            ui.end_row();
            ui.label("Publisher"); ui.add(egui::TextEdit::singleline(&mut draft.publisher).hint_text("optional").desired_width(360.0)); ui.end_row();
            ui.label("Vendor page"); ui.add(egui::TextEdit::singleline(&mut draft.vendor).hint_text("optional - where research starts").desired_width(360.0)); ui.end_row();
            ui.label("Recipe id"); if ui.add(egui::TextEdit::singleline(&mut draft.id).desired_width(360.0)).changed() { draft.id_touched = true; } ui.end_row();
            ui.label("Notes"); ui.add(egui::TextEdit::multiline(&mut draft.notes).hint_text("optional - licensing, architecture, quirks…").desired_rows(3).desired_width(360.0)); ui.end_row();
        });
        let id = draft.id.trim();
        let valid = recipe::is_valid_id(id);
        let development_exists = app.recipes.iter().any(|r| r.id.eq_ignore_ascii_case(id) && r.location == ystudio_core::workspace::RecipeLocation::Development);
        let shipped_exists = app.recipes.iter().any(|r| r.id.eq_ignore_ascii_case(id) && r.location != ystudio_core::workspace::RecipeLocation::Development);
        if !id.is_empty() && !valid { ui.colored_label(theme::red(), "Ids use lowercase letters, digits, dots, dashes, or underscores."); }
        else if development_exists { ui.colored_label(theme::red(), "A development recipe with this id already exists."); }
        else if shipped_exists { ui.colored_label(theme::amber(), "A shipped recipe has this id; the development copy will take precedence."); }
        let ready = valid && !development_exists && !draft.display.trim().is_empty();
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            with_agent = theme::primary(ui, ready && can_agent, format!("\u{E99A}  Write with {agent_name}")).clicked();
            ui.separator();
            ui.label("Start from template");
            let template_name = |i: usize| templates.get(i).and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "(blank)".into());
            egui::ComboBox::from_id_salt("builder-template").selected_text(template_name(draft.template)).show_ui(ui, |ui| {
                for i in 0..templates.len() { ui.selectable_value(&mut draft.template, i, template_name(i)); }
                ui.selectable_value(&mut draft.template, templates.len(), "(blank)");
            });
            create = ui.add_enabled(ready, egui::Button::new("Create and open")).clicked();
        });
    });
    if with_agent {
        let d = std::mem::take(&mut app.batch.new_recipe);
        start_new_recipe(app, BatchItem { app: d.display.trim().to_string(), publisher: d.publisher.trim().to_string(), vendor_url: d.vendor.trim().to_string(), suggested_id: d.id.trim().to_string(), notes: d.notes.trim().to_string(), ..Default::default() });
    } else if create { create_from_template(app, &ws, &templates); }
}

fn create_from_template(app: &mut StudioApp, ws: &ystudio_core::workspace::Workspace, templates: &[PathBuf]) {
    let d = &app.batch.new_recipe;
    let id = d.id.trim().to_string();
    let result = (|| -> anyhow::Result<PathBuf> {
        ws.ensure_writable("create recipes")?;
        let path = ws.development_recipe_path(&id);
        anyhow::ensure!(!path.exists(), "{} already exists.", path.display());
        let doc = match templates.get(d.template) {
            Some(template) => recipe::new_from_template(&ystudio_core::read_text(template)?, &id, d.display.trim(), d.publisher.trim())?,
            None => { let mut doc = RecipeDoc::default(); doc.set_from_text("id", &id); doc.set_from_text("displayName", d.display.trim()); doc.set_from_text("publisher", d.publisher.trim()); doc }
        };
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, doc.to_yaml())?;
        Ok(path)
    })();
    match result {
        Ok(path) => {
            app.batch.new_recipe = NewRecipeDraft::default();
            app.batch.composing = false;
            app.refresh_recipes();
            add_existing_recipe(app, &id);
            app.request_open_recipe(path);
            app.view = View::Designer;
            app.notify(format!("Created {id}. Walk through each step, then test it."));
        }
        Err(error) => app.error(format!("Could not create recipe: {error:#}")),
    }
}

fn toolbar(app: &mut StudioApp, ui: &mut Ui) {
    let busy = app.batch.busy(app);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!busy && app.batch.batch.is_some(), egui::Button::new("\u{E74E}  Save")).clicked() {
            save(app);
            app.batch.dirty = false;
            app.notify("Recipe Builder list saved.");
        }
        if ui.add_enabled(!busy && app.batch.batch.is_some(), egui::Button::new("Save as…")).clicked() { app.batch.save_as = Some(String::new()); }
        let saved = app.batch.saved.get_or_insert_with(Batch::list).clone();
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("batch-saved").selected_text(format!("Load… ({})", saved.len())).height(400.0).show_ui(ui, |ui| {
                    for (name, dir) in &saved {
                        if ui.selectable_label(false, name).clicked() {
                            open_saved(app, dir);
                            ui.close();
                        }
                    }
                    if saved.is_empty() {
                        ui.label(RichText::new("No saved lists yet.").weak());
                    }
                });
        });
        if ui.button("\u{E72C}").on_hover_text("Refresh saved lists").clicked() {
            app.batch.saved = None;
        }
        if let Some(b) = &app.batch.batch {
            let dir = b.dir();
            if ui.button("\u{E8C8}  Copy report").on_hover_text("Markdown table to paste back into the tickets").clicked() {
                let r = b.report();
                ui.ctx().copy_text(r);
                app.notify("Report copied to the clipboard.");
            }
            if ui.button("\u{E8B7}  List folder").on_hover_text("Open this list's files in Explorer").clicked() {
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
        ui.label("3. For messy tickets, “Extract with agent” has your agent read every request and return a clean list.");
        ui.label("4. Complete every remaining recipe together, select a subset, or open one recipe and finish it individually.");
        ui.label("5. Pause after the active recipe whenever you need to inspect or edit one, then resume the remaining work.");
    });
    if app.ws.is_none() {
        ui.label(RichText::new("Tip: open a Yardstick folder first so items can be matched to existing recipes.").small().weak());
    }
}

fn items_header(app: &mut StudioApp, ui: &mut Ui) {
    let busy = app.batch.busy(app);
    let can_agent = app.writable() && app.ws_cache.has_skill;
    let can_test = app.writable();
    let running_extract = app.batch.extract_job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running());
    let running_items = !app.workflow.running() && (app.batch.run_job.and_then(|j| app.job(j)).is_some_and(|j| j.proc.is_running()) || !app.batch.queue.is_empty());
    let queued = app.batch.queue.len();
    let backend = app.settings.agent_backend.label();
    let b = app.batch.batch.as_ref().unwrap();
    let software = b.items.iter().filter(|i| i.request != RequestKind::NotSoftware).count();
    let included = b.items.iter().filter(|i| i.include && i.is_recipe_work() && i.request != RequestKind::NotSoftware).count();
    let pending = b.items.iter().filter(|i| i.include && i.is_recipe_work() && i.request != RequestKind::NotSoftware && i.status != ItemStatus::Done).count();
    let selected_pending = app
        .batch
        .selected_items
        .iter()
        .filter(|&&n| b.items.get(n).is_some_and(|i| i.include && i.is_recipe_work() && i.request != RequestKind::NotSoftware && i.status != ItemStatus::Done))
        .count();
    let has_selection = !app.batch.selected_items.is_empty();
    let repairable = repair_targets(app, b).len();
    let file = b.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let requests = b.records.len();
    let summary = format!(
        "{requests} request{} · {software} software item{} · extracted by {}",
        if requests == 1 { "" } else { "s" },
        if software == 1 { "" } else { "s" },
        b.extracted_by,
    );
    // Requests made in the Studio are already a clean list.
    let extractable = b.extracted_by != "Studio";
    let format = b.format.clone();

    // Header.
    let mut extract = false;
    let mut queue_all = false;
    let mut queue_selected = false;
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
                    .on_hover_text(format!("{backend} reads every request, matches recipes, and routes specialist work to installed plugins (replaces the current items)."))
                    .clicked()
                {
                    extract = true;
                }
                ui.separator();
            }
            if running_items {
                ui.spinner();
                ui.label(format!("Running batch · {queued} waiting"));
                if ui.button("\u{E769}  Pause after this recipe").on_hover_text("Finish the active recipe, then leave the remaining recipes pending so you can inspect or edit one").clicked() {
                    stop = true;
                }
                if ui.button("\u{E711}  Cancel now").clicked() {
                    stop = true;
                    cancel = true;
                }
            } else {
                let (complete_count, selected_action) = if has_selection { (selected_pending, true) } else { (pending, false) };
                let label = if selected_action {
                    format!("\u{E768}  Complete {complete_count} selected")
                } else {
                    format!("\u{E768}  Complete all {complete_count} remaining")
                };
                let resp = theme::primary(ui, can_agent && complete_count > 0 && !busy, RichText::new(label));
                if !can_agent {
                    resp.on_disabled_hover_text("Needs a writable development folder with the yardstick-recipes skill.");
                } else if complete_count == 0 {
                    resp.on_disabled_hover_text(if selected_action { "The selected recipes are already complete or excluded." } else { "Every included recipe is complete." });
                } else if busy {
                    resp.on_disabled_hover_text("Finish or cancel the active run first.");
                } else if resp.clicked() {
                    if selected_action { queue_selected = true; } else { queue_all = true; }
                }
                if has_selection
                    && ui.add_enabled(can_agent && pending > 0 && !busy, egui::Button::new(format!("Complete all {pending} remaining"))).clicked()
                {
                    queue_all = true;
                }
            }
            if ui.add_enabled(can_test && included > 0 && !busy, egui::Button::new(format!("Test all {included} included"))).clicked() {
                test = true;
            }
            let resp = ui.add_enabled(can_agent && repairable > 0 && !busy, egui::Button::new(format!("\u{E90F}  Repair {repairable} unfinished")));
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
        ui.horizontal_wrapped(|ui| {
            ui.label("Agent");
            super::settings::agent_picker(app, ui, "requests");
        });
        super::settings::downloads_checkbox(app, ui);
        crate::workflow::configure(&mut app.workflow.options, ui, true);
    });
    let ids: Vec<String> = app.batch.batch.as_ref().unwrap().items.iter()
        .filter(|i| i.include && i.is_recipe_work() && i.request != RequestKind::NotSoftware)
        .flat_map(|i| if i.recipes.is_empty() { vec![i.recipe_id()] } else { i.recipes.clone() })
        .collect();
    super::dropbox_banner(app, ui, &ids);
    crate::workflow::progress(app, ui);
    if extract {
        start_extract(app);
    }
    if queue_all || queue_selected {
        let b = app.batch.batch.as_ref().unwrap();
        let indices = b
            .items
            .iter()
            .enumerate()
            .filter(|(n, i)| {
                i.include
                    && i.is_recipe_work()
                    && i.request != RequestKind::NotSoftware
                    && i.status != ItemStatus::Done
                    && (!queue_selected || app.batch.selected_items.contains(n))
            })
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

}

fn items_rows(app: &mut StudioApp, ui: &mut Ui) {
    let busy = app.batch.busy(app);
    let total = app.batch.batch.as_ref().unwrap().items.len();
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
        egui::Grid::new("batch-grid").striped(true).num_columns(10).spacing([12.0, 6.0]).min_row_height(24.0).show(ui, |ui| {
            for h in ["", "Ticket", "Software", "Version", "Request", "Destination", "Status", "Icon", "Recipe", "Credits"] {
                ui.label(RichText::new(h).semibold());
            }
            ui.end_row();
            for (n, it, (icon_text, icon_color), exists) in &rows {
                let mut inc = it.include;
                if ui
                    .add_enabled(it.request != RequestKind::NotSoftware && it.is_recipe_work() && !busy, egui::Checkbox::without_text(&mut inc))
                    .on_hover_text("Include this request in Recipe Builder batch actions")
                    .changed()
                {
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
                let destination = if it.is_recipe_work() {
                    "Recipe Builder".to_string()
                } else {
                    app.plugins.get(&it.handler).map(|p| p.manifest.name.clone()).unwrap_or_else(|| it.handler.clone())
                };
                theme::badge(ui, destination, if it.is_recipe_work() { theme::blue() } else { theme::amber() });
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
                    if it.request != RequestKind::NotSoftware && it.is_recipe_work() {
                        if ui.small_button("\u{E91B}").on_hover_text("Find an icon").clicked() {
                            icon_for = Some(*n);
                        }
                        ui.label(RichText::new(icon_text).small().color(*icon_color));
                    }
                });
                ui.horizontal(|ui| {
                    let id = if it.recipes.is_empty() { it.recipe_id() } else { it.recipes.join(", ") };
                    if it.request == RequestKind::NotSoftware || !it.is_recipe_work() {
                        ui.label("");
                    } else if *exists {
                        if ui.link(RichText::new(&id).monospace()).on_hover_text("Open in Design").clicked() {
                            open = Some(id.split(", ").next().unwrap_or_default().to_string());
                        }
                    } else {
                        ui.label(RichText::new(id).monospace().weak()).on_hover_text("Suggested id - no recipe yet");
                    }
                });
                // Every agent run on the item's recipes, in any request.
                let ids = if it.recipes.is_empty() { vec![it.recipe_id()] } else { it.recipes.clone() };
                let credits: f64 = ids.iter().map(|id| ystudio_core::usage::recipe_credits(&app.usage, id)).sum();
                if credits > 0.0 {
                    ui.label(format!("{credits:.1}")).on_hover_text("AI credits spent by agents on this app so far");
                } else {
                    ui.label("");
                }
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
    let handlers: Vec<(String, String, String)> = app
        .plugins
        .registry
        .plugins
        .iter()
        .filter(|p| p.manifest.page.kind == ystudio_core::plugin::PageKind::Workitems)
        .map(|p| (p.manifest.id.clone(), p.manifest.name.clone(), p.manifest.description.clone()))
        .collect();
    if let Err(e) = b.prepare_agent(&dir, &app.recipes, &handlers) {
        app.error(format!("Could not prepare the batch: {e:#}"));
        return;
    }
    match agent::build_extract_command(app.settings.agent_backend, &app.settings.agent_model, &dir, super::settings::agent_options(app)) {
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
    let handlers: Vec<(String, String)> = app
        .plugins
        .registry
        .plugins
        .iter()
        .filter(|p| p.manifest.page.kind == ystudio_core::plugin::PageKind::Workitems)
        .map(|p| (p.manifest.id.clone(), p.manifest.name.clone()))
        .collect();
    let Some(b) = app.batch.batch.as_mut() else { return };
    let Some(it) = b.items.get(n).cloned() else {
        app.batch.selected = None;
        return;
    };
    let record = b.record(&it.source).cloned();
    let mut edit = it.clone();
    let linked_origin = edit.plugin_origins.first().cloned();
    let mut run = false;
    let mut open_recipe = false;
    let mut icon = false;
    let mut handoff = false;
    let mut open_handoff = false;
    let mut open_linked_plugin = false;
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
        if let Some(origin) = &linked_origin {
            let p = theme::palette();
            egui::Frame::new()
                .fill(p.info_bg)
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("\u{E946}").color(p.info));
                        ui.label(
                            RichText::new(format!(
                                "Linked to {} product {}. Passing safe checks copies this development recipe back to the plugin work item.",
                                origin.plugin, origin.product_id
                            ))
                            .color(p.info),
                        );
                        let name = handlers
                            .iter()
                            .find(|(id, _)| id == &origin.plugin)
                            .map(|(_, name)| name.as_str())
                            .unwrap_or("plugin");
                        if ui.button(format!("Back to {name}")).clicked() {
                            open_linked_plugin = true;
                        }
                    });
                });
            ui.add_space(6.0);
        }
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
            if edit.action != ItemAction::Auto {
                ui.label("Task");
                ui.label(RichText::new(edit.action.label()).semibold()).on_hover_text("Started from the recipe's Agent tab; Run agent repeats it.");
                ui.end_row();
            }
            ui.label("Request");
            egui::ComboBox::from_id_salt("batch-req").selected_text(edit.request.label()).show_ui(ui, |ui| {
                for k in RequestKind::ALL {
                    ui.selectable_value(&mut edit.request, k, k.label());
                }
            });
            ui.end_row();
            ui.label("Destination");
            let destination = if edit.is_recipe_work() {
                "Recipe Builder".to_string()
            } else {
                handlers.iter().find(|(id, _)| id == &edit.handler).map(|(_, name)| name.clone()).unwrap_or_else(|| edit.handler.clone())
            };
            let fixed_destination = linked_origin.is_some() || (!edit.is_recipe_work() && edit.handler_item.is_some());
            if fixed_destination {
                ui.label(RichText::new(destination).semibold())
                    .on_hover_text("This destination owns linked work already. Open that work item to continue.");
            } else {
                egui::ComboBox::from_id_salt("batch-handler").selected_text(destination).show_ui(ui, |ui| {
                    ui.selectable_value(&mut edit.handler, String::new(), "Recipe Builder · reusable recipe");
                    for (id, name) in &handlers {
                        ui.selectable_value(&mut edit.handler, id.clone(), name);
                    }
                });
            }
            ui.end_row();
            if edit.is_recipe_work() {
                ui.label("Existing recipe");
                if linked_origin.is_some() {
                    ui.label(if edit.existing_recipe.is_empty() { "none" } else { &edit.existing_recipe })
                        .on_hover_text("The plugin product id determines which development recipe is synchronized.");
                } else {
                    egui::ComboBox::from_id_salt("batch-existing")
                        .selected_text(if edit.existing_recipe.is_empty() { "none".to_string() } else { edit.existing_recipe.clone() })
                        .height(400.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut edit.existing_recipe, String::new(), "none");
                            for id in &ids {
                                ui.selectable_value(&mut edit.existing_recipe, id.clone(), id);
                            }
                        });
                }
                ui.end_row();
                ui.label("New recipe id");
                ui.add_enabled(
                    linked_origin.is_none(),
                    egui::TextEdit::singleline(&mut edit.suggested_id)
                        .hint_text(ystudio_core::recipe::suggest_id(&edit.app))
                        .desired_width(w),
                )
                .on_disabled_hover_text("The linked plugin product id fixes this recipe id so synchronization cannot target the wrong product.");
                ui.end_row();
            }
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
        ui.label(if edit.action == ItemAction::Freeform { "Prompt" } else { "Notes for the agent" });
        ui.add(egui::TextEdit::multiline(&mut edit.notes).desired_rows(3).desired_width(f32::INFINITY));
        if edit.action == ItemAction::Fix && !edit.evidence.is_empty() {
            ui.collapsing("Evidence the fix started from", |ui| {
                egui::ScrollArea::vertical().id_salt("batch-evidence").max_height(200.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut edit.evidence).code_editor().desired_width(f32::INFINITY));
                });
            });
        }
        if edit.confidence > 0.0 {
            ui.label(RichText::new(format!("Extractor confidence {:.0}%", edit.confidence * 100.0)).small().weak());
        }
        ui.horizontal_wrapped(|ui| {
            let can_run = can_agent && edit.is_recipe_work() && edit.request != RequestKind::NotSoftware;
            if edit.is_recipe_work()
                && theme::primary(ui, can_run, "\u{E768}  Complete this recipe")
                    .on_disabled_hover_text("Needs a writable development folder, the recipe skill, and no other active run.")
                    .clicked()
            {
                run = true;
            }
            if !edit.is_recipe_work() {
                let name = handlers.iter().find(|(id, _)| id == &edit.handler).map(|(_, name)| name.as_str()).unwrap_or("selected plugin");
                if edit.handler_item.as_ref().is_some_and(|path| path.is_dir()) {
                    if ui.button(format!("\u{E8A5}  Open in {name}")).clicked() {
                        open_handoff = true;
                    }
                } else if ui.button(format!("\u{E72A}  Send to {name}")).on_hover_text("Creates a plugin work item with the original request and extracted details.").clicked() {
                    handoff = true;
                }
            }
            if existing(app, &edit).is_some()
                && ui.button("\u{E70F}  Open recipe").on_hover_text("Inspect or edit this recipe, then return to the batch overview to continue").clicked()
            {
                open_recipe = true;
            }
            if edit.is_recipe_work()
                && ui.add_enabled(edit.request != RequestKind::NotSoftware, egui::Button::new("\u{E91B}  Find icon")).clicked()
            {
                icon = true;
            }
            if edit.status == ItemStatus::Skipped {
                if ui.button("Unskip").clicked() {
                    edit.status = ItemStatus::Pending;
                }
            } else if edit.handler_item.is_none()
                && !matches!(edit.status, ItemStatus::Running | ItemStatus::Queued | ItemStatus::Delegated)
                && ui.button("Skip").clicked()
            {
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
            ui.add(egui::TextEdit::multiline(&mut r.body.as_str()).desired_rows(12).desired_width(f32::INFINITY));
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
        let sync_linked_work = !edit.plugin_origins.is_empty();
        if edit.handler != it.handler {
            edit.handler_item = None;
            edit.include = edit.is_recipe_work() && matches!(edit.request, RequestKind::New | RequestKind::Update);
            edit.status = ItemStatus::Pending;
            edit.status_note.clear();
        }
        if edit.is_recipe_work()
            && (edit.app != it.app || edit.version != it.version || edit.existing_recipe != it.existing_recipe
                || edit.suggested_id != it.suggested_id || edit.vendor_url != it.vendor_url
                || edit.notes != it.notes || edit.publisher != it.publisher)
        {
            edit.recipes.clear();
            edit.checks.clear();
            edit.status = ItemStatus::Pending;
            edit.status_note.clear();
        }
        app.batch.batch.as_mut().unwrap().items[n] = edit.clone();
        save(app);
        if sync_linked_work
            && let Some(batch_id) = app.batch.batch.as_ref().map(|batch| batch.id.clone())
        {
            sync_plugin_recipe_work(app, &batch_id, n);
        }
    }
    if close {
        app.batch.selected = None;
    }
    if run {
        crate::workflow::request(app, crate::workflow::Request::BatchAuthor(vec![n]));
    }
    if open_recipe
        && let Some(id) = existing(app, &it).map(|recipe| recipe.id.clone())
        && app.open_recipe_by_id(&id)
    {
        app.view = View::Designer;
    }
    if icon {
        find_icon(app, n);
    }
    if handoff {
        let current = app.batch.batch.as_ref().and_then(|b| b.items.get(n)).cloned();
        let batch_id = app.batch.batch.as_ref().map(|b| b.id.clone()).unwrap_or_default();
        if let Some(current) = current {
            let text = current.agent_notes(record.as_ref());
            match crate::views::plugin_page::create_from_recipe_builder(
                app,
                &current.handler,
                &current.app,
                &current.source,
                &text,
                &format!("{batch_id}:{n}"),
            ) {
                Ok(path) => {
                    let plugin_name = app
                        .plugins
                        .get(&current.handler)
                        .map(|plugin| plugin.manifest.name.clone())
                        .unwrap_or_else(|| current.handler.clone());
                    if let Some(item) = app.batch.batch.as_mut().and_then(|b| b.items.get_mut(n)) {
                        item.handler_item = Some(path);
                        item.status = ItemStatus::Delegated;
                        item.status_note = format!("Started in {plugin_name}. Open the linked work item to continue.");
                    }
                    save(app);
                }
                Err(e) => app.error(format!("Could not send the request: {e:#}")),
            }
        }
    } else if open_handoff
        && let Some(current) = app.batch.batch.as_ref().and_then(|b| b.items.get(n)).cloned()
        && let Some(view) = app.plugins.view_of(&current.handler)
    {
        if let Some(path) = current.handler_item {
            app.plugins.page(&current.handler).selected = Some(path);
        }
        app.view = view;
    }
    if open_linked_plugin
        && let Some(origin) = linked_origin
        && let Some(view) = app.plugins.view_of(&origin.plugin)
    {
        app.plugins.page(&origin.plugin).selected = Some(origin.work_item);
        app.view = view;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::plugin_page::tests::install_sample;
    use crate::workflow::tests::fake_app;

    #[test]
    fn working_list_deduplicates_library_but_appends_every_batch_item() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\nRuntimeRoot = '.'\nRecipesPath = 'Recipes'\nDevelopmentRecipes = 'Recipes\\Development'\n}\n").unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# never executed").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        std::fs::write(root.join(r"Recipes\Development\one.yaml"), "id: one\ndisplayName: One\n").unwrap();
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        let mut app = StudioApp::isolated_for_tests(ws);
        let mut working = Batch::from_items("working", Vec::new());
        working.id = root.join("working-list").to_string_lossy().into_owned();
        app.batch.batch = Some(working);

        add_existing_recipe(&mut app, "one");
        add_existing_recipe(&mut app, "ONE");
        assert_eq!(app.batch.batch.as_ref().unwrap().items.len(), 1);

        let incoming = Batch::from_items("incoming", vec![
            BatchItem { app: "One again".into(), existing_recipe: "one".into(), ..Default::default() },
            BatchItem { app: "Two".into(), suggested_id: "two".into(), ..Default::default() },
        ]);
        append_batch(&mut app, incoming);
        let items = &app.batch.batch.as_ref().unwrap().items;
        assert_eq!(items.len(), 3);
        assert_eq!(item_key(&items[1]), "one");
        assert_eq!(item_key(&items[2]), "two");
        assert!(app.batch.dirty);
    }

    #[test]
    fn passing_recipe_builder_work_syncs_the_checked_recipe_back_to_its_plugin() {
        let (dir, mut app) = fake_app();
        let work_root = install_sample(&mut app, dir.path(), "sample-plugin", |manifest| manifest);
        let work = WorkItem::create(
            &work_root,
            "{date}-{slug}",
            "Linked recipe",
            "TASK-1",
            plugin::Origin::default(),
            std::time::SystemTime::now(),
        ).unwrap();
        let workspace = app.ws.as_ref().unwrap();
        std::fs::create_dir_all(&workspace.layout.development_recipes).unwrap();
        std::fs::create_dir_all(&workspace.layout.icons).unwrap();
        std::fs::write(
            workspace.layout.development_recipes.join("one.yaml"),
            "id: one\ndisplayName: One\npublisher: Test\niconFile: one.png\nurl: https://example.com\ninstallScript: one.exe /S\nuninstallScript: one.exe /U\ndetectionType: file\n",
        ).unwrap();
        std::fs::write(workspace.layout.icons.join("one.png"), b"icon").unwrap();
        app.refresh_recipes();

        let recipe_item = BatchItem {
            app: "One".into(),
            suggested_id: "one".into(),
            status: ItemStatus::Done,
            recipes: vec!["one".into()],
            checks: vec![BatchCheck {
                recipe: "one".into(),
                passed: true,
                note: "Safe checks passed".into(),
                timestamp: intake::timestamp(),
                summary_dir: None,
                live: false,
            }],
            plugin_origins: vec![PluginRecipeOrigin {
                plugin: "sample-plugin".into(),
                work_item: work.dir.clone(),
                product_id: "one".into(),
                recipe_folder: "recipes".into(),
                icon_folder: "icons".into(),
            }],
            ..Default::default()
        };
        let batch = Batch::from_items("linked", vec![recipe_item]);
        let batch_id = batch.id.clone();
        app.batch.batch = Some(batch);

        sync_plugin_recipe_work(&mut app, &batch_id, 0);

        let synced = WorkItem::open(&work.dir).unwrap();
        assert_eq!(synced.meta.recipe_work["one"].state, RecipeWorkState::Ready, "{}", synced.meta.recipe_work["one"].note);
        assert_eq!(std::fs::read_to_string(work.dir.join(r"recipes\one.yaml")).unwrap().lines().next(), Some("id: one"));
        assert_eq!(std::fs::read(work.dir.join(r"icons\one.png")).unwrap(), b"icon");
    }

    #[test]
    fn removing_an_open_recipe_can_keep_or_close_the_editor_without_losing_other_items() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\nRuntimeRoot = '.'\nRecipesPath = 'Recipes'\nDevelopmentRecipes = 'Recipes\\Development'\n}\n").unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# never executed").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        for id in ["one", "two"] {
            std::fs::write(root.join(format!(r"Recipes\Development\{id}.yaml")), format!("id: {id}\ndisplayName: {id}\n")).unwrap();
        }
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        let mut app = StudioApp::isolated_for_tests(ws);
        let mut batch = Batch::from_items("working", vec![
            BatchItem { app: "One".into(), existing_recipe: "one".into(), ..Default::default() },
            BatchItem { app: "Two".into(), existing_recipe: "two".into(), ..Default::default() },
        ]);
        batch.id = root.join("working-list").to_string_lossy().into_owned();
        app.batch.batch = Some(batch);
        assert!(app.open_recipe_by_id("one"));
        app.view = View::Designer;
        app.recipe.as_mut().unwrap().text.push_str("description: unsaved\n");

        let remove_one = RemoveScope::Selected(vec![0]);
        request_removal(&mut app, remove_one.clone());
        assert!(app.batch.pending_remove.is_some(), "an open dirty recipe must prompt");
        apply_removal(&mut app, &remove_one, true);
        app.batch.pending_remove = None;
        assert_eq!(app.batch.batch.as_ref().unwrap().items.iter().map(item_key).collect::<Vec<_>>(), ["two"]);
        assert!(app.recipe.as_ref().is_some_and(|recipe| recipe.id == "one" && recipe.dirty()));
        assert_eq!(app.view, View::Designer);

        add_existing_recipe(&mut app, "one");
        let one = app.batch.batch.as_ref().unwrap().items.iter().position(|item| item_contains_recipe(item, "one")).unwrap();
        let recipe = app.recipe.as_mut().unwrap();
        recipe.text.clone_from(&recipe.saved);
        apply_removal(&mut app, &RemoveScope::Selected(vec![one]), false);
        assert!(app.recipe.is_none());
        assert_eq!(app.view, View::Batch);
        assert_eq!(app.batch.batch.as_ref().unwrap().items.iter().map(item_key).collect::<Vec<_>>(), ["two"]);

        add_existing_recipe(&mut app, "one");
        assert!(app.open_recipe_by_id("one"));
        app.view = View::Designer;
        request_removal(&mut app, RemoveScope::Clear);
        assert!(app.batch.pending_remove.is_some(), "clearing the list must always ask for confirmation");
        apply_removal(&mut app, &RemoveScope::Clear, false);
        app.batch.pending_remove = None;
        assert!(app.batch.batch.as_ref().unwrap().items.is_empty());
        assert!(app.recipe.is_none());
        assert_eq!(app.view, View::Batch);
    }

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

    fn failed_check(recipe: &str) -> BatchCheck {
        BatchCheck { recipe: recipe.into(), passed: false, note: "Checks failed".into(), timestamp: String::new(), summary_dir: None, live: false }
    }

    fn app_with_list(items: Vec<BatchItem>) -> (tempfile::TempDir, StudioApp) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\nRuntimeRoot = '.'\nRecipesPath = 'Recipes'\nDevelopmentRecipes = 'Recipes\\Development'\n}\n").unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# never executed").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        let mut app = StudioApp::isolated_for_tests(ws);
        let mut working = Batch::from_items("working", items);
        working.id = root.join("working-list").to_string_lossy().into_owned();
        app.batch.batch = Some(working);
        (dir, app)
    }

    #[test]
    fn a_live_pass_from_another_view_clears_a_failed_item_but_not_a_running_one() {
        let failed = BatchItem { app: "Broken".into(), existing_recipe: "broken".into(), recipes: vec!["broken".into()], status: ItemStatus::Failed, checks: vec![failed_check("broken")], ..Default::default() };
        let running = BatchItem { status: ItemStatus::Running, ..failed.clone() };
        let other = BatchItem { app: "Other".into(), existing_recipe: "other".into(), recipes: vec!["other".into()], status: ItemStatus::Failed, checks: vec![failed_check("other")], ..Default::default() };
        let (dir, mut app) = app_with_list(vec![failed, running, other]);
        recipe_validated(&mut app, "BROKEN", "2026-11-18 10:00:00", dir.path());
        let items = &app.batch.batch.as_ref().unwrap().items;
        assert_eq!(items[0].status, ItemStatus::Done);
        assert!(items[0].checks.iter().all(|c| c.passed && c.live), "{:?}", items[0].checks);
        assert_eq!(items[1].status, ItemStatus::Running, "a run in progress owns its own status");
        assert_eq!(items[2].status, ItemStatus::Failed, "other recipes are untouched");
    }

    #[test]
    fn a_check_drops_failures_of_recipes_the_item_no_longer_uses() {
        // The repair agent replaced `old` with `new`; the old failure must not keep the item failed.
        let item = BatchItem { app: "App".into(), recipes: vec!["new".into()], status: ItemStatus::Running, checks: vec![failed_check("old")], ..Default::default() };
        let (_dir, mut app) = app_with_list(vec![item]);
        let id = app.batch.batch.as_ref().unwrap().id.clone();
        let pass = BatchCheck { recipe: "new".into(), passed: true, note: "Validated".into(), timestamp: String::new(), summary_dir: None, live: true };
        check_finished(&mut app, &id, 0, pass);
        let it = &app.batch.batch.as_ref().unwrap().items[0];
        assert_eq!(it.status, ItemStatus::Done);
        assert_eq!(it.checks.len(), 1);
    }
}
