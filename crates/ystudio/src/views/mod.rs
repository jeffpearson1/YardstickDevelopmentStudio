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

use eframe::egui::{RichText, Ui};

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
