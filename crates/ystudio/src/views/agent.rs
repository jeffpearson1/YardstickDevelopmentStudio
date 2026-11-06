//! Agentic authoring: hand a task to GitHub Copilot CLI (or Claude Code) with
//! the yardstick-recipes skill and the Studio's guardrails, stream its work,
//! then review what changed and send the result to the Test Bench.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::{self, AgentBackend, AgentTask, Change, Snapshot};

use crate::app::{JobKind, Step, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    #[default]
    Create,
    Fix,
    Review,
    Update,
    Freeform,
}

impl TaskKind {
    const ALL: [(TaskKind, &'static str, &'static str); 5] = [
        (TaskKind::Create, "\u{E710}  Create", "Research a vendor and write a new recipe, then dry-run it."),
        (TaskKind::Fix, "\u{E90F}  Fix", "Diagnose a failing recipe from harness evidence and fix it."),
        (TaskKind::Review, "\u{E721}  Review", "Read-only review against the skill; no files change."),
        (TaskKind::Update, "\u{E895}  Update", "Refresh download source, version scheme or switches."),
        (TaskKind::Freeform, "\u{E8BD}  Freeform", "Anything else - the guardrails still apply."),
    ];
}

pub struct AgentState {
    pub kind: TaskKind,
    pub app_name: String,
    pub id_hint: String,
    pub vendor_url: String,
    pub notes: String,
    pub target_id: String,
    pub evidence: String,
    pub freeform: String,
    pub show_prompt: bool,
    pub job: Option<u64>,
    pub changes: Vec<Change>,
    pub reported: Vec<String>,
    /// Whether each of [`AgentBackend::ALL`] is installed; probed once (PATH scan).
    backends_found: Option<[bool; AgentBackend::ALL.len()]>,
    pub auto_test: bool,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            kind: TaskKind::default(), app_name: String::new(), id_hint: String::new(),
            vendor_url: String::new(), notes: String::new(), target_id: String::new(),
            evidence: String::new(), freeform: String::new(), show_prompt: false,
            job: None, changes: Vec::new(), reported: Vec::new(), backends_found: None, auto_test: true,
        }
    }
}

impl AgentState {
    pub fn prepare_fix(&mut self, id: &str, evidence: String) {
        self.kind = TaskKind::Fix;
        self.target_id = id.to_string();
        self.evidence = evidence;
    }

    pub fn start(app: &mut StudioApp, mut task: AgentTask, options: Option<crate::workflow::Options>) {
        let verification = options.and_then(|options| crate::workflow::task_id(&task)
            .map(|recipe_id| crate::workflow::Verification { recipe_id, options }));
        if let AgentTask::Create { id_hint, .. } = &mut task
            && let Some(verification) = &verification
        {
            *id_hint = verification.recipe_id.clone();
        }
        let Some(ws) = &app.ws else {
            app.error("Open a development workspace before starting the agent.");
            return;
        };
        match agent::build_command(ws, app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &task) {
            Ok(spec) => {
                let before = Snapshot::take(ws);
                app.agent.changes.clear();
                app.agent.reported.clear();
                app.workflow.message.clear();
                app.agent.job = app.start_job(spec, JobKind::Agent { before, batch_item: None, verification });
                app.save_settings();
            }
            Err(e) => app.error(format!("{e:#}")),
        }
    }

    fn task(&self) -> Result<AgentTask, &'static str> {
        let need_id = || if self.target_id.trim().is_empty() { Err("Choose the recipe id.") } else { Ok(self.target_id.trim().to_string()) };
        Ok(match self.kind {
            TaskKind::Create => {
                if self.app_name.trim().is_empty() {
                    return Err("Name the application to package.");
                }
                AgentTask::Create {
                    app: self.app_name.trim().to_string(),
                    id_hint: self.id_hint.trim().to_string(),
                    vendor_url: self.vendor_url.clone(),
                    notes: self.notes.clone(),
                }
            }
            TaskKind::Fix => AgentTask::Fix { id: need_id()?, evidence: self.evidence.clone(), notes: self.notes.clone() },
            TaskKind::Review => AgentTask::Review { id: need_id()? },
            TaskKind::Update => AgentTask::Update { id: need_id()?, notes: self.notes.clone() },
            TaskKind::Freeform => {
                if self.freeform.trim().is_empty() {
                    return Err("Write a prompt.");
                }
                AgentTask::Freeform { prompt: self.freeform.clone() }
            }
        })
    }
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_workspace(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    ui.horizontal(|ui| {
        theme::page_title(ui, "Agent");
        ui.label(RichText::new("powered by the yardstick-recipes skill").weak());
    });
    let ws = app.ws.clone().unwrap();
    if !app.ws_cache.has_skill {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label("This folder has no .claude\\skills\\yardstick-recipes\\SKILL.md. Pull the latest Yardstick first.");
        });
    }
    // Two agents writing at once would muddle each other's change lists.
    let running = crate::workflow::busy(app) || !app.batch.queue.is_empty();
    let open_id = app.recipe.as_ref().map(|r| r.id.clone());
    let ids: Vec<String> = app.recipes.iter().map(|r| r.id.clone()).collect();

    egui::Panel::left("agent-form").resizable(true).default_size(430.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("agent-form-scroll").show(ui, |ui| {
            let st = &mut app.agent;
            ui.horizontal_wrapped(|ui| {
                for (k, label, _) in TaskKind::ALL {
                    ui.selectable_value(&mut st.kind, k, label);
                }
            });
            let blurb = TaskKind::ALL.iter().find(|(k, ..)| *k == st.kind).map(|t| t.2).unwrap_or_default();
            ui.label(RichText::new(blurb).weak());
            ui.add_space(6.0);
            egui::Grid::new("agent-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                match st.kind {
                    TaskKind::Create => {
                        ui.label("Application");
                        ui.add(egui::TextEdit::singleline(&mut st.app_name).hint_text("e.g. Notepad++").desired_width(ui.available_width()));
                        ui.end_row();
                        ui.label("Recipe id");
                        ui.add(egui::TextEdit::singleline(&mut st.id_hint).hint_text(ystudio_core::recipe::suggest_id(&st.app_name)).desired_width(ui.available_width()));
                        ui.end_row();
                        ui.label("Vendor page");
                        ui.add(egui::TextEdit::singleline(&mut st.vendor_url).hint_text("optional").desired_width(ui.available_width()));
                        ui.end_row();
                    }
                    TaskKind::Fix | TaskKind::Review | TaskKind::Update => {
                        ui.label("Recipe");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("agent-target")
                                .selected_text(if st.target_id.is_empty() { "choose…".to_string() } else { st.target_id.clone() })
                                .height(400.0)
                                .show_ui(ui, |ui| {
                                    for id in &ids {
                                        ui.selectable_value(&mut st.target_id, id.clone(), id);
                                    }
                                });
                            if let Some(id) = &open_id
                                && ui.small_button("use open recipe").clicked()
                            {
                                st.target_id = id.clone();
                            }
                        });
                        ui.end_row();
                    }
                    TaskKind::Freeform => {}
                }
            });
            match st.kind {
                TaskKind::Freeform => {
                    ui.label("Prompt");
                    ui.add(egui::TextEdit::multiline(&mut st.freeform).desired_rows(8).desired_width(ui.available_width()));
                }
                TaskKind::Review => {}
                _ => {
                    ui.label("Notes for the agent");
                    ui.add(egui::TextEdit::multiline(&mut st.notes).desired_rows(3).desired_width(ui.available_width()).hint_text("Constraints, preferred source, known quirks…"));
                }
            }
            if st.kind == TaskKind::Fix {
                ui.label("Evidence (harness output, error text)");
                ui.add(egui::TextEdit::multiline(&mut st.evidence).code_editor().desired_rows(6).desired_width(ui.available_width()));
            }
            ui.add_space(6.0);
            ui.collapsing("Agent backend", |ui| {
                let found = app.agent.backends_found.get_or_insert_with(|| AgentBackend::ALL.map(|b| b.locate().is_some()));
                ui.horizontal(|ui| {
                    for (b, found) in AgentBackend::ALL.into_iter().zip(*found) {
                        let label = if found { b.label().to_string() } else { format!("{} (not found)", b.label()) };
                        ui.selectable_value(&mut app.settings.agent_backend, b, label);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Model");
                    ui.add(egui::TextEdit::singleline(&mut app.settings.agent_model).hint_text("CLI default").desired_width(200.0));
                });
            });
            ui.collapsing("Guardrails", |ui| {
                ui.label(RichText::new(
                    "The agent prefers vendor internet downloads; software dropbox is a last resort. It may only write \
                     inside this workspace and run safe checks. Studio owns live tests, requires consent, and sends \
                     recipe failures back for bounded repairs. Production, publishing and git push remain denied.",
                ).small());
            });
            let st = &mut app.agent;
            let task = st.task();
            let can_verify = task.as_ref().ok().and_then(crate::workflow::task_id).is_some();
            if can_verify {
                ui.add_enabled(!running, egui::Checkbox::new(&mut st.auto_test, "Test and repair automatically"));
                if st.auto_test {
                    ui.add_enabled_ui(!running, |ui| crate::workflow::configure(&mut app.workflow.options, ui, true));
                }
            }
            ui.checkbox(&mut st.show_prompt, "Show the full prompt");
            if st.show_prompt
                && let Ok(t) = &task
            {
                let mut prompt = agent::full_prompt(&ws, &app.settings.production_paths, t);
                ui.add(egui::TextEdit::multiline(&mut prompt).code_editor().desired_rows(10).desired_width(ui.available_width()).interactive(false));
            }
            ui.add_space(6.0);
            if let Err(why) = &task {
                ui.label(RichText::new(*why).weak());
            }
            let can_run = task.is_ok() && app.writable() && !running;
            if theme::primary(ui, can_run, RichText::new("\u{E99A}  Start agent")).clicked() {
                let task = task.unwrap();
                if can_verify && app.agent.auto_test {
                    crate::workflow::request(app, crate::workflow::Request::Agent(task));
                } else {
                    AgentState::start(app, task, None);
                }
            }
        });
    });

    crate::workflow::progress(app, ui);
    // Output, changes and next steps.
    if !app.agent.changes.is_empty() || !app.agent.reported.is_empty() {
        let mut open = None;
        let mut test = None;
        theme::card(ui, |ui| {
            ui.label(RichText::new("Result").semibold());
            for id in &app.agent.reported {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(id).monospace().strong());
                    if ui.button("Open in Designer").clicked() {
                        open = Some(id.clone());
                    }
                    if ui.button("\u{E9D5}  Test it").clicked() {
                        test = Some(id.clone());
                    }
                });
            }
            if !app.agent.changes.is_empty() {
                ui.collapsing(format!("{} file(s) changed", app.agent.changes.len()), |ui| {
                    for c in &app.agent.changes {
                        let (tag, color) = match c {
                            Change::Added(_) => ("added", theme::green()),
                            Change::Modified(_) => ("modified", theme::amber()),
                            Change::Removed(_) => ("removed", theme::red()),
                        };
                        ui.horizontal(|ui| {
                            theme::badge(ui, tag, color);
                            let rel = c.path().strip_prefix(&ws.root).unwrap_or(c.path());
                            if ui.link(rel.display().to_string()).clicked() && c.path().is_file() {
                                super::open_external(c.path());
                            }
                        });
                    }
                });
            }
            ui.label(RichText::new("Only Studio's recorded harness evidence establishes validation; the agent's claims do not.").small().weak());
        });
        if let Some(id) = open
            && app.open_recipe_by_id(&id)
        {
            app.view = View::Designer;
        }
        if let Some(id) = test
            && app.open_recipe_by_id(&id)
        {
            app.view = View::TestBench;
            app.run_steps(&[Step::Lint, Step::Schema, Step::DryRun]);
        }
    }
    match app.agent.job.and_then(|j| app.job_mut(j)) {
        Some(job) => job_log(ui, &mut job.proc, "agent"),
        None => {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Describe a task on the left and start the agent.").weak());
                ui.label(RichText::new("Its output streams here; changed files and reported recipes appear when it finishes.").small().weak());
            });
        }
    }
}
