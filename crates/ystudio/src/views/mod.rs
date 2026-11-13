pub mod agent;
pub mod batch;
pub mod designer;
pub mod docs;
pub mod editor;
pub mod home;
pub mod icons;
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
    ui.horizontal(|ui| {
        if theme::primary(ui, true, format!("{}  Recipe library", theme::icon::LIBRARY)).clicked() {
            app.view = crate::app::View::Recipes;
        }
        if ui.add_enabled(app.writable(), egui::Button::new("\u{E710}  New recipe…")).clicked() {
            app.library.new_open = true;
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
        View::Agent => app.agent.target_id = id,
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
    ui.horizontal(|ui| {
        ui.label(RichText::new(&r.id).monospace().size(24.0).strong());
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
            if ui.small_button("\u{E838}").on_hover_text(format!("Open {} in VS Code / the default editor", r.path.display())).clicked() {
                open_external(&r.path);
            }
        });
    });
    let mut goto = None;
    ui.horizontal(|ui| {
        for (view, icon, name) in RECIPE_TABS {
            if ui.selectable_label(app.view == view, format!("{icon}  {name}")).clicked() && app.view != view {
                goto = Some(view);
            }
        }
    });
    ui.separator();
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
