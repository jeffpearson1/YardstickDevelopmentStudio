//! The Designer: a step-by-step wizard over the field catalog, with guidance
//! from the yardstick-recipes skill and live lint for every field.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::fields::{self, FieldKind, FieldSpec, Section};
use ystudio_core::lint::{Diagnostic, Severity};
use ystudio_core::recipe::RecipeDoc;

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

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_recipe(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    super::recipe_header(app, ui);

    let font_size = app.settings.editor_font_size;
    let dark = app.settings.dark_mode;
    let mut line_numbers = app.settings.line_numbers;
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
    let assigned = ystudio_core::lint::script_assignments(&doc);
    let editor = EditorOpts { icons: &icons, font_size, dark, line_numbers };
    egui::ScrollArea::vertical().id_salt(("designer-fields", r.section)).auto_shrink(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut r.show_all_fields, "Show all fields in this step");
            ui.checkbox(&mut line_numbers, "Line numbers");
            ui.label(RichText::new("Blank fields are removed so they inherit from preferences.yaml.").small().weak());
        });
        ui.add_space(4.0);
        ui.add_enabled_ui(!locked, |ui| {
            for spec in fields::fields_in(section) {
                let present = doc.contains(spec.name);
                let set_by = match spec.kind {
                    FieldKind::Script | FieldKind::Complex => None,
                    _ => assigned.get(&spec.name.to_ascii_lowercase()).copied(),
                };
                let catalog_required = is_required(spec.name, &doc);
                let required = catalog_required && set_by.is_none();
                if !(spec.common || r.show_all_fields || present || catalog_required || set_by.is_some()) {
                    continue;
                }
                let buf = r.designer_bufs.entry(spec.name.to_string()).or_default();
                let field_diags: Vec<&Diagnostic> =
                    diags.iter().filter(|d| d.field.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(spec.name))).collect();
                let state = FieldState { required, present, set_by };
                if field_editor(ui, spec, buf, state, &field_diags, &editor) {
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
    if line_numbers != app.settings.line_numbers {
        app.settings.line_numbers = line_numbers;
        app.save_settings();
    }
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

struct EditorOpts<'a> {
    icons: &'a [String],
    font_size: f32,
    dark: bool,
    line_numbers: bool,
}

#[derive(Clone, Copy)]
struct FieldState<'a> {
    required: bool,
    present: bool,
    /// The download script that assigns this field's variable, if any.
    set_by: Option<&'a str>,
}

fn field_editor(ui: &mut Ui, spec: &FieldSpec, buf: &mut String, state: FieldState, diags: &[&Diagnostic], opts: &EditorOpts) -> bool {
    let mut changed = false;
    let worst = diags.iter().map(|d| d.severity).min();
    let frame_color = worst.map(theme::severity_color).unwrap_or(ui.visuals().widgets.noninteractive.bg_stroke.color);
    // A value only a script sets has nothing to edit here. When the YAML also has
    // one it stays editable: scripts commonly read it first (e.g. a landing-page url).
    let script_only = state.set_by.is_some() && !state.present;
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, frame_color))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(spec.name).monospace().strong());
                if state.required {
                    ui.label(RichText::new("required").small().color(theme::red()));
                }
                if let Some(script) = state.set_by {
                    let (text, hover) = if script_only {
                        (format!("set by {script}"), format!("${} is assigned in {script}, so it is not set here. Edit the script to change it.", spec.name))
                    } else {
                        (format!("reassigned in {script}"), format!("${} is assigned in {script} at run time; this value is its starting point.", spec.name))
                    };
                    ui.label(RichText::new(text).small().color(theme::blue())).on_hover_text(hover);
                }
                ui.label(RichText::new(spec.help).small().weak());
            });
            let width = ui.available_width();
            if script_only {
                let hint = format!("Set by {} at run time", state.set_by.unwrap_or_default());
                ui.add_enabled(false, egui::TextEdit::singleline(&mut String::new()).hint_text(hint).desired_width(width));
            } else {
                changed = field_input(ui, spec, buf, width, opts);
            }
            for d in diags {
                ui.label(RichText::new(format!("{} {}", theme::severity_icon(d.severity), d.message)).color(theme::severity_color(d.severity)));
            }
        });
    ui.add_space(4.0);
    changed
}

fn field_input(ui: &mut Ui, spec: &FieldSpec, buf: &mut String, width: f32, opts: &EditorOpts) -> bool {
    let icons = opts.icons;
    let mut changed = false;
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
            changed |= ui.add(egui::TextEdit::multiline(buf).desired_rows(2).desired_width(width).hint_text("one per line")).changed();
        }
        FieldKind::Script | FieldKind::Complex => {
            let (font_size, dark) = (opts.font_size, opts.dark);
            let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, wrap: f32| highlight::yaml_galley(ui, text.as_str(), font_size, dark, wrap);
            let lines = highlight::line_count(buf);
            let rows = buf.lines().count().clamp(4, 24);
            let edit = egui::TextEdit::multiline(buf).code_editor().desired_rows(rows).desired_width(f32::INFINITY).layouter(&mut layouter);
            changed |= highlight::code_edit(ui, edit, opts.line_numbers, lines, font_size).changed();
        }
    }
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
