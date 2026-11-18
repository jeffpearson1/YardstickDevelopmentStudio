//! Agentic authoring: hand a task to GitHub Copilot CLI, Claude Code or Codex with
//! the yardstick-recipes skill and the Studio's guardrails, stream its work,
//! then review what changed and send the result to the Test Bench.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::{self, AgentBackend, AgentTask, Change, Snapshot};
use ystudio_core::intake::{Batch, BatchItem, ItemStatus};

use crate::app::{JobKind, Step, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    #[default]
    Fix,
    Review,
    Update,
    Freeform,
}

impl TaskKind {
    fn name(self) -> &'static str {
        match self {
            TaskKind::Fix => "Fix",
            TaskKind::Review => "Review",
            TaskKind::Update => "Update",
            TaskKind::Freeform => "Freeform",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [TaskKind::Fix, TaskKind::Review, TaskKind::Update, TaskKind::Freeform].into_iter().find(|k| k.name() == name)
    }

    const ALL: [(TaskKind, &'static str, &'static str); 4] = [
        (TaskKind::Fix, "\u{E90F}  Fix", "Diagnose a failing recipe from harness evidence and fix it."),
        (TaskKind::Review, "\u{E721}  Review", "Read-only review against the skill; no files change."),
        (TaskKind::Update, "\u{E895}  Update", "Refresh download source, version scheme or switches."),
        (TaskKind::Freeform, "\u{E8BD}  Freeform", "Anything else - the guardrails still apply."),
    ];
}

/// What the user is writing for one recipe's agent task. Each recipe keeps its
/// own, so switching recipes never carries a prompt across.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub kind: TaskKind,
    pub notes: String,
    pub evidence: String,
    pub freeform: String,
}

impl Draft {
    /// The text the user wrote for this task kind (not the pasted evidence).
    fn prompt_text(&self) -> &str {
        match self.kind {
            TaskKind::Freeform => &self.freeform,
            TaskKind::Review => "",
            _ => &self.notes,
        }
    }

    fn clear_prompt(&mut self) {
        self.notes.clear();
        self.evidence.clear();
        self.freeform.clear();
    }
}

pub struct AgentState {
    /// The open recipe's draft; others wait in `drafts`.
    pub draft: Draft,
    drafts: std::collections::HashMap<String, Draft>,
    /// The recipe `draft` belongs to.
    pub target_id: String,
    pub show_prompt: bool,
    /// The latest agent run, whichever recipe it is for.
    pub job: Option<u64>,
    /// The recipe the latest run is for.
    pub job_recipe: String,
    /// Each recipe's latest run, so its tab shows its own output.
    last_jobs: std::collections::HashMap<String, u64>,
    pub changes: Vec<Change>,
    pub reported: Vec<String>,
    /// Whether each of [`AgentBackend::ALL`] is installed; probed once (PATH scan).
    pub backends_found: Option<[bool; AgentBackend::ALL.len()]>,
    pub auto_test: bool,
    /// The request tracking the latest run: (request id, name).
    pub request: Option<(String, String)>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            draft: Draft::default(), drafts: Default::default(), target_id: String::new(), show_prompt: false,
            job: None, job_recipe: String::new(), last_jobs: Default::default(),
            changes: Vec::new(), reported: Vec::new(), backends_found: None, auto_test: true, request: None,
        }
    }
}

impl AgentState {
    /// Makes `id`'s draft the active one, keeping the previous recipe's aside.
    pub fn focus(&mut self, id: &str) {
        if self.target_id.eq_ignore_ascii_case(id) {
            self.target_id = id.to_string();
            return;
        }
        let previous = std::mem::take(&mut self.draft);
        if !self.target_id.is_empty() && previous != Draft::default() {
            self.drafts.insert(self.target_id.to_lowercase(), previous);
        }
        self.draft = self.drafts.remove(&id.to_lowercase()).unwrap_or_default();
        self.target_id = id.to_string();
    }

    pub fn prepare_fix(&mut self, id: &str, evidence: String) {
        self.focus(id);
        self.draft.kind = TaskKind::Fix;
        self.draft.evidence = evidence;
    }

    /// The latest run for `id`, if its job is still listed.
    fn job_for(&self, id: &str) -> Option<u64> {
        self.last_jobs.get(&id.to_lowercase()).copied()
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
        match agent::build_command(ws, app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &task, super::settings::agent_options(app)) {
            Ok(spec) => {
                let before = Snapshot::take(ws);
                app.agent.changes.clear();
                app.agent.reported.clear();
                app.workflow.message.clear();
                let mut request = tracking_request(app, &task);
                let batch_item = request.as_ref().map(|b| (b.id.clone(), 0));
                app.agent.job = app.start_job(spec, JobKind::Agent { before, batch_item, verification });
                app.agent.job_recipe = app.agent.target_id.clone();
                if let Some(job) = app.agent.job
                    && !app.agent.target_id.is_empty()
                {
                    app.agent.last_jobs.insert(app.agent.target_id.to_lowercase(), job);
                }
                if let Some(b) = &mut request {
                    let started = app.agent.job.is_some();
                    b.items[0].status = if started { ItemStatus::Running } else { ItemStatus::Failed };
                    b.items[0].status_note = if started { format!("{} started", app.settings.agent_backend.label()) } else { "could not start the agent".into() };
                    match b.save() {
                        Ok(()) => app.agent.request = Some((b.id.clone(), super::home::request_label(b))),
                        Err(e) => app.error(format!("Could not save the request for this run: {e:#}")),
                    }
                    app.batch.saved = None;
                }
                app.save_settings();
            }
            Err(e) => app.error(format!("{e:#}")),
        }
    }

    fn task(&self) -> Result<AgentTask, &'static str> {
        let need_id = || if self.target_id.trim().is_empty() { Err("Choose the recipe id.") } else { Ok(self.target_id.trim().to_string()) };
        let d = &self.draft;
        Ok(match d.kind {
            TaskKind::Fix => AgentTask::Fix { id: need_id()?, evidence: d.evidence.clone(), notes: d.notes.clone() },
            TaskKind::Review => AgentTask::Review { id: need_id()? },
            TaskKind::Update => AgentTask::Update { id: need_id()?, notes: d.notes.clone() },
            TaskKind::Freeform => {
                if d.freeform.trim().is_empty() {
                    return Err("Write a prompt.");
                }
                AgentTask::Freeform { prompt: d.freeform.clone(), recipe: Some(self.target_id.trim().to_string()).filter(|id| !id.is_empty()) }
            }
        })
    }
}

/// Most prompts kept in the history.
const HISTORY_LIMIT: usize = 50;

/// Records the prompt the user just started with (newest first, no repeats).
fn remember_prompt(app: &mut StudioApp) {
    let text = app.agent.draft.prompt_text().trim().to_string();
    if text.is_empty() {
        return;
    }
    let kind = app.agent.draft.kind.name().to_string();
    let history = &mut app.settings.agent_prompt_history;
    history.retain(|h| !(h.text == text && h.kind == kind && h.recipe.eq_ignore_ascii_case(&app.agent.target_id)));
    history.insert(0, ystudio_core::settings::PromptRecord {
        recipe: app.agent.target_id.clone(),
        kind,
        text,
        at: ystudio_core::intake::timestamp(),
    });
    history.truncate(HISTORY_LIMIT);
}

/// The History menu: this recipe's prompts first, then other recipes'. Picking
/// one puts it back in the form, with its task kind.
fn history_menu(app: &mut StudioApp, ui: &mut Ui) {
    let id = app.agent.target_id.clone();
    let (mine, others): (Vec<_>, Vec<_>) = app.settings.agent_prompt_history.iter().cloned().partition(|h| h.recipe.eq_ignore_ascii_case(&id));
    let mut picked = None;
    ui.add_enabled_ui(!(mine.is_empty() && others.is_empty()), |ui| {
        ui.menu_button(format!("\u{E81C}  History ({})", mine.len() + others.len()), |ui| {
            ui.set_max_width(420.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (heading, list) in [(id.clone(), &mine), ("Other recipes".to_string(), &others)] {
                    if list.is_empty() {
                        continue;
                    }
                    ui.label(RichText::new(heading).small().weak());
                    for h in list {
                        let first = h.text.lines().next().unwrap_or_default();
                        let short: String = first.chars().take(70).collect();
                        let more = if short.len() < h.text.trim_end().len() { "…" } else { "" };
                        let label = format!("{}  {short}{more}", h.kind);
                        let hover = format!("{} · {}\n\n{}", if h.recipe.is_empty() { "no recipe" } else { &h.recipe }, h.at, h.text);
                        if ui.button(label).on_hover_text(hover).clicked() {
                            picked = Some(h.clone());
                            ui.close();
                        }
                    }
                    ui.add_space(4.0);
                }
            });
        });
    });
    if let Some(h) = picked {
        let d = &mut app.agent.draft;
        d.kind = TaskKind::from_name(&h.kind).unwrap_or(TaskKind::Freeform);
        if d.kind == TaskKind::Review {
            d.kind = TaskKind::Freeform;
        }
        match d.kind {
            TaskKind::Freeform => d.freeform = h.text,
            _ => d.notes = h.text,
        }
    }
}

/// A one-item request recording an Agent tab run, so its status and test
/// results are kept in Recipe Builder like any other work. It is saved, not
/// opened: the request open in Recipe Builder stays as it is.
pub(crate) fn tracking_request(app: &StudioApp, task: &AgentTask) -> Option<Batch> {
    let recipe = app.agent.target_id.trim().to_string();
    if recipe.is_empty() {
        return None;
    }
    let name = app.recipes.iter().find(|r| r.id == recipe).map(|r| r.meta.display_name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| recipe.clone());
    let verb = match task {
        AgentTask::Create { .. } => "new",
        AgentTask::Fix { .. } => "fix",
        AgentTask::Review { .. } => "review",
        AgentTask::Update { .. } => "update",
        AgentTask::Freeform { .. } => "agent task on",
    };
    Some(Batch::from_items(&format!("{verb} {recipe}"), vec![BatchItem::for_task(task, &recipe, &name)]))
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    if !super::need_recipe(app, ui) {
        return;
    }
    super::production_banner(app, ui);
    super::recipe_header(app, ui);
    let ws = app.ws.clone().unwrap();
    if !app.ws_cache.has_skill {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label("This folder has no .claude\\skills\\yardstick-recipes\\SKILL.md. Pull the latest Yardstick first.");
        });
    }
    // Two agents writing at once would muddle each other's change lists.
    let running = crate::workflow::busy(app) || !app.batch.queue.is_empty();
    let open_id = app.recipe.as_ref().map(|r| r.id.clone()).unwrap_or_default();

    egui::Panel::left("agent-form").resizable(true).default_size(430.0).frame(theme::pane_frame()).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("agent-form-scroll").show(ui, |ui| {
            // The tab always works on the open recipe, with that recipe's own draft.
            app.agent.focus(&open_id);
            let st = &mut app.agent;
            ui.horizontal_wrapped(|ui| {
                for (k, label, _) in TaskKind::ALL {
                    ui.selectable_value(&mut st.draft.kind, k, label);
                }
            });
            let blurb = TaskKind::ALL.iter().find(|(k, ..)| *k == st.draft.kind).map(|t| t.2).unwrap_or_default();
            ui.label(RichText::new(blurb).weak());
            ui.add_space(6.0);
            let kind = st.draft.kind;
            if kind != TaskKind::Review {
                ui.horizontal(|ui| {
                    ui.label(if kind == TaskKind::Freeform { "Prompt" } else { "Notes for the agent" });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| history_menu(app, ui));
                });
            }
            let st = &mut app.agent;
            match st.draft.kind {
                TaskKind::Freeform => {
                    ui.add(egui::TextEdit::multiline(&mut st.draft.freeform).desired_rows(8).desired_width(ui.available_width()));
                }
                TaskKind::Review => {}
                _ => {
                    ui.add(egui::TextEdit::multiline(&mut st.draft.notes).desired_rows(3).desired_width(ui.available_width()).hint_text("Constraints, preferred source, known quirks…"));
                }
            }
            if st.draft.kind == TaskKind::Fix {
                ui.label("Evidence (harness output, error text)");
                ui.add(egui::TextEdit::multiline(&mut st.draft.evidence).code_editor().desired_rows(6).desired_width(ui.available_width()));
            }
            ui.add_space(6.0);
            ui.label("Agent and model");
            ui.add_enabled_ui(!running, |ui| {
                super::settings::agent_picker(app, ui, "agent-tab");
                super::settings::downloads_checkbox(app, ui);
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
                let prompt = agent::full_prompt(&ws, &app.settings.production_paths, t, super::settings::agent_options(app));
                // `&mut &str` is a read-only buffer that can still be selected and copied.
                ui.add(egui::TextEdit::multiline(&mut prompt.as_str()).code_editor().desired_rows(10).desired_width(ui.available_width()));
            }
            ui.add_space(6.0);
            if let Err(why) = &task {
                ui.label(RichText::new(*why).weak());
            }
            let can_run = task.is_ok() && app.writable() && !running;
            if theme::primary(ui, can_run, RichText::new("\u{E99A}  Start agent")).clicked() {
                let task = task.unwrap();
                // The prompt goes to History, then the form is ready for the next one.
                remember_prompt(app);
                app.agent.draft.clear_prompt();
                if can_verify && app.agent.auto_test {
                    crate::workflow::request(app, crate::workflow::Request::Agent(task));
                } else {
                    AgentState::start(app, task, None);
                }
                app.save_settings();
            }
        });
    });

    theme::pane(ui, |ui| {
    crate::workflow::progress(app, ui);
    let shown_job = app.agent.job_for(&open_id).filter(|j| app.job(*j).is_some());
    let other_running = app.agent.job.filter(|j| Some(*j) != shown_job && app.job(*j).is_some_and(|job| job.proc.is_running()));
    if other_running.is_some() && !app.agent.job_recipe.is_empty() {
        let other = app.agent.job_recipe.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("The agent is working on").weak());
            if ui.link(&other).on_hover_text("Open that recipe's Agent tab.").clicked() && app.open_recipe_by_id(&other) {
                app.view = View::Agent;
            }
        });
    }
    // Output, changes and next steps of this recipe's latest run.
    let own_result = shown_job.is_some() && shown_job == app.agent.job;
    if own_result && (!app.agent.changes.is_empty() || !app.agent.reported.is_empty()) {
        let mut open = None;
        let mut test = None;
        theme::card(ui, |ui| {
            ui.label(RichText::new("Result").semibold());
            for id in &app.agent.reported {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(id).monospace().strong());
                    if ui.button("Open in Design").clicked() {
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
            if let Some(u) = app.agent.job.and_then(|j| app.usage_by_job.get(&j)) {
                ui.label(format!("Used {:.1} AI credits ({}{}).", u.credits, u.model, if u.subagents > 0 { format!(", {} subagent(s)", u.subagents) } else { String::new() }));
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
    if let Some((id, label)) = app.agent.request.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Tracked in Recipe Builder as").weak());
            if ui.link(&label).on_hover_text("Its status and test results are kept there.").clicked() {
                if app.batch.busy(app) && app.batch.batch.as_ref().is_none_or(|b| b.id != id) {
                    app.error("Finish or cancel the active run before switching requests.");
                } else {
                    super::batch::open_saved(app, &Batch::root().join(&id));
                    app.view = View::Batch;
                }
            }
        });
    }
    match shown_job.and_then(|j| app.job_mut(j)) {
        Some(job) => job_log(ui, &mut job.proc, "agent"),
        None => {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Describe a task on the left and start the agent.").weak());
                ui.label(RichText::new("Its output streams here; changed files and reported recipes appear when it finishes.").small().weak());
            });
        }
    }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::fake_app;

    #[test]
    fn each_recipe_keeps_its_own_draft() {
        let mut st = AgentState::default();
        st.focus("one");
        st.draft.kind = TaskKind::Freeform;
        st.draft.freeform = "for one".into();
        st.focus("two");
        assert_eq!(st.draft, Draft::default(), "a new recipe starts empty");
        st.draft.notes = "for two".into();
        st.focus("ONE");
        assert_eq!((st.draft.kind, st.draft.freeform.as_str()), (TaskKind::Freeform, "for one"));
        st.prepare_fix("two", "evidence".into());
        assert_eq!((st.draft.kind, st.draft.notes.as_str(), st.draft.evidence.as_str()), (TaskKind::Fix, "for two", "evidence"));
    }

    #[test]
    fn started_prompts_go_to_history_newest_first_without_repeats() {
        let (_dir, mut app) = fake_app();
        app.agent.focus("app");
        for text in ["first", "second", "first"] {
            app.agent.draft.kind = TaskKind::Freeform;
            app.agent.draft.freeform = text.into();
            remember_prompt(&mut app);
        }
        let texts: Vec<_> = app.settings.agent_prompt_history.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(app.settings.agent_prompt_history[0].recipe, "app");
        // A review has no prompt to remember.
        app.agent.draft.kind = TaskKind::Review;
        remember_prompt(&mut app);
        assert_eq!(app.settings.agent_prompt_history.len(), 2);
        app.agent.draft.clear_prompt();
        assert!(app.agent.draft.freeform.is_empty());
    }
}
