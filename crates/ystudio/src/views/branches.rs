//! The footer's branch chip and menu, like VS Code's: the development
//! workspace's branch and sync state, and a menu to search, switch and create
//! branches, fetch, pull and push.

use eframe::egui::{self, RichText, Ui};
use ystudio_core::git::{self, Branch};

use crate::app::{JobKind, StudioApp};
use crate::theme;

/// Segoe Fluent Icons "Branch fork".
const BRANCH: &str = "\u{F003}";

#[derive(Default)]
pub struct BranchMenu {
    pub open: bool,
    /// Search text, or the name of a branch to create.
    query: String,
    branches: Vec<Branch>,
    /// Tracked files with uncommitted changes when the menu opened.
    changed: usize,
    error: Option<String>,
    /// Where the chip is, to anchor the menu above it.
    anchor: Option<egui::Rect>,
    focus_query: bool,
}

impl BranchMenu {
    /// Reads branches and uncommitted changes (local git commands, no network).
    fn load(&mut self, root: &std::path::Path) {
        self.error = None;
        match git::branches(root) {
            Ok(b) => self.branches = b,
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.changed = git::changed_files(root).unwrap_or(0);
    }
}

fn git_busy(app: &StudioApp) -> bool {
    app.jobs.iter().any(|j| {
        j.proc.is_running() && matches!(j.kind, JobKind::GitFetch | JobKind::GitPull | JobKind::GitSwitch { .. } | JobKind::GitPush)
    })
}

/// Why the workspace cannot change branch right now, if it cannot.
fn switch_blocker(app: &StudioApp) -> Option<&'static str> {
    if git_busy(app) {
        Some("A git command is still running.")
    } else if app.jobs.iter().any(|j| j.proc.is_running()) || crate::workflow::busy(app) || !app.batch.queue.is_empty() {
        Some("Agents, checks or other jobs are running in this workspace; switching would change files under them.")
    } else if app.recipe.as_ref().is_some_and(|r| r.dirty()) {
        Some("The open recipe has unsaved changes. Save or revert it first.")
    } else {
        None
    }
}

/// The footer chip: `⑂ main ↓2 ↑1`. Clicking opens the menu.
pub fn chip(app: &mut StudioApp, ui: &mut Ui) {
    let Some(ws) = &app.ws else { return };
    if !ws.is_writable() {
        return;
    }
    let Some(Ok(status)) = &app.git_status else { return };
    let mut text = format!("{BRANCH}  {}", status.branch);
    if status.behind > 0 {
        text += &format!("  ↓{}", status.behind);
    }
    if status.ahead > 0 {
        text += &format!("  ↑{}", status.ahead);
    }
    if status.upstream.is_none() {
        text += "  (not published)";
    }
    let busy = git_busy(app);
    ui.separator();
    let resp = ui.selectable_label(app.branch_menu.open, text).on_hover_text("Switch branch, create one, or fetch, pull and push the development workspace.");
    if busy {
        ui.spinner();
    }
    app.branch_menu.anchor = Some(resp.rect);
    if resp.clicked() {
        let root = ws.root.clone();
        let menu = &mut app.branch_menu;
        menu.open = !menu.open;
        if menu.open {
            menu.query.clear();
            menu.focus_query = true;
            menu.load(&root);
        }
    }
}

/// The menu, floating above the chip while open.
pub fn menu(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.branch_menu.open {
        return;
    }
    let (Some(ws), Some(anchor)) = (app.ws.clone(), app.branch_menu.anchor) else {
        app.branch_menu.open = false;
        return;
    };
    let Some(Ok(status)) = app.git_status.clone() else {
        app.branch_menu.open = false;
        return;
    };
    let blocker = switch_blocker(app);
    let busy = git_busy(app);
    let mut switch_to: Option<Branch> = None;
    let mut create: Option<String> = None;
    let (mut fetch, mut pull, mut push) = (false, false, false);
    let area = egui::Area::new(egui::Id::unique("branch-menu"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(anchor.left_top() - egui::vec2(0.0, 4.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(380.0);
                let menu = &mut app.branch_menu;
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut menu.query)
                        .hint_text("Search branches, or name a new one")
                        .desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut menu.focus_query) {
                    edit.request_focus();
                }
                let q = menu.query.trim().to_string();
                let ql = q.to_lowercase();
                if let Some(e) = &menu.error {
                    ui.colored_label(theme::red(), e);
                }
                if let Some(why) = blocker {
                    ui.colored_label(theme::amber(), why);
                } else if menu.changed > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} tracked file(s) have uncommitted changes; git refuses to switch if they would be overwritten. Recipes\\Development is not affected.",
                            menu.changed
                        ))
                        .small()
                        .color(theme::amber()),
                    );
                }
                ui.add_space(4.0);
                let matches: Vec<&Branch> = menu.branches.iter().filter(|b| ql.is_empty() || b.name.to_lowercase().contains(&ql)).collect();
                let exact = menu.branches.iter().any(|b| b.name == q || b.name.split_once('/').is_some_and(|(_, n)| n == q));
                if !q.is_empty() && !exact {
                    let valid = git::is_valid_branch_name(&q);
                    let label = format!("+  Create branch {q} from {}", status.branch);
                    let resp = ui.add_enabled(valid && blocker.is_none(), egui::Button::new(label).frame(false));
                    let resp = if valid { resp.on_hover_text("git switch -c: a new branch from the current commit.") } else { resp.on_disabled_hover_text("Not a valid branch name: no spaces, and none of ~ ^ : ? * [ \\ or '..'.") };
                    if resp.clicked() || (valid && blocker.is_none() && matches.is_empty() && edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        create = Some(q.clone());
                    }
                    ui.separator();
                }
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    let mut section = None;
                    for b in &matches {
                        let s = if b.remote { "Remote branches" } else { "Branches" };
                        if section != Some(s) {
                            ui.label(RichText::new(s).small().weak());
                            section = Some(s);
                        }
                        let mark = if b.current { "\u{E73E}" } else { "   " };
                        let detail = match (&b.upstream, b.remote) {
                            (_, true) => "creates a local branch tracking it".to_string(),
                            (Some(u), false) => u.clone(),
                            (None, false) => "not published".to_string(),
                        };
                        // The whole row is the target: name on the left, detail on the right.
                        let row = egui::Button::selectable(b.current, RichText::new(format!("{mark}  {}", b.name)).monospace())
                            .right_text(RichText::new(detail).small().weak())
                            .min_size(egui::vec2(ui.available_width(), 0.0));
                        if ui.add_enabled(!b.current && blocker.is_none(), row).clicked() {
                            switch_to = Some((*b).clone());
                        }
                    }
                    if matches.is_empty() && q.is_empty() {
                        ui.label(RichText::new("No branches found.").weak());
                    }
                });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("\u{E895}  Fetch")).on_hover_text("git fetch --prune").clicked() {
                        fetch = true;
                    }
                    let pull_label = if status.behind > 0 { format!("\u{E896}  Pull {}", status.behind) } else { "\u{E896}  Pull".to_string() };
                    if ui
                        .add_enabled(!busy && status.upstream.is_some() && blocker.is_none(), egui::Button::new(pull_label))
                        .on_hover_text("git pull --ff-only: never merges or rebases your commits.")
                        .clicked()
                    {
                        pull = true;
                    }
                    let (push_label, push_hover) = if status.upstream.is_none() {
                        ("\u{E898}  Publish branch".to_string(), format!("git push -u origin {}", status.branch))
                    } else {
                        (
                            if status.ahead > 0 { format!("\u{E898}  Push {}", status.ahead) } else { "\u{E898}  Push".to_string() },
                            format!("git push to {}", status.upstream.as_deref().unwrap_or_default()),
                        )
                    };
                    if ui
                        .add_enabled(!busy && (status.ahead > 0 || status.upstream.is_none()), egui::Button::new(push_label))
                        .on_hover_text(push_hover)
                        .clicked()
                    {
                        push = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    menu.open = false;
                }
            });
        });
    // A click outside the menu and the chip closes it.
    let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !anchor.contains(p))
    });
    if clicked_outside {
        app.branch_menu.open = false;
    }
    let start = |app: &mut StudioApp, spec: anyhow::Result<ystudio_core::process::ProcessSpec>, kind: JobKind| match spec {
        Ok(spec) => {
            app.branch_menu.open = false;
            app.start_job(spec, kind);
        }
        Err(e) => app.branch_menu.error = Some(format!("{e:#}")),
    };
    if let Some(b) = switch_to {
        start(app, git::switch(&ws, &b), JobKind::GitSwitch { branch: b.name.split_once('/').filter(|_| b.remote).map_or(b.name.clone(), |(_, n)| n.to_string()) });
    } else if let Some(name) = create {
        start(app, git::create_branch(&ws, &name), JobKind::GitSwitch { branch: name });
    } else if fetch {
        app.branch_menu.open = false;
        app.start_git_fetch(true);
    } else if pull {
        app.branch_menu.open = false;
        app.start_git_pull();
    } else if push {
        start(app, git::push(&ws, &status), JobKind::GitPush);
    }
}

/// A switch (or create) finished: reload what depends on the checkout.
pub fn switched(app: &mut StudioApp, branch: &str, ok: bool, job: u64) {
    app.refresh_git_status();
    if ok {
        app.refresh_recipes();
        app.reload_clean_recipe();
        app.notify(format!("Switched the workspace to {branch}."));
    } else {
        app.console_job = Some(job);
        app.console_open = true;
        app.error(format!("Could not switch to {branch} - see the console."));
    }
}

pub fn pushed(app: &mut StudioApp, ok: bool, job: u64) {
    app.refresh_git_status();
    if ok {
        let branch = app.git_status.as_ref().and_then(|s| s.as_ref().ok()).map(|s| s.branch.clone()).unwrap_or_default();
        app.notify(format!("Pushed {branch}."));
    } else {
        app.console_job = Some(job);
        app.console_open = true;
        app.error("git push failed - see the console.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::{fake_app, render};

    fn git(root: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=Studio Test", "-c", "user.email=test@example.invalid", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn finish(app: &mut StudioApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while app.jobs.iter().any(|j| j.proc.is_running()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            app.poll_for_tests();
        }
        app.poll_for_tests();
    }

    #[test]
    fn the_branch_menu_creates_and_switches_branches() {
        if ystudio_core::process::which("git").is_none() {
            return;
        }
        let (dir, mut app) = fake_app();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join(".gitignore"), "Recipes/Development/\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        git(&root, &["branch", "recipe/brave"]);
        app.refresh_git_status();
        assert_eq!(app.git_status.as_ref().unwrap().as_ref().unwrap().branch, "main");

        app.branch_menu.open = true;
        app.branch_menu.load(&root);
        app.branch_menu.anchor = Some(egui::Rect::from_min_size(egui::pos2(420.0, 790.0), egui::vec2(120.0, 20.0)));
        assert_eq!(app.branch_menu.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["main", "recipe/brave"]);
        render(&mut app, "branch-menu", 1200, 820);

        // An unsaved recipe blocks switching.
        assert!(app.open_recipe_by_id("app"));
        app.recipe.as_mut().unwrap().text.push_str("# edit\n");
        app.recipe.as_mut().unwrap().changed();
        assert!(switch_blocker(&app).is_some_and(|w| w.contains("unsaved")));
        app.recipe.as_mut().unwrap().text = app.recipe.as_ref().unwrap().saved.clone();
        app.recipe.as_mut().unwrap().changed();
        assert_eq!(switch_blocker(&app), None);

        let ws = app.ws.clone().unwrap();
        app.start_job(git::create_branch(&ws, "studio/new").unwrap(), JobKind::GitSwitch { branch: "studio/new".into() });
        finish(&mut app);
        let s = app.git_status.as_ref().unwrap().as_ref().unwrap();
        assert_eq!((s.branch.as_str(), s.upstream.as_deref()), ("studio/new", None));
        let main = Branch { name: "main".into(), remote: false, current: false, upstream: None };
        app.start_job(git::switch(&ws, &main).unwrap(), JobKind::GitSwitch { branch: "main".into() });
        finish(&mut app);
        assert_eq!(app.git_status.as_ref().unwrap().as_ref().unwrap().branch, "main");
        assert!(app.recipe.is_some(), "the open recipe survives a switch");
    }
}
