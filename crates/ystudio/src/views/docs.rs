//! Docs: the recipe skill, its references and Yardstick's own documentation.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkViewer;

use crate::app::StudioApp;
use crate::theme;

#[derive(Default)]
pub struct DocsState {
    selected: Option<PathBuf>,
    text: String,
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    let docs = &app.ws_cache.documents;
    if app.docs.selected.is_none()
        && let Some((_, p)) = docs.first()
    {
        app.docs.text = ystudio_core::read_text(p).unwrap_or_default();
        app.docs.selected = Some(p.clone());
    }
    egui::Panel::left("docs-list").resizable(true).default_size(240.0).show(ui, |ui| {
        theme::page_title(ui, "Docs");
        ui.separator();
        for (label, path) in docs {
            let selected = app.docs.selected.as_ref() == Some(path);
            if ui.selectable_label(selected, label).clicked() {
                app.docs.text = ystudio_core::read_text(path).unwrap_or_else(|e| format!("Could not read: {e}"));
                app.docs.selected = Some(path.clone());
            }
        }
        if docs.is_empty() {
            ui.weak("No documents found in this folder.");
        }
    });
    if let Some(p) = &app.docs.selected {
        ui.horizontal(|ui| {
            ui.label(RichText::new(p.display().to_string()).small().monospace().weak());
            if ui.small_button("\u{E838}").on_hover_text("Open externally").clicked() {
                super::open_external(p);
            }
        });
        ui.separator();
    }
    egui::ScrollArea::vertical().id_salt(("doc", app.docs.selected.clone())).auto_shrink(false).show(ui, |ui| {
        CommonMarkViewer::new().show(ui, &mut app.md_cache, &app.docs.text);
    });
}
