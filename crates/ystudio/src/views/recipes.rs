//! The recipe library: browse, filter, open, create from template, duplicate.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::recipe;
use ystudio_core::workspace::{RecipeEntry, RecipeLocation};

use crate::app::{StudioApp, View};
use crate::theme::{self, FluentText as _};

#[derive(Default, PartialEq, Clone, Copy)]
pub enum LocationFilter {
    #[default]
    All,
    Development,
    Shipped,
}

#[derive(Default)]
pub struct LibraryState {
    pub filter: String,
    pub location: LocationFilter,
    pub group: Option<String>,
    /// Recipe Builder opened the library to add existing recipes to its list.
    pub adding_to_builder: bool,
    pub duplicate_of: Option<(String, PathBuf)>,
    pub duplicate_id: String,
    /// "Update several recipes": rows get checkboxes, and the picked recipes
    /// become a request.
    pub selecting: bool,
    pub picked: BTreeSet<String>,
    pub update_notes: String,
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    let writable = app.writable();
    ui.horizontal_wrapped(|ui| {
        theme::page_title(ui, "Recipe Library");
        ui.add_space(12.0);
        if ui.add_enabled(writable, egui::Button::new("\u{E710}  New recipe")).clicked() {
            super::batch::show_new_recipe(app);
        }
        if ui
            .add_enabled(writable, egui::Button::selectable(app.library.selecting, "\u{E762}  Update several…"))
            .on_hover_text("Pick recipes to re-test, repair or bring up to date together, as one request.")
            .clicked()
        {
            app.library.selecting = !app.library.selecting;
        }
        if ui.button("\u{E91B}  Missing icons").on_hover_text("Recipes whose iconFile is empty or missing, in the icon finder.").clicked() {
            app.icons.prepare("", "", "", "");
            app.view = View::Icons;
        }
        if ui.button("\u{E72C}  Refresh").on_hover_text("Rescan recipes; also git fetch and sync shared icons/overrides when enabled in Settings").clicked() {
            app.refresh_recipes();
            app.refresh_runs();
            app.refresh_git_status();
            app.refresh_remote(false);
        }
    });
    if app.library.adding_to_builder {
        theme::card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Add recipes to Recipe Builder").semibold());
                ui.label(RichText::new("Click recipes below; added recipes stay highlighted.").weak());
                if theme::primary(ui, true, "Done - return to Recipe Builder").clicked() {
                    app.library.adding_to_builder = false;
                    app.view = View::Batch;
                }
            });
        });
    }
    super::git_bar(app, ui);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.library.filter).hint_text("Filter by id…").desired_width(240.0));
        ui.selectable_value(&mut app.library.location, LocationFilter::All, "All");
        ui.selectable_value(&mut app.library.location, LocationFilter::Development, "Development");
        ui.selectable_value(&mut app.library.location, LocationFilter::Shipped, "Shipped");
        let groups = &app.ws_cache.groups;
        if !groups.is_empty() {
            egui::ComboBox::from_id_salt("group-filter")
                .selected_text(app.library.group.clone().unwrap_or_else(|| "Any group".into()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut app.library.group, None, "Any group");
                    for g in groups {
                        ui.selectable_value(&mut app.library.group, Some(g.clone()), g);
                    }
                });
        }
    });
    ui.separator();

    let needle = app.library.filter.trim().to_lowercase();
    let rows: Vec<&RecipeEntry> = app
        .recipes
        .iter()
        .filter(|r| needle.is_empty() || r.id.to_lowercase().contains(&needle))
        .filter(|r| match app.library.location {
            LocationFilter::All => true,
            LocationFilter::Development => r.location == RecipeLocation::Development,
            LocationFilter::Shipped => r.location != RecipeLocation::Development,
        })
        .filter(|r| app.library.group.as_ref().is_none_or(|g| r.groups.contains(g)))
        .collect();
    ui.label(RichText::new(format!("{} of {} recipes", rows.len(), app.recipes.len())).weak());
    let make_request = app.library.selecting && selection_bar(&mut app.library, ui, &rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>());

    let selecting = app.library.selecting;
    let mut toggle = None;
    let open_path = app.recipe.as_ref().map(|o| o.path.as_path());
    let mut add_to_builder = None;
    let mut duplicate = None;
    let row_h = 26.0;
    egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, rows.len(), |ui, range| {
        for r in &rows[range] {
            ui.horizontal(|ui| {
                ui.set_height(row_h);
                if selecting {
                    let mut on = app.library.picked.contains(&r.id);
                    if ui.checkbox(&mut on, "").changed() {
                        toggle = Some((r.id.clone(), on));
                    }
                }
                // Both paths come from the same directory listing, so a plain comparison suffices.
                let is_open = open_path == Some(r.path.as_path());
                let in_builder = super::batch::contains_recipe(app, &r.id);
                if ui.selectable_label(is_open || in_builder, RichText::new(&r.id).monospace().strong()).clicked() {
                    if selecting {
                        toggle = Some((r.id.clone(), !app.library.picked.contains(&r.id)));
                    } else {
                        add_to_builder = Some(r.id.clone());
                    }
                }
                match &r.location {
                    RecipeLocation::Development => theme::badge(ui, "development", theme::amber()),
                    RecipeLocation::Shipped(None) => theme::badge(ui, "shipped", theme::grey()),
                    RecipeLocation::Shipped(Some(sub)) => theme::badge(ui, format!("shipped/{sub}"), theme::grey()),
                };
                for g in &r.groups {
                    theme::badge(ui, g, theme::blue());
                }
                super::recipe_status_badge(app, ui, &r.id);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if r.location != RecipeLocation::Development
                        && ui.add_enabled(writable, egui::Button::new("\u{E8C8}  Copy to Development").small()).on_hover_text(
                            "Development recipes take precedence and are Git-ignored - the safe place to change a shipped recipe.",
                        ).clicked()
                    {
                        duplicate = Some((r.id.clone(), r.path.clone()));
                    }
                    if app.library.adding_to_builder {
                        if in_builder {
                            theme::badge(ui, "added", theme::green());
                        } else if ui.small_button("Add").clicked() {
                            add_to_builder = Some(r.id.clone());
                        }
                    } else if ui.small_button("Open in Builder").clicked() {
                        add_to_builder = Some(r.id.clone());
                    }
                });
            });
        }
    });

    if make_request {
        let ids: Vec<String> = app.library.picked.iter().cloned().collect();
        let notes = app.library.update_notes.trim().to_string();
        if super::batch::start_update_request(app, &ids, &notes) {
            app.library.selecting = false;
            app.library.picked.clear();
            app.library.update_notes.clear();
        }
    }
    if let Some((id, on)) = toggle {
        if on {
            app.library.picked.insert(id);
        } else {
            app.library.picked.remove(&id);
        }
    }
    if let Some(id) = add_to_builder {
        open_in_builder(app, &id);
    }
    if let Some((id, path)) = duplicate {
        app.library.duplicate_id = id.clone();
        app.library.duplicate_of = Some((id, path));
    }
}

fn open_in_builder(app: &mut StudioApp, id: &str) {
    super::batch::add_existing_recipe(app, id);
    app.library.adding_to_builder = false;
    if app.open_recipe_by_id(id) {
        app.view = View::Designer;
    } else {
        app.view = View::Batch;
    }
}

/// What to do with the picked recipes in "Update several". Returns true when
/// the user asks for a request.
fn selection_bar(lib: &mut LibraryState, ui: &mut Ui, shown: &[String]) -> bool {
    let n = lib.picked.len();
    let mut create = false;
    theme::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{n} selected")).semibold());
            if ui.small_button(format!("Select the {} shown", shown.len())).clicked() {
                lib.picked.extend(shown.iter().cloned());
            }
            if ui.small_button("Clear").clicked() {
                lib.picked.clear();
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut lib.update_notes)
                .hint_text("What should change? e.g. \"vendor moved downloads to GitHub releases\" (optional)")
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            create = theme::primary(ui, n > 0, format!("\u{E8C3}  Make a request for {n} recipe(s)"))
                .on_hover_text("Opens them in Recipe Builder, where you can re-test them, repair the failures, or have the agent update them.")
                .clicked();
            if ui.button("Cancel").clicked() {
                lib.selecting = false;
                lib.picked.clear();
            }
        });
    });
    create
}

/// The library's dialogs.
pub fn windows(app: &mut StudioApp, ctx: &egui::Context) {
    duplicate_window(app, ctx);
}

fn duplicate_window(app: &mut StudioApp, ctx: &egui::Context) {
    let Some((src_id, src)) = app.library.duplicate_of.clone() else { return };
    let Some(ws) = app.ws.clone() else { return };
    let mut open = true;
    let mut go = false;
    egui::Window::new("Copy to Development").open(&mut open).collapsible(false).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
        ui.label(format!("Copy '{src_id}' into Recipes\\Development. Comments and formatting are kept."));
        ui.horizontal(|ui| {
            ui.label("New id");
            ui.text_edit_singleline(&mut app.library.duplicate_id);
        });
        ui.label(RichText::new("Keep the same id to override the shipped recipe while you work on it.").small().weak());
        let id = app.library.duplicate_id.trim();
        let taken = app.recipes.iter().any(|r| r.location == RecipeLocation::Development && r.id.eq_ignore_ascii_case(id));
        let ok = recipe::is_valid_id(id) && !taken;
        if taken {
            ui.colored_label(theme::red(), "A development recipe with that id already exists.");
        }
        if ui.add_enabled(ok, egui::Button::new("Copy")).clicked() {
            go = true;
        }
    });
    if go {
        let id = app.library.duplicate_id.trim().to_string();
        let result = ws.copy_to_development(&src, &src_id, &id);
        app.library.duplicate_of = None;
        match result {
            Ok(dest) => {
                app.refresh_recipes();
                super::batch::add_existing_recipe(app, &id);
                app.request_open_recipe(dest);
                app.view = View::Designer;
                app.notify(format!("Copied {src_id} → Recipes\\Development\\{id}.yaml"));
            }
            Err(e) => app.error(format!("{e:#}")),
        }
    } else if !open {
        app.library.duplicate_of = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::fake_app;

    #[test]
    fn library_recipe_click_routes_to_the_unified_builder() {
        let (_dir, mut app) = fake_app();
        app.library.adding_to_builder = true;
        app.view = View::Recipes;
        open_in_builder(&mut app, "app");
        assert_eq!(app.view, View::Designer);
        assert!(!app.library.adding_to_builder);
        assert!(super::super::batch::contains_recipe(&app, "app"));
        assert_eq!(app.recipe.as_ref().map(|r| r.id.as_str()), Some("app"));
    }
}
