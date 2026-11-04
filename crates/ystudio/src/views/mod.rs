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
pub mod testbench;
pub mod update;

use eframe::egui::{self, RichText, Ui};

use crate::app::StudioApp;
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
    ui.label(RichText::new("Pick one in the library, create one from a template, or ask the agent to write one.").weak());
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if theme::primary(ui, true, format!("{}  Recipe library", theme::icon::LIBRARY)).clicked() {
            app.view = crate::app::View::Recipes;
        }
        if ui.button(format!("{}  Agent", theme::icon::ROBOT)).clicked() {
            app.view = crate::app::View::Agent;
        }
    });
    false
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
