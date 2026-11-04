//! The recipe library: browse, filter, open, create from template, duplicate.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::recipe::{self, RecipeDoc};
use ystudio_core::workspace::{RecipeEntry, RecipeLocation};

use crate::app::{StudioApp, View};
use crate::theme;

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
    pub duplicate_of: Option<(String, PathBuf)>,
    pub duplicate_id: String,
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

    let open_path = app.recipe.as_ref().map(|o| o.path.as_path());
    let mut open = None;
    let mut duplicate = None;
    let row_h = 26.0;
    egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, rows.len(), |ui, range| {
        for r in &rows[range] {
            ui.horizontal(|ui| {
                ui.set_height(row_h);
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
                if let Some((v, ts)) = app.verdicts.get(&r.id) {
                    theme::badge(ui, v.label(), theme::verdict_color(*v)).on_hover_text(format!("Install test {ts}"));
                }
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

    if let Some(p) = open {
        app.request_open_recipe(p);
        app.view = View::Designer;
    }
    if let Some((id, path)) = duplicate {
        app.library.duplicate_id = id.clone();
        app.library.duplicate_of = Some((id, path));
    }
    new_recipe_window(app, ui.ctx());
    duplicate_window(app, ui.ctx());
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
    let mut open = true;
    let mut create = false;
    egui::Window::new("New recipe").open(&mut open).collapsible(false).default_width(460.0).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
        let st = &mut app.library;
        ui.label("Start from a template. You'll finish it in the Designer.");
        egui::Grid::new("new-grid").num_columns(2).show(ui, |ui| {
            ui.label("Template");
            let name = |i: usize| templates.get(i).and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "(blank)".into());
            egui::ComboBox::from_id_salt("tpl").selected_text(name(st.new_template)).show_ui(ui, |ui| {
                for i in 0..templates.len() {
                    ui.selectable_value(&mut st.new_template, i, name(i));
                }
                ui.selectable_value(&mut st.new_template, templates.len(), "(blank)");
            });
            ui.end_row();
            ui.label("Display name");
            if ui.text_edit_singleline(&mut st.new_display).changed() && !st.new_id_touched {
                st.new_id = recipe::suggest_id(&st.new_display);
            }
            ui.end_row();
            ui.label("Publisher");
            ui.text_edit_singleline(&mut st.new_publisher);
            ui.end_row();
            ui.label("Recipe id");
            if ui.text_edit_singleline(&mut st.new_id).changed() {
                st.new_id_touched = true;
            }
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
        ui.label(RichText::new(format!("Creates {}", target.display())).small().weak());
        if ui.add_enabled(valid && !exists && !st.new_display.trim().is_empty(), egui::Button::new("Create")).clicked() {
            create = true;
        }
    });
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
        let dest = ws.development_recipe_path(&id);
        let result = (|| -> anyhow::Result<()> {
            ws.ensure_writable("copy recipes")?;
            anyhow::ensure!(!dest.exists(), "{} already exists.", dest.display());
            let mut text = ystudio_core::read_text(&src)?;
            if id != src_id {
                // Keep `id` equal to the file name.
                let re = regex::Regex::new(&format!(r"(?m)^id:\s*['\x22]?{}['\x22]?\s*$", regex::escape(&src_id)))?;
                text = re.replace(&text, format!("id: {id}").as_str()).into_owned();
            }
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::write(&dest, text)?;
            Ok(())
        })();
        app.library.duplicate_of = None;
        match result {
            Ok(()) => {
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
