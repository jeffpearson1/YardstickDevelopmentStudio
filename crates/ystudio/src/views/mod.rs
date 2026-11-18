pub mod agent;
pub mod batch;
pub mod designer;
pub mod docs;
pub mod editor;
pub mod home;
pub mod icons;
pub mod plugin_page;
pub mod plugins;
pub mod promote;
pub mod recipes;
pub mod runs;
pub mod settings;
pub mod setup;
pub mod testbench;
pub mod tools;
pub mod update;
pub mod usage;

use eframe::egui::{self, RichText, Ui};

use ystudio_core::workspace::RecipeLocation;

use crate::app::{StudioApp, View};
use crate::theme::{self, FluentText as _};

/// Shows a placeholder when no workspace is open. Returns true if one is.
pub fn need_workspace(app: &mut StudioApp, ui: &mut Ui) -> bool {
    if app.ws.is_some() {
        return true;
    }
    ui.add_space(24.0);
    ui.label(RichText::new("No Yardstick folder is open.").heading());
    ui.label(RichText::new("Open or clone a workspace on the Home page.").weak());
    ui.add_space(8.0);
    if theme::primary(ui, true, "Go to Home").clicked() {
        app.view = crate::app::View::Home;
    }
    false
}

/// Shows a placeholder when no recipe is open. Returns true if one is.
pub fn need_recipe(app: &mut StudioApp, ui: &mut Ui) -> bool {
    if !need_workspace(app, ui) {
        return false;
    }
    if app.recipe.is_some() {
        return true;
    }
    ui.add_space(24.0);
    ui.label(RichText::new("No recipe is open.").heading());
    ui.label(RichText::new("Pick one in the library, or start a new one - written by the agent or from a template.").weak());
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if theme::primary(ui, true, format!("{}  Recipe Library", theme::icon::LIBRARY)).clicked() {
            app.view = crate::app::View::Recipes;
        }
        if ui.add_enabled(app.writable(), egui::Button::new("\u{E710}  New recipe…")).clicked() {
            batch::show_new_recipe(app);
        }
    });
    false
}

/// Shows how far a recipe has got: complete (the tested contents, not yet
/// published), changed since it was validated, or its latest test verdict.
/// Returns false when there is nothing to show.
pub fn recipe_status_badge(app: &StudioApp, ui: &mut Ui, id: &str) -> bool {
    use ystudio_core::validation::Completion;
    let development = app.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(id))
        .is_some_and(|r| r.location == ystudio_core::workspace::RecipeLocation::Development);
    match app.completion(id) {
        Completion::Complete(r) => {
            let text = if development { "Complete · not published" } else { "Validated" };
            let warn = if r.warnings { " with warnings" } else { "" };
            theme::badge(ui, text, theme::green()).on_hover_text(format!("Passed its live install test{warn} on {}, and is unchanged since.", r.tested));
        }
        Completion::Changed(r) => {
            theme::badge(ui, "Changed since validation", theme::amber())
                .on_hover_text(format!("Passed its live install test on {}, but has been edited since. Run the live test again.", r.tested));
        }
        Completion::NotValidated => {
            let Some((v, ts)) = app.verdicts.get(id) else { return false };
            theme::badge(ui, v.label(), theme::verdict_color(*v)).on_hover_text(format!("Install test {ts}"));
        }
    }
    true
}

/// The pages of the open recipe, shown as tabs under its header.
pub const RECIPE_TABS: [(View, &str, &str); 5] = [
    (View::Designer, theme::icon::EDIT, "Design"),
    (View::Editor, theme::icon::CODE, "YAML"),
    (View::TestBench, theme::icon::CHECKLIST, "Test"),
    (View::Icons, theme::icon::PHOTO, "Icon"),
    (View::Agent, theme::icon::ROBOT, "Agent"),
];

/// Shows one of the open recipe's pages, pointing the icon finder and the
/// agent at it so every tab works on the same app.
pub fn open_recipe_tab(app: &mut StudioApp, view: View) {
    let Some(id) = app.recipe.as_ref().map(|r| r.id.clone()) else { return };
    match view {
        View::Icons if app.icons.recipe_id.as_deref() != Some(id.as_str()) || app.icons.batch_item.is_some() => {
            icons::prepare_for_recipe(app, &id);
            // Search the web only when the recipe has no usable icon yet.
            let file = app.recipes.iter().find(|r| r.id == id).map(|r| r.meta.icon_file.clone()).unwrap_or_default();
            app.icons.auto_search = file.is_empty() || !app.ws_cache.icons.iter().any(|i| i.eq_ignore_ascii_case(&file));
        }
        View::Agent => app.agent.focus(&id),
        _ => {}
    }
    app.view = view;
}

/// The open recipe's id, location, status, Save/Revert, and its page tabs.
pub fn recipe_header(app: &mut StudioApp, ui: &mut Ui) {
    let Some(r) = &app.recipe else { return };
    let id = r.id.clone();
    let mut save = false;
    let mut revert = false;
    let mut rename = false;
    let can_rename = r.location == RecipeLocation::Development && !r.read_only;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&r.id).monospace().size(24.0).strong());
        if can_rename
            && ui
                .add(egui::Button::new("\u{E8AC}").frame(false))
                .on_hover_text("Rename this recipe: its file name and id change together.")
                .clicked()
        {
            rename = true;
        }
        match &r.location {
            RecipeLocation::Development => theme::badge(ui, "development", theme::amber()),
            RecipeLocation::Shipped(Some(group)) => theme::badge(ui, format!("shipped / {group}"), theme::grey()),
            RecipeLocation::Shipped(None) => theme::badge(ui, "shipped", theme::grey()),
        };
        if r.dirty() {
            theme::badge(ui, "unsaved", theme::amber());
        }
        if r.read_only {
            theme::badge(ui, "read-only", theme::red());
        }
        recipe_status_badge(app, ui, &id);
        let r = app.recipe.as_ref().unwrap();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary(ui, r.dirty() && !r.read_only, "\u{E74E}  Save  (Ctrl+S)").clicked() {
                save = true;
            }
            if ui.add_enabled(r.dirty(), egui::Button::new("\u{E7A7}  Revert")).clicked() {
                revert = true;
            }
            if ui.button("\u{E838}").on_hover_text(format!("Open {} in VS Code / the default editor", r.path.display())).clicked() {
                open_external(&r.path);
            }
        });
    });
    let mut goto = None;
    ui.horizontal_wrapped(|ui| {
        for (view, icon, name) in RECIPE_TABS {
            if ui.selectable_label(app.view == view, format!("{icon}  {name}")).clicked() && app.view != view {
                goto = Some(view);
            }
        }
    });
    ui.separator();
    if rename {
        open_rename(app);
    }
    if save {
        app.save_recipe();
    }
    if revert && let Some(r) = &mut app.recipe {
        r.text = r.saved.clone();
        r.changed();
    }
    if let Some(view) = goto {
        open_recipe_tab(app, view);
    }
}

/// The Rename dialog's state for the open recipe.
#[derive(Debug, Clone, PartialEq)]
pub struct RenameDraft {
    pub new_id: String,
    pub rename_icon: bool,
    pub error: Option<String>,
}

/// Opens the Rename dialog for the open recipe. It starts from the YAML's id
/// when that no longer matches the file, so it doubles as the fix for that lint error.
pub fn open_rename(app: &mut StudioApp) {
    let Some(r) = &mut app.recipe else { return };
    let yaml_id = ystudio_core::recipe::RecipeDoc::parse(&r.text).ok().and_then(|d| d.get_str("id").map(|s| s.trim().to_string()));
    let new_id = yaml_id.filter(|y| *y != r.id && ystudio_core::recipe::is_valid_id(y)).unwrap_or_else(|| r.id.clone());
    r.rename = Some(RenameDraft { new_id, rename_icon: true, error: None });
}

/// Renames the open development recipe (file name and id together) from its
/// header, carrying unsaved edits, the icon when it is named after the recipe,
/// and the Studio's own references to the id.
pub fn rename_modal(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(r) = &app.recipe else { return };
    let Some(draft) = r.rename.clone() else { return };
    let old_id = r.id.clone();
    let dirty = r.dirty();
    let icon = ystudio_core::workspace::renamable_icon(&r.text, &old_id);
    let complete = app.completion(&old_id).is_complete();
    let mut draft_out = draft.clone();
    let mut action = None;
    egui::Modal::new(egui::Id::unique("rename-recipe")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Rename recipe");
        ui.label(RichText::new("The file name and the recipe id must match, so both change.").weak());
        ui.add_space(8.0);
        ui.label("New id");
        let edit = ui.add(egui::TextEdit::singleline(&mut draft_out.new_id).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
        if edit.changed() {
            draft_out.error = None;
        }
        let new_id = draft_out.new_id.trim().to_string();
        let valid = ystudio_core::recipe::is_valid_id(&new_id);
        if !new_id.is_empty() && !valid {
            ui.colored_label(theme::red(), "Use lowercase letters, digits, '.', '-' or '_'.");
        } else if valid {
            ui.label(RichText::new(format!("Recipes\\Development\\{old_id}.yaml  →  {new_id}.yaml")).small().monospace().weak());
        }
        if let Some(icon) = &icon {
            let ext = std::path::Path::new(icon).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
            ui.checkbox(&mut draft_out.rename_icon, format!("Rename the icon too: Icons\\{icon} → {new_id}.{ext}"));
        }
        if dirty {
            ui.label(RichText::new("Your unsaved changes are saved with the rename.").small());
        }
        if complete {
            ui.label(RichText::new("The recipe will no longer be complete: its contents change, so publishing needs a fresh live test.").small().color(theme::amber()));
        }
        if let Some(e) = &draft_out.error {
            ui.colored_label(theme::red(), e);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (theme::primary(ui, valid && new_id != old_id, "\u{E8AC}  Rename").clicked() || enter) && valid && new_id != old_id {
                action = Some(true);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                action = Some(false);
            }
        });
    });
    match action {
        Some(false) => {
            if let Some(r) = &mut app.recipe {
                r.rename = None;
            }
        }
        Some(true) => match rename_open_recipe(app, &draft_out.new_id, draft_out.rename_icon) {
            Ok(()) => {}
            Err(e) => {
                draft_out.error = Some(e);
                if let Some(r) = &mut app.recipe {
                    r.rename = Some(draft_out);
                }
            }
        },
        None => {
            if let Some(r) = &mut app.recipe {
                r.rename = Some(draft_out);
            }
        }
    }
}

fn rename_open_recipe(app: &mut StudioApp, new_id: &str, rename_icon: bool) -> Result<(), String> {
    if crate::workflow::busy(app) || !app.batch.queue.is_empty() {
        return Err("Wait for the running agent or checks to finish before renaming.".into());
    }
    let Some(r) = &app.recipe else { return Err("No recipe is open.".into()) };
    let (old_id, path, text) = (r.id.clone(), r.path.clone(), r.text.clone());
    if batch::linked_to_plugin(app, &old_id) {
        return Err(format!("A plugin work item is linked to {old_id} by id; finish or return that work before renaming."));
    }
    let ws = app.ws.as_ref().ok_or("Open a workspace first.")?;
    let renamed = ws.rename_development_recipe(&path, &text, new_id, rename_icon).map_err(|e| format!("{e:#}"))?;
    let new_id = new_id.trim().to_string();
    batch::recipe_renamed(app, &old_id, &new_id);
    app.agent.rename(&old_id, &new_id);
    if app.icons.recipe_id.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(&old_id)) {
        app.icons.recipe_id = Some(new_id.clone());
    }
    app.refresh_recipes();
    let view = app.view;
    app.open_recipe(&renamed.path);
    app.view = view;
    let icon = renamed.icon.map(|(from, to)| format!("; icon {from} → {to}")).unwrap_or_default();
    app.notify(format!("Renamed {old_id} to {new_id}{icon}."));
    Ok(())
}

/// Banner explaining why actions are disabled in a production folder.
pub fn production_banner(app: &StudioApp, ui: &mut Ui) {
    if let Some(ws) = &app.ws
        && let ystudio_core::workspace::WorkspaceKind::Production { reason } = &ws.kind
    {
        theme::banner(ui, theme::red(), |ui| {
            ui.label(RichText::new("\u{E72E}  Production folder - read-only").semibold().color(theme::red()));
            ui.label(format!(
                "The Studio never writes to or runs the harness in a folder that deploys to your tenant ({reason}). \
                 Create a development clone on the Home page, then promote finished recipes with Publish-Yardstick."
            ));
        });
        ui.add_space(6.0);
    }
}

/// Branch, ahead/behind, and Fetch / Pull / Sync buttons for the open workspace.
pub fn git_bar(app: &mut StudioApp, ui: &mut Ui) {
    use crate::app::JobKind;
    let Some(ws) = &app.ws else { return };
    let writable = ws.is_writable();
    let sync_on = ws.sync_shared_content;
    let running = |k: fn(&JobKind) -> bool| app.jobs.iter().any(|j| k(&j.kind) && j.proc.is_running());
    let fetching = running(|k| matches!(k, JobKind::GitFetch));
    let pulling = running(|k| matches!(k, JobKind::GitPull));
    let syncing = running(|k| matches!(k, JobKind::SyncShared));
    let (mut fetch, mut pull, mut sync) = (false, false, false);
    ui.horizontal_wrapped(|ui| {
        match &app.git_status {
            None => {
                ui.label(RichText::new("Not a git clone").small().weak());
            }
            Some(Err(e)) => {
                ui.label(RichText::new("\u{E7BA}  git unavailable").small().color(theme::amber())).on_hover_text(e);
            }
            Some(Ok(s)) => {
                let up = s.upstream.as_deref().unwrap_or("no upstream");
                ui.label(RichText::new(format!("{} → {up}", s.branch)).small().monospace());
                if s.upstream.is_some() {
                    if s.behind > 0 {
                        theme::badge(ui, format!("{} behind", s.behind), theme::amber());
                    }
                    if s.ahead > 0 {
                        theme::badge(ui, format!("{} ahead", s.ahead), theme::blue());
                    }
                    if s.behind == 0 && s.ahead == 0 {
                        ui.label(RichText::new("up to date as of last fetch").small().weak());
                    }
                }
                if writable {
                    if ui.add_enabled(!fetching && !pulling, egui::Button::new("\u{E895}  Fetch").small()).on_hover_text("git fetch --prune").clicked() {
                        fetch = true;
                    }
                    if s.behind > 0
                        && ui
                            .add_enabled(!fetching && !pulling, egui::Button::new(format!("\u{E896}  Pull {}", s.behind)).small())
                            .on_hover_text("git pull --ff-only. Recipes\\Development is git-ignored, so your work there is untouched.")
                            .clicked()
                    {
                        pull = true;
                    }
                }
                if fetching || pulling {
                    ui.spinner();
                }
            }
        }
        if writable && sync_on {
            ui.separator();
            if ui
                .add_enabled(!syncing, egui::Button::new("\u{E895}  Sync icons & overrides").small())
                .on_hover_text("Yardstick's shared-content sync (SharedIcons / SharedRecipeOverrides in preferences.yaml)")
                .clicked()
            {
                sync = true;
            }
            if syncing {
                ui.spinner();
            }
        }
    });
    if fetch {
        app.start_git_fetch(true);
    }
    if pull {
        app.start_git_pull();
    }
    if sync {
        app.start_shared_sync(true);
    }
}

/// Recipes among `ids` with `manualDownload: true` cannot be tested without a
/// software dropbox: say so before an agent or a test run finds out the hard way.
pub fn dropbox_banner(app: &mut StudioApp, ui: &mut Ui, ids: &[String]) {
    let Some(ws) = app.ws.as_ref().filter(|w| w.is_writable() && w.software_dropbox.is_none()) else { return };
    let manual: Vec<&str> = ids
        .iter()
        .filter_map(|id| app.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(id) && r.meta.manual_download))
        .map(|r| r.id.as_str())
        .collect();
    if manual.is_empty() {
        return;
    }
    let prefs = ws.layout.preferences.clone();
    let production = app.ws_cache.production_dropbox.clone();
    let mut copy = None;
    theme::banner(ui, theme::amber(), |ui| {
        ui.label(RichText::new("No software dropbox in this workspace").semibold().color(theme::amber()));
        ui.label(format!(
            "{} use{} manualDownload, so the media comes from the software dropbox, but SoftwareDropbox is not set in {}. \
             Tests stop before the download step and agents can only report that.",
            manual.join(", "),
            if manual.len() == 1 { "s" } else { "" },
            prefs.display()
        ));
        if let Some(p) = &production {
            ui.label(RichText::new(format!("Production uses {}.", p.display())).small());
        }
        ui.horizontal(|ui| {
            if ui.button("\u{E8E5}  Open preferences.yaml").clicked() {
                open_in_explorer(&prefs);
            }
            if let Some(p) = &production
                && ui.button("\u{E8C8}  Copy production's path").clicked()
            {
                copy = Some(format!("SoftwareDropbox: '{}'", p.display()));
            }
        });
    });
    if let Some(text) = copy {
        ui.ctx().copy_text(text);
        app.notify("Copied. Paste it over the SoftwareDropbox line in preferences.yaml, save, then Refresh.");
    }
    ui.add_space(6.0);
}

pub fn open_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// Opens a file in VS Code if available, otherwise with the default handler.
pub fn open_external(path: &std::path::Path) {
    if let Some(code) = ystudio_core::process::which("code") {
        let _ = std::process::Command::new(code).arg(path).spawn();
    } else {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::{fake_app, render};
    use ystudio_core::intake::{Batch, BatchItem};

    #[test]
    fn renaming_the_open_recipe_moves_its_file_icon_and_references() {
        let (dir, mut app) = fake_app();
        let dev = dir.path().join("Recipes").join("Development");
        let mut list = Batch::from_items("working", vec![BatchItem { app: "App".into(), existing_recipe: "app".into(), recipes: vec!["app".into()], ..Default::default() }]);
        list.id = dir.path().join("working-list").to_string_lossy().into_owned();
        app.batch.batch = Some(list);
        assert!(app.open_recipe_by_id("app"));
        app.agent.focus("app");
        app.agent.draft.notes = "keep me".into();
        // An unsaved edit travels with the rename.
        let r = app.recipe.as_mut().unwrap();
        r.text = r.text.replace("displayName: App", "displayName: App Two");
        r.changed();
        crate::views::open_recipe_tab(&mut app, View::Designer);
        app.recipe.as_mut().unwrap().rename = Some(RenameDraft { new_id: "app-two".into(), rename_icon: true, error: None });
        render(&mut app, "rename-dialog", 1200, 760);

        rename_open_recipe(&mut app, "app-two", true).unwrap();
        assert!(!dev.join("app.yaml").exists());
        let text = std::fs::read_to_string(dev.join("app-two.yaml")).unwrap();
        assert!(text.contains("id: app-two") && text.contains("iconFile: app-two.png") && text.contains("displayName: App Two"), "{text}");
        assert!(dir.path().join("Icons").join("app-two.png").is_file());
        let r = app.recipe.as_ref().unwrap();
        assert_eq!((r.id.as_str(), r.dirty()), ("app-two", false));
        let item = &app.batch.batch.as_ref().unwrap().items[0];
        assert_eq!((item.existing_recipe.as_str(), item.recipes.as_slice()), ("app-two", ["app-two".to_string()].as_slice()));
        app.agent.focus("app-two");
        assert_eq!(app.agent.draft.notes, "keep me");
        // A taken id is refused with a message, and nothing moves.
        assert!(rename_open_recipe(&mut app, "broken", true).unwrap_err().contains("already exists"));
        assert!(dev.join("app-two.yaml").is_file());
    }
}
