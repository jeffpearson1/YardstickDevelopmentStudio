//! Raw YAML editor with highlighting and a live problems panel. Keeps comments.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::fields;
use ystudio_core::lint::{self, Severity};

use crate::app::{StudioApp, View};
use crate::theme::FluentText as _;
use crate::{highlight, theme};

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_recipe(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    super::recipe_header(app, ui);

    let font_size = app.settings.editor_font_size;
    let dark = app.settings.dark_mode;
    let mut line_numbers = app.settings.line_numbers;
    let mut jump = None;
    let r = app.recipe.as_mut().unwrap();

    egui::Panel::right("editor-problems").resizable(true).default_size(340.0).show(ui, |ui| {
        let errors = lint::count(&r.diagnostics, Severity::Error);
        let warnings = lint::count(&r.diagnostics, Severity::Warning);
        let hints = lint::count(&r.diagnostics, Severity::Hint);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Problems").semibold());
            theme::badge(ui, errors.to_string(), theme::red());
            theme::badge(ui, warnings.to_string(), theme::amber());
            theme::badge(ui, hints.to_string(), theme::blue());
        });
        ui.label(RichText::new("Click a problem to open its field in the Designer.").small().weak());
        ui.separator();
        egui::ScrollArea::vertical().id_salt("problems").show(ui, |ui| {
            if r.diagnostics.is_empty() {
                ui.colored_label(theme::green(), "\u{E73E}  Lint clean. Run schema validation on the Test Bench to confirm.");
            }
            for d in &r.diagnostics {
                let text = format!(
                    "{} {}{}",
                    theme::severity_icon(d.severity),
                    d.field.as_ref().map(|f| format!("{f}: ")).unwrap_or_default(),
                    d.message
                );
                let resp = ui.add(egui::Label::new(RichText::new(text).color(theme::severity_color(d.severity))).sense(egui::Sense::click()));
                if let Some(section) = d.field.as_deref().and_then(fields::spec_ci).map(|s| s.section)
                    && resp.on_hover_text("Open in the Designer").clicked()
                {
                    jump = Some(section);
                }
                ui.add_space(2.0);
            }
        });
    });

    ui.checkbox(&mut line_numbers, "Line numbers");
    let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, wrap: f32| highlight::yaml_galley(ui, text.as_str(), font_size, dark, wrap);
    egui::ScrollArea::vertical().id_salt("yaml-editor").auto_shrink(false).show(ui, |ui| {
        let lines = highlight::line_count(&r.text);
        let edit = egui::TextEdit::multiline(&mut r.text)
            .code_editor()
            .interactive(!r.read_only)
            .desired_width(f32::INFINITY)
            .desired_rows(40)
            .lock_focus(true)
            .layouter(&mut layouter);
        if highlight::code_edit(ui, edit, line_numbers, lines, font_size).changed() {
            r.changed();
        }
    });

    if let Some(section) = jump {
        if let Some(i) = fields::Section::ALL.iter().position(|s| *s == section) {
            r.section = i;
        }
        app.view = View::Designer;
    }
    if line_numbers != app.settings.line_numbers {
        app.settings.line_numbers = line_numbers;
        app.save_settings();
    }
}
