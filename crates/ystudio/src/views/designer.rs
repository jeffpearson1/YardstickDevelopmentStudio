//! The Designer: a step-by-step wizard over the field catalog, with guidance
//! from the yardstick-recipes skill and live lint for every field.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::fields::{self, FieldKind, FieldSpec, Section};
use ystudio_core::lint::{Diagnostic, Severity};
use ystudio_core::recipe::RecipeDoc;
use ystudio_core::workspace::RecipeLocation;

use crate::app::{OpenRecipe, Step, StudioApp, View};
use crate::theme::FluentText as _;
use crate::{highlight, theme};

fn section_of(d: &Diagnostic) -> Option<Section> {
    d.field.as_deref().and_then(fields::spec_ci).map(|s| s.section)
}

fn is_required(name: &str, doc: &RecipeDoc) -> bool {
    if fields::ALWAYS_REQUIRED.contains(&name) {
        return true;
    }
    let dt = doc.get_str("detectionType").unwrap_or_default();
    fields::detection_requirements(dt.trim()).contains(&name)
}

pub fn header(app: &mut StudioApp, ui: &mut Ui, title: &str) {
    let Some(r) = &app.recipe else { return };
    let mut save = false;
    let mut revert = false;
    ui.horizontal(|ui| {
        theme::page_title(ui, title);
        ui.label(RichText::new(&r.id).monospace().size(20.0).color(theme::accent()));
        match &r.location {
            RecipeLocation::Development => theme::badge(ui, "development", theme::green()),
            RecipeLocation::Shipped(Some(group)) => theme::badge(ui, format!("shipped / {group}"), theme::blue()),
            RecipeLocation::Shipped(None) => theme::badge(ui, "shipped", theme::blue()),
        };
        if r.dirty() {
            theme::badge(ui, "unsaved", theme::amber());
        }
        if r.read_only {
            theme::badge(ui, "read-only", theme::red());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary(ui, r.dirty() && !r.read_only, "\u{E74E}  Save  (Ctrl+S)").clicked() {
                save = true;
            }
            if ui.add_enabled(r.dirty(), egui::Button::new("\u{E7A7}  Revert")).clicked() {
                revert = true;
            }
            if ui.small_button("\u{E838}").on_hover_text("Open in VS Code / default editor").clicked() {
                super::open_external(&r.path);
            }
        });
    });
    ui.label(RichText::new(r.path.display().to_string()).small().monospace().weak());
    if save {
        app.save_recipe();
    }
    if revert && let Some(r) = &mut app.recipe {
        r.text = r.saved.clone();
        r.changed();
    }
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_recipe(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    header(app, ui, "Designer");
    ui.separator();

    let font_size = app.settings.editor_font_size;
    let dark = app.settings.dark_mode;
    let icons = std::mem::take(&mut app.ws_cache.icons);
    let mut goto = None;
    let r = app.recipe.as_mut().unwrap();

    let parsed = r.take_parsed();
    let (parsed_rev, has_comments) = (parsed.rev, parsed.has_comments);
    let mut doc = match parsed.doc {
        Ok(d) => d,
        Err(e) => {
            theme::banner(ui, theme::red(), |ui| {
                ui.label(RichText::new("The YAML does not parse, so the Designer cannot show it.").semibold());
                ui.label(&e);
                if ui.button("Fix it in the YAML editor").clicked() {
                    goto = Some(View::Editor);
                }
            });
            r.parsed = Some(crate::app::ParsedRecipe { doc: Err(e), ..parsed });
            app.ws_cache.icons = icons;
            if let Some(v) = goto {
                app.view = v;
            }
            return;
        }
    };
    if r.bufs_rev != r.rev {
        r.designer_bufs = fields::FIELDS.iter().map(|f| (f.name.to_string(), doc.text_for_editor(f.name))).collect();
        r.bufs_rev = r.rev;
    }

    let locked = r.read_only || (has_comments && !r.accept_comment_loss);
    if has_comments && !r.accept_comment_loss && !r.read_only {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(RichText::new("This recipe contains comments").semibold().color(theme::amber()));
            ui.label("The Designer rewrites the YAML from its fields, which removes comments. Use the YAML editor to keep them, or continue and lose them.");
            ui.horizontal(|ui| {
                if ui.button("Edit in the Designer anyway").clicked() {
                    r.accept_comment_loss = true;
                }
                if ui.button("Open the YAML editor").clicked() {
                    goto = Some(View::Editor);
                }
            });
        });
    }

    // Stepper.
    let diags = r.diagnostics.clone();
    ui.horizontal_wrapped(|ui| {
        for (i, s) in Section::ALL.iter().enumerate() {
            let errs = diags.iter().filter(|d| d.severity == Severity::Error && section_of(d) == Some(*s)).count();
            let warns = diags.iter().filter(|d| d.severity == Severity::Warning && section_of(d) == Some(*s)).count();
            let mark = if errs > 0 {
                RichText::new(format!("{}. {} \u{EA39}", i + 1, s.title())).color(theme::red())
            } else if warns > 0 {
                RichText::new(format!("{}. {} \u{E7BA}", i + 1, s.title())).color(theme::amber())
            } else {
                RichText::new(format!("{}. {}", i + 1, s.title()))
            };
            if ui.add(egui::Button::selectable(r.section == i, mark.size(14.5))).clicked() {
                r.section = i;
            }
            if i + 1 < Section::ALL.len() {
                ui.label(RichText::new("›").weak());
            }
        }
    });
    ui.add_space(4.0);
    let section = Section::ALL[r.section.min(Section::ALL.len() - 1)];

    egui::Panel::right("designer-guide").resizable(true).default_size(320.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("guide-scroll").show(ui, |ui| {
            theme::section_heading(ui, section.title());
            ui.label(section.guidance());
            ui.add_space(8.0);
            let here: Vec<&Diagnostic> = diags.iter().filter(|d| section_of(d) == Some(section)).collect();
            let general: Vec<&Diagnostic> = diags.iter().filter(|d| section_of(d).is_none()).collect();
            if !here.is_empty() {
                ui.label(RichText::new("Problems in this step").semibold());
                for d in here {
                    diag_line(ui, d);
                }
            }
            if !general.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Recipe-wide").semibold());
                for d in general {
                    diag_line(ui, d);
                }
            }
            if diags.is_empty() {
                ui.colored_label(theme::green(), "\u{E73E}  No problems found by the Studio linter.");
            }
        });
    });

    let mut changed: Vec<String> = Vec::new();
    egui::ScrollArea::vertical().id_salt(("designer-fields", r.section)).auto_shrink(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut r.show_all_fields, "Show all fields in this step");
            ui.label(RichText::new("Blank fields are removed so they inherit from preferences.yaml.").small().weak());
        });
        ui.add_space(4.0);
        ui.add_enabled_ui(!locked, |ui| {
            for spec in fields::fields_in(section) {
                let present = doc.contains(spec.name);
                let required = is_required(spec.name, &doc);
                if !(spec.common || r.show_all_fields || present || required) {
                    continue;
                }
                let buf = r.designer_bufs.entry(spec.name.to_string()).or_default();
                let field_diags: Vec<&Diagnostic> =
                    diags.iter().filter(|d| d.field.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(spec.name))).collect();
                if field_editor(ui, spec, buf, required, &field_diags, &icons, font_size, dark) {
                    changed.push(spec.name.to_string());
                }
            }
            if section == Section::Advanced {
                let unknown: Vec<String> = doc.keys().filter(|k| fields::spec(k).is_none()).map(str::to_string).collect();
                if !unknown.is_empty() {
                    ui.add_space(8.0);
                    theme::banner(ui, theme::amber(), |ui| {
                        ui.label(RichText::new("Fields the catalog does not know").semibold());
                        ui.label(unknown.join(", "));
                        ui.label(RichText::new("Fix casing or remove them in the YAML editor.").small());
                    });
                }
            }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if r.section > 0 && ui.button("← Back").clicked() {
                r.section -= 1;
            }
            if r.section + 1 < Section::ALL.len() {
                if ui.button(RichText::new("Next →").semibold()).clicked() {
                    r.section += 1;
                }
            } else if ui.button(RichText::new("\u{E9D5}  Test this recipe →").semibold()).clicked() {
                goto = Some(View::TestBench);
            }
        });
    });

    if !changed.is_empty() {
        apply_changes(r, &mut doc, &changed);
    }
    if r.rev == parsed_rev {
        r.parsed = Some(crate::app::ParsedRecipe { rev: parsed_rev, doc: Ok(doc), has_comments });
    }
    app.ws_cache.icons = icons;
    if ui.ctx().data_mut(|d| d.remove_temp::<bool>(egui::Id::unique(FIND_ICON))).unwrap_or(false) {
        find_icon(app);
        return;
    }
    if goto == Some(View::TestBench) {
        app.view = View::TestBench;
        if app.step_state(&app.recipe.as_ref().unwrap().id, Step::Lint).is_none() {
            app.run_steps(&[Step::Lint]);
        }
    } else if let Some(v) = goto {
        app.view = v;
    }
}

const FIND_ICON: &str = "designer-find-icon";

/// Opens the Icon finder for the open recipe, using its current (unsaved) fields.
fn find_icon(app: &mut StudioApp) {
    let Some(r) = app.recipe.as_ref() else { return };
    let id = r.id.clone();
    let field = |k: &str| r.designer_bufs.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let (display, publisher, url, file) = (field("displayName"), field("publisher"), field("url"), field("iconFile"));
    super::icons::prepare_for_recipe(app, &id);
    let st = &mut app.icons;
    if !display.is_empty() {
        st.name = display;
    }
    if !publisher.is_empty() {
        st.publisher = publisher;
    }
    if url.starts_with("http") {
        st.vendor_url = url;
    }
    if !file.is_empty() {
        st.file_name = file;
    }
    st.auto_search = !st.name.is_empty();
    app.view = View::Icons;
}

fn apply_changes(r: &mut OpenRecipe, doc: &mut RecipeDoc, changed: &[String]) {
    for name in changed {
        let text = r.designer_bufs.get(name).cloned().unwrap_or_default();
        doc.set_from_text(name, &text);
    }
    r.text = doc.to_yaml();
    r.changed();
    // The buffers already hold what the user typed; don't normalise under the cursor.
    r.bufs_rev = r.rev;
}

fn diag_line(ui: &mut Ui, d: &Diagnostic) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(theme::severity_icon(d.severity)).color(theme::severity_color(d.severity)));
        if let Some(f) = &d.field {
            ui.label(RichText::new(f).monospace().small());
        }
        ui.label(&d.message);
        if d.from_validator {
            ui.label(RichText::new("(validator)").small().weak());
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn field_editor(
    ui: &mut Ui,
    spec: &FieldSpec,
    buf: &mut String,
    required: bool,
    diags: &[&Diagnostic],
    icons: &[String],
    font_size: f32,
    dark: bool,
) -> bool {
    let mut changed = false;
    let worst = diags.iter().map(|d| d.severity).min();
    let frame_color = worst.map(theme::severity_color).unwrap_or(ui.visuals().widgets.noninteractive.bg_stroke.color);
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, frame_color))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(spec.name).monospace().strong());
                if required {
                    ui.label(RichText::new("required").small().color(theme::red()));
                }
                ui.label(RichText::new(spec.help).small().weak());
            });
            let width = ui.available_width();
            match spec.kind {
                FieldKind::Text if spec.name == "iconFile" => {
                    ui.horizontal(|ui| {
                        changed |= ui.add(egui::TextEdit::singleline(buf).desired_width(260.0)).changed();
                        egui::ComboBox::from_id_salt("icon-pick").selected_text("Pick from Icons\\").height(400.0).show_ui(ui, |ui| {
                            for icon in icons {
                                if ui.selectable_label(buf == icon, icon).clicked() {
                                    *buf = icon.clone();
                                    changed = true;
                                }
                            }
                        });
                        let t = buf.trim();
                        if !t.is_empty() {
                            if icons.iter().any(|i| i == t) {
                                ui.colored_label(theme::green(), "\u{E73E}  found");
                            } else {
                                ui.colored_label(theme::red(), "\u{E711}  not in Icons\\");
                            }
                        }
                        if ui.button("\u{E91B}  Find icon…").on_hover_text("Search installers, Wikidata, Chocolatey, the vendor site and the web").clicked() {
                            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique(FIND_ICON), true));
                        }
                    });
                }
                FieldKind::Text | FieldKind::Int => {
                    changed |= ui.add(egui::TextEdit::singleline(buf).desired_width(width)).changed();
                }
                FieldKind::Enum(values) => {
                    changed |= choice(ui, spec.name, buf, values);
                }
                FieldKind::Bool => {
                    changed |= choice(ui, spec.name, buf, &["true", "false"]);
                }
                FieldKind::List => {
                    changed |= ui
                        .add(egui::TextEdit::multiline(buf).desired_rows(2).desired_width(width).hint_text("one per line"))
                        .changed();
                }
                FieldKind::Script | FieldKind::Complex => {
                    let mut layouter =
                        |ui: &Ui, text: &dyn egui::TextBuffer, wrap: f32| highlight::yaml_galley(ui, text.as_str(), font_size, dark, wrap);
                    let rows = buf.lines().count().clamp(4, 24);
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(buf)
                                .code_editor()
                                .desired_rows(rows)
                                .desired_width(width)
                                .layouter(&mut layouter),
                        )
                        .changed();
                }
            }
            for d in diags {
                ui.label(RichText::new(format!("{} {}", theme::severity_icon(d.severity), d.message)).color(theme::severity_color(d.severity)));
            }
        });
    ui.add_space(4.0);
    changed
}

fn choice(ui: &mut Ui, name: &str, buf: &mut String, values: &[&str]) -> bool {
    let mut changed = false;
    let shown = if buf.trim().is_empty() { "(not set - inherit)".to_string() } else { buf.clone() };
    egui::ComboBox::from_id_salt(("choice", name)).selected_text(shown).show_ui(ui, |ui| {
        if ui.selectable_label(buf.trim().is_empty(), "(not set - inherit)").clicked() {
            buf.clear();
            changed = true;
        }
        for v in values {
            if ui.selectable_label(buf == v, *v).clicked() {
                *buf = v.to_string();
                changed = true;
            }
        }
        if !buf.trim().is_empty() && !values.iter().any(|v| v.eq_ignore_ascii_case(buf.trim())) {
            ui.label(RichText::new(format!("current: {buf}")).color(theme::amber()));
        }
    });
    changed
}
