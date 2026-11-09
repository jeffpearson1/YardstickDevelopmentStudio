//! The recipe library: browse, filter, open, create from template, duplicate.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::recipe::{self, RecipeDoc};
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
    pub new_open: bool,
    pub new_template: usize,
    pub new_display: String,
    pub new_publisher: String,
    pub new_id: String,
    pub new_id_touched: bool,
    pub new_vendor: String,
    pub new_notes: String,
    pub duplicate_of: Option<(String, PathBuf)>,
    pub duplicate_id: String,
    /// "Update several recipes": rows get checkboxes, and the picked recipes
    /// become a request.
    pub selecting: bool,
    pub picked: BTreeSet<String>,
    pub update_notes: String,
    /// Templates listed when the New recipe window opened.
    templates: Option<Vec<PathBuf>>,
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    let writable = app.writable();
    ui.horizontal(|ui| {
        theme::page_title(ui, "Recipe library");
        ui.add_space(12.0);
        if ui.add_enabled(writable, egui::Button::new("\u{E710}  New recipe")).clicked() {
            app.library.new_open = true;
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
    let mut open = None;
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
                if ui.selectable_label(is_open, RichText::new(&r.id).monospace().strong()).clicked() {
                    open = Some(r.path.clone());
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
                    if ui.small_button("Open").clicked() {
                        open = Some(r.path.clone());
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
    if let Some(p) = open {
        app.request_open_recipe(p);
        app.view = View::Designer;
    }
    if let Some((id, path)) = duplicate {
        app.library.duplicate_id = id.clone();
        app.library.duplicate_of = Some((id, path));
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
                .on_hover_text("Opens them under Requests, where you can re-test them, repair the failures, or have the agent update them.")
                .clicked();
            if ui.button("Cancel").clicked() {
                lib.selecting = false;
                lib.picked.clear();
            }
        });
    });
    create
}

/// The library's dialogs; drawn every frame so Home can open them too.
pub fn windows(app: &mut StudioApp, ctx: &egui::Context) {
    new_recipe_window(app, ctx);
    duplicate_window(app, ctx);
}

fn new_recipe_window(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.library.new_open {
        return;
    }
    let Some(ws) = app.ws.clone() else { return };
    let templates = app.library.templates.get_or_insert_with(|| ws.templates()).clone();
    let known = |id: &str, dev: bool| {
        app.recipes.iter().any(|r| r.id.eq_ignore_ascii_case(id) && (r.location == RecipeLocation::Development) == dev)
    };
    let can_agent = app.writable() && app.ws_cache.has_skill;
    let agent_name = app.settings.agent_backend.label();
    let mut open = true;
    let mut create = false;
    let mut with_agent = false;
    egui::Window::new("New recipe").open(&mut open).collapsible(false).default_width(500.0).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
        let st = &mut app.library;
        egui::Grid::new("new-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Application");
            if ui.add(egui::TextEdit::singleline(&mut st.new_display).hint_text("e.g. Notepad++").desired_width(320.0)).changed() && !st.new_id_touched {
                st.new_id = recipe::suggest_id(&st.new_display);
            }
            ui.end_row();
            ui.label("Publisher");
            ui.add(egui::TextEdit::singleline(&mut st.new_publisher).hint_text("optional").desired_width(320.0));
            ui.end_row();
            ui.label("Vendor page");
            ui.add(egui::TextEdit::singleline(&mut st.new_vendor).hint_text("optional - where the agent starts its research").desired_width(320.0));
            ui.end_row();
            ui.label("Recipe id");
            if ui.add(egui::TextEdit::singleline(&mut st.new_id).desired_width(320.0)).changed() {
                st.new_id_touched = true;
            }
            ui.end_row();
            ui.label("Notes");
            ui.add(egui::TextEdit::multiline(&mut st.new_notes).hint_text("optional - licensing, architecture, quirks…").desired_rows(2).desired_width(320.0));
            ui.end_row();
        });
        let id = st.new_id.trim().to_string();
        let target = ws.development_recipe_path(&id);
        let valid = recipe::is_valid_id(&id);
        let exists = known(&id, true);
        if !id.is_empty() && !valid {
            ui.colored_label(theme::red(), "Ids are lowercase letters, digits, dots, dashes or underscores.");
        } else if exists {
            ui.colored_label(theme::red(), format!("{} already exists.", target.display()));
        } else if known(&id, false) {
            ui.colored_label(theme::amber(), "A shipped recipe with this id exists - the development copy will take precedence.");
        }
        let ready = valid && !exists && !st.new_display.trim().is_empty();
        ui.add_space(6.0);
        let resp = theme::primary(ui, ready && can_agent, format!("\u{E99A}  Write it with {agent_name}"));
        if !can_agent {
            resp.on_disabled_hover_text("Needs a writable development folder with the yardstick-recipes skill.");
        } else if resp.on_hover_text("The agent researches the vendor and writes the recipe; the Studio then tests and repairs it. Follow it under Requests.").clicked() {
            with_agent = true;
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("or start from a template yourself:");
            let name = |i: usize| templates.get(i).and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "(blank)".into());
            egui::ComboBox::from_id_salt("tpl").selected_text(name(st.new_template)).show_ui(ui, |ui| {
                for i in 0..templates.len() {
                    ui.selectable_value(&mut st.new_template, i, name(i));
                }
                ui.selectable_value(&mut st.new_template, templates.len(), "(blank)");
            });
            if ui.add_enabled(ready, egui::Button::new("Create")).on_hover_text(format!("Creates {} and opens it in the Designer.", target.display())).clicked() {
                create = true;
            }
        });
    });
    if with_agent {
        let st = &app.library;
        let item = ystudio_core::intake::BatchItem {
            app: st.new_display.trim().to_string(),
            publisher: st.new_publisher.trim().to_string(),
            vendor_url: st.new_vendor.trim().to_string(),
            suggested_id: st.new_id.trim().to_string(),
            notes: st.new_notes.trim().to_string(),
            ..Default::default()
        };
        if super::batch::start_new_recipe(app, item) {
            app.library = LibraryState::default();
        }
        return;
    }
    if create {
        let st = &app.library;
        let id = st.new_id.trim().to_string();
        let result = (|| -> anyhow::Result<PathBuf> {
            ws.ensure_writable("create recipes")?;
            let path = ws.development_recipe_path(&id);
            anyhow::ensure!(!path.exists(), "{} already exists.", path.display());
            let doc = match templates.get(st.new_template) {
                Some(t) => recipe::new_from_template(&ystudio_core::read_text(t)?, &id, st.new_display.trim(), st.new_publisher.trim())?,
                None => {
                    let mut d = RecipeDoc::default();
                    d.set_from_text("id", &id);
                    d.set_from_text("displayName", st.new_display.trim());
                    d.set_from_text("publisher", st.new_publisher.trim());
                    d
                }
            };
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, doc.to_yaml())?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                app.library = LibraryState::default();
                app.refresh_recipes();
                app.request_open_recipe(path);
                app.view = View::Designer;
                app.notify(format!("Created {id}. Walk through each step, then test it."));
            }
            Err(e) => app.error(format!("{e:#}")),
        }
        return;
    }
    if !open {
        app.library.new_open = false;
        app.library.templates = None;
    }
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
