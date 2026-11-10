//! Agent credits by model: what an app costs on average with each model, as
//! bars, with the exact numbers in a table underneath.

use eframe::egui::{self, CornerRadius, RichText, Sense, Stroke, Ui, vec2};
use ystudio_core::usage::{self, ModelUsage};

use crate::app::StudioApp;
use crate::theme::{self, FluentText as _};

const LABEL_WIDTH: f32 = 200.0;
const VALUE_WIDTH: f32 = 70.0;
const BAR_HEIGHT: f32 = 14.0;
const ROW_HEIGHT: f32 = 24.0;

pub fn card(app: &StudioApp, ui: &mut Ui) {
    let models = usage::by_model(&app.usage);
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Average AI credits per app");
        ui.label(
            RichText::new(
                "Every agent run on an app counted together - writing, fixes and repairs - by the model the run used. \
                 Copilot CLI reports credits; Claude Code runs are not counted.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        if models.is_empty() {
            ui.label(RichText::new("No agent runs recorded yet. Credits appear here after the first Copilot CLI run.").weak());
            return;
        }
        bars(ui, &models);
        ui.add_space(8.0);
        table(ui, &models);
    });
}

/// One bar per model, sorted by model name, on a shared zero baseline.
fn bars(ui: &mut Ui, models: &[ModelUsage]) {
    let p = theme::palette();
    let max = models.iter().filter_map(ModelUsage::per_app).fold(0.0f64, f64::max);
    let width = (ui.available_width() - LABEL_WIDTH - VALUE_WIDTH).max(80.0);
    for m in models {
        let (row, resp) = ui.allocate_exact_size(vec2(LABEL_WIDTH + width + VALUE_WIDTH, ROW_HEIGHT), Sense::hover());
        let painter = ui.painter_at(row);
        painter.text(
            egui::pos2(row.left(), row.center().y),
            egui::Align2::LEFT_CENTER,
            &m.model,
            egui::FontId::monospace(12.0),
            p.text,
        );
        let x0 = row.left() + LABEL_WIDTH;
        // Recessive baseline at zero.
        painter.line_segment([egui::pos2(x0, row.top() + 3.0), egui::pos2(x0, row.bottom() - 3.0)], Stroke::new(1.0, p.divider));
        let value = match m.per_app() {
            Some(v) => {
                let len = if max > 0.0 { (v / max) as f32 * width } else { 0.0 };
                if len > 0.5 {
                    let bar = egui::Rect::from_min_size(egui::pos2(x0, row.center().y - BAR_HEIGHT / 2.0), vec2(len, BAR_HEIGHT));
                    // Square at the baseline, rounded at the data end.
                    painter.rect_filled(bar, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, p.accent);
                }
                painter.text(
                    egui::pos2(x0 + len + 6.0, row.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("{v:.1}"),
                    egui::FontId::proportional(12.0),
                    p.text,
                );
                Some(v)
            }
            None => {
                painter.text(egui::pos2(x0 + 6.0, row.center().y), egui::Align2::LEFT_CENTER, "no app runs", egui::FontId::proportional(12.0), p.text_secondary);
                None
            }
        };
        resp.on_hover_ui(|ui| {
            ui.label(RichText::new(&m.model).monospace().strong());
            if let Some(v) = value {
                ui.label(format!("{v:.1} AI credits per app, over {} app(s)", m.apps));
            }
            ui.label(format!("{:.1} per run, over {} run(s)", m.per_run(), m.runs));
            ui.label(format!("{:.1} credits in total", m.credits));
        });
    }
}

fn table(ui: &mut Ui, models: &[ModelUsage]) {
    egui::CollapsingHeader::new("Numbers").id_salt("usage-table").show(ui, |ui| {
        egui::Grid::new("usage-grid").striped(true).num_columns(6).spacing([16.0, 4.0]).show(ui, |ui| {
            for h in ["Model", "Runs", "Per run", "Apps", "Per app", "Total"] {
                ui.label(RichText::new(h).semibold());
            }
            ui.end_row();
            for m in models {
                ui.label(RichText::new(&m.model).monospace());
                ui.label(m.runs.to_string());
                ui.label(format!("{:.1}", m.per_run()));
                ui.label(m.apps.to_string());
                ui.label(m.per_app().map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()));
                ui.label(format!("{:.1}", m.credits));
                ui.end_row();
            }
        });
    });
}
