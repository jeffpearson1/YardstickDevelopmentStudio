//! Locating a Yardstick folder, resolving its layout, and deciding whether the
//! Studio may write to it.
//!
//! The Yardstick recipe skill is explicit that a folder which runs on a schedule
//! against the tenant is *production* and must never receive experiments. The
//! Studio therefore classifies every folder before it enables any action that
//! writes files or runs the harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::{read_text, same_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceKind {
    /// `developmentOnly: true` in `Local\preferences.yaml`, or a folder the user
    /// explicitly trusted as a development clone.
    Development,
    /// A fresh clone with no `Local\preferences.yaml` yet. Safe to write to, but
    /// the harness needs `Install-Yardstick.ps1 -DevelopmentOnly` first.
    Unconfigured,
    /// Read-only in the Studio. `reason` explains which rule matched.
    Production { reason: String },
}

impl WorkspaceKind {
    pub fn is_writable(&self) -> bool {
        !matches!(self, WorkspaceKind::Production { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            WorkspaceKind::Development => "Development",
            WorkspaceKind::Unconfigured => "Development (not set up)",
            WorkspaceKind::Production { .. } => "Production (read-only)",
        }
    }
}

/// Paths from `Yardstick.Project.psd1`, resolved against the workspace root.
#[derive(Debug, Clone)]
pub struct Layout {
    pub recipes: PathBuf,
    pub development_recipes: PathBuf,
    pub icons: PathBuf,
    pub harness: PathBuf,
    pub artifacts: PathBuf,
    pub harness_artifacts: PathBuf,
    pub preferences: PathBuf,
    pub recipe_overrides: PathBuf,
    pub docs: PathBuf,
    pub templates: PathBuf,
    pub skill: PathBuf,
}

impl Layout {
    fn resolve(root: &Path, psd1: &BTreeMap<String, String>) -> Self {
        let get = |key: &str, default: &str| {
            let rel = psd1.get(key).map(String::as_str).unwrap_or(default);
            if rel == "." { root.to_path_buf() } else { root.join(rel) }
        };
        Layout {
            recipes: get("RecipesPath", "Recipes"),
            development_recipes: get("DevelopmentRecipes", r"Recipes\Development"),
            icons: get("IconsPath", "Icons"),
            harness: get("HarnessPath", r"Tests\RecipeHarness"),
            artifacts: get("ArtifactsPath", "Artifacts"),
            harness_artifacts: get("HarnessArtifacts", r"Artifacts\RecipeHarness"),
            preferences: get("Preferences", r"Local\preferences.yaml"),
            recipe_overrides: get("RecipeOverrides", r"Local\recipeOverrides.yaml"),
            docs: get("Documentation", "Docs"),
            templates: root.join("Templates"),
            skill: root.join(r".claude\skills\yardstick-recipes"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecipeLocation {
    /// `Recipes\Development` - Git-ignored, where new work happens.
    Development,
    /// A shipped recipe, with the sub-folder name if it is not the top level.
    Shipped(Option<String>),
}

#[derive(Debug, Clone)]
pub struct RecipeEntry {
    pub id: String,
    pub path: PathBuf,
    pub location: RecipeLocation,
    pub groups: Vec<String>,
    pub meta: RecipeMeta,
}

/// The few top-level recipe fields the library, batch matcher and icon engine
/// need, with `base:` inheritance applied. Read with a line regex, not a YAML
/// parse, so listing 250+ recipes stays cheap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecipeMeta {
    pub display_name: String,
    pub publisher: String,
    pub icon_file: String,
    /// The `url` field: usually the vendor's download page.
    pub url: String,
    pub base: String,
}

impl RecipeMeta {
    pub fn parse(text: &str) -> Self {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"(?m)^(displayName|publisher|iconFile|url|base):[ \t]*(.*?)[ \t]*\r?$").unwrap());
        let mut m = RecipeMeta::default();
        for c in re.captures_iter(text) {
            let raw = c[2].trim();
            let value = if raw.len() >= 2 && ((raw.starts_with('"') && raw.ends_with('"')) || (raw.starts_with('\'') && raw.ends_with('\''))) {
                raw[1..raw.len() - 1].to_string()
            } else {
                raw.split(" #").next().unwrap_or(raw).trim().to_string()
            };
            let slot = match &c[1] {
                "displayName" => &mut m.display_name,
                "publisher" => &mut m.publisher,
                "iconFile" => &mut m.icon_file,
                "url" => &mut m.url,
                _ => &mut m.base,
            };
            if slot.is_empty() {
                *slot = value;
            }
        }
        m
    }

    fn inherit(&mut self, base: &RecipeMeta) {
        for (mine, theirs) in [
            (&mut self.display_name, &base.display_name),
            (&mut self.publisher, &base.publisher),
            (&mut self.icon_file, &base.icon_file),
            (&mut self.url, &base.url),
        ] {
            if mine.is_empty() {
                mine.clone_from(theirs);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub layout: Layout,
    pub kind: WorkspaceKind,
    pub development_only: bool,
}

/// Parses the flat `@{ Key = 'value' }` hashtable in `Yardstick.Project.psd1`.
pub fn parse_project_psd1(text: &str) -> BTreeMap<String, String> {
    let re = Regex::new(r#"(?m)^\s*(\w+)\s*=\s*['"]([^'"]*)['"]"#).unwrap();
    re.captures_iter(text).map(|c| (c[1].to_string(), c[2].to_string())).collect()
}

pub fn is_yardstick_root(path: &Path) -> bool {
    path.join("Yardstick.Project.psd1").is_file() && path.join("Yardstick.ps1").is_file()
}

impl Workspace {
    /// Opens `root` and classifies it. `production_paths` are folders the user
    /// has declared as production; `trusted_dev_paths` override the heuristic for
    /// full (non development-only) installs the user has confirmed are scratch.
    pub fn open(root: &Path, production_paths: &[PathBuf], trusted_dev_paths: &[PathBuf]) -> Result<Self> {
        if !is_yardstick_root(root) {
            bail!(
                "'{}' is not a Yardstick folder (Yardstick.Project.psd1 and Yardstick.ps1 not found).",
                root.display()
            );
        }
        let psd1 = read_text(&root.join("Yardstick.Project.psd1")).context("reading Yardstick.Project.psd1")?;
        let layout = Layout::resolve(root, &parse_project_psd1(&psd1));
        let prefs = read_preferences(&layout.preferences);
        let development_only = prefs
            .as_ref()
            .and_then(|p| p.get("developmentOnly"))
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false);

        let kind = classify(root, prefs.is_some(), development_only, production_paths, trusted_dev_paths);
        Ok(Workspace { root: root.to_path_buf(), layout, kind, development_only })
    }

    pub fn is_writable(&self) -> bool {
        self.kind.is_writable()
    }

    /// Errors with a user-facing explanation unless the workspace is writable.
    pub fn ensure_writable(&self, action: &str) -> Result<()> {
        if let WorkspaceKind::Production { reason } = &self.kind {
            bail!(
                "Refusing to {action} in '{}': it is a production Yardstick folder ({reason}). \
                 Work in a development clone and promote with Publish-Yardstick.ps1.",
                self.root.display()
            );
        }
        Ok(())
    }

    pub fn has_skill(&self) -> bool {
        self.layout.skill.join("SKILL.md").is_file()
    }

    pub fn development_recipe_path(&self, id: &str) -> PathBuf {
        self.layout.development_recipes.join(format!("{id}.yaml"))
    }

    pub fn icon_exists(&self, icon_file: &str) -> bool {
        !icon_file.trim().is_empty() && self.layout.icons.join(icon_file.trim()).is_file()
    }

    pub fn list_icons(&self) -> Vec<String> {
        let mut icons: Vec<String> = std::fs::read_dir(&self.layout.icons)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                let n = n.to_lowercase();
                n.ends_with(".png") || n.ends_with(".jpg") || n.ends_with(".jpeg")
            })
            .collect();
        icons.sort_by_key(|n| n.to_lowercase());
        icons
    }

    /// Every `*.yaml` recipe under `Recipes\`, development recipes included.
    pub fn list_recipes(&self) -> Vec<RecipeEntry> {
        let groups = self.recipe_groups();
        let mut out = Vec::new();
        collect_yaml(&self.layout.recipes, &mut |path| {
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { return };
            let location = if path.starts_with(&self.layout.development_recipes) {
                RecipeLocation::Development
            } else {
                let parent = path.parent().unwrap_or(&self.layout.recipes);
                if same_path(parent, &self.layout.recipes) {
                    RecipeLocation::Shipped(None)
                } else {
                    RecipeLocation::Shipped(parent.file_name().map(|n| n.to_string_lossy().into_owned()))
                }
            };
            let member_of = groups
                .iter()
                .filter(|(_, ids)| ids.iter().any(|g| g.eq_ignore_ascii_case(&id)))
                .map(|(g, _)| g.clone())
                .collect();
            let meta = read_text(path).map(|t| RecipeMeta::parse(&t)).unwrap_or_default();
            out.push(RecipeEntry { id, path: path.to_path_buf(), location, groups: member_of, meta });
        });
        out.sort_by(|a, b| a.location.cmp(&b.location).then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase())));
        // `base:` names another recipe file; development copies are listed first,
        // so the first match is the one Yardstick would merge with.
        let metas: Vec<(String, RecipeMeta)> = out.iter().map(|r| (r.id.to_lowercase(), r.meta.clone())).collect();
        for r in &mut out {
            let mut base = r.meta.base.trim().trim_end_matches(".yaml").to_lowercase();
            for _ in 0..4 {
                if base.is_empty() || base == r.id.to_lowercase() {
                    break;
                }
                let Some((_, b)) = metas.iter().find(|(id, _)| *id == base) else { break };
                r.meta.inherit(b);
                base = b.base.trim().trim_end_matches(".yaml").to_lowercase();
            }
        }
        out
    }

    /// `RecipeGroups.yaml`: group name -> recipe ids.
    pub fn recipe_groups(&self) -> BTreeMap<String, Vec<String>> {
        let Ok(text) = read_text(&self.root.join("RecipeGroups.yaml")) else { return BTreeMap::new() };
        let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
            return BTreeMap::new();
        };
        map.into_iter()
            .filter_map(|(k, v)| {
                let name = k.as_str()?.to_string();
                let ids = v
                    .as_sequence()
                    .map(|s| s.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                Some((name, ids))
            })
            .collect()
    }

    /// Finds a recipe by id, preferring the development copy.
    pub fn find_recipe(&self, id: &str) -> Option<PathBuf> {
        let dev = self.development_recipe_path(id);
        if dev.is_file() {
            return Some(dev);
        }
        self.list_recipes().into_iter().find(|r| r.id == id).map(|r| r.path)
    }

    pub fn templates(&self) -> Vec<PathBuf> {
        let mut t: Vec<PathBuf> = std::fs::read_dir(&self.layout.templates)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("yaml")))
            .collect();
        t.sort();
        t
    }

    /// Markdown documents worth surfacing in the Docs view.
    pub fn documents(&self) -> Vec<(String, PathBuf)> {
        let mut docs = Vec::new();
        let mut push = |label: &str, path: PathBuf| {
            if path.is_file() {
                docs.push((label.to_string(), path));
            }
        };
        push("Recipe skill (SKILL.md)", self.layout.skill.join("SKILL.md"));
        push("Skill: field reference", self.layout.skill.join(r"references\fields.md"));
        push("Skill: installer patterns", self.layout.skill.join(r"references\installer-patterns.md"));
        push("Skill: test harness", self.layout.skill.join(r"references\test-harness.md"));
        push("Writing recipes", self.layout.docs.join("WritingRecipes.md"));
        push("Software dropbox", self.layout.docs.join("SoftwareDropbox.md"));
        push("Installation", self.layout.docs.join("Installation.md"));
        push("Harness README", self.layout.harness.join("README.md"));
        push("Yardstick README", self.root.join("README.md"));
        push("Changelog", self.root.join("CHANGELOG.md"));
        docs
    }
}

fn read_preferences(path: &Path) -> Option<serde_yaml::Mapping> {
    let text = read_text(path).ok()?;
    match serde_yaml::from_str::<serde_yaml::Value>(&text) {
        Ok(serde_yaml::Value::Mapping(m)) => Some(m),
        // An empty or comment-only file is still a configured instance.
        _ => Some(serde_yaml::Mapping::new()),
    }
}

fn classify(
    root: &Path,
    has_prefs: bool,
    development_only: bool,
    production_paths: &[PathBuf],
    trusted_dev_paths: &[PathBuf],
) -> WorkspaceKind {
    if production_paths.iter().any(|p| same_path(p, root)) {
        return WorkspaceKind::Production { reason: "listed as a production path in Studio settings".into() };
    }
    if development_only {
        return WorkspaceKind::Development;
    }
    if trusted_dev_paths.iter().any(|p| same_path(p, root)) {
        return WorkspaceKind::Development;
    }
    if has_prefs {
        return WorkspaceKind::Production {
            reason: "Local\\preferences.yaml exists without developmentOnly: true, so this is a full Intune-connected install"
                .into(),
        };
    }
    WorkspaceKind::Unconfigured
}

fn collect_yaml(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_yaml(&path, f);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("yaml")) {
            f(&path);
        }
    }
}

/// Finds a sensible default Yardstick folder to open on first launch: a sibling
/// development clone if there is one, otherwise the production folder (read-only).
pub fn discover_default(production: &Path) -> Option<PathBuf> {
    let parent = production.parent()?;
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_yardstick_root(p) && !same_path(p, production))
        .collect();
    candidates.sort();
    candidates.into_iter().next().or_else(|| is_yardstick_root(production).then(|| production.to_path_buf()))
}

#[cfg(test)]
pub(crate) use tests::fake_workspace;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn fake_workspace(prefs: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Yardstick.Project.psd1"),
            "@{\n    RuntimeRoot = '.'\n    RecipesPath = 'Recipes'\n    DevelopmentRecipes = 'Recipes\\Development'\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# stub").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Jetbrains")).unwrap();
        std::fs::create_dir_all(root.join("Icons")).unwrap();
        std::fs::write(root.join(r"Recipes\7zip.yaml"), "id: 7zip\n").unwrap();
        std::fs::write(root.join(r"Recipes\Jetbrains\clion.yaml"), "id: clion\n").unwrap();
        std::fs::write(root.join(r"Recipes\Development\brave.yaml"), "id: brave\n").unwrap();
        std::fs::write(root.join("RecipeGroups.yaml"), "Development:\n  - brave\nCore:\n  - 7zip\n").unwrap();
        std::fs::write(root.join(r"Icons\7zip.png"), [0u8]).unwrap();
        if let Some(p) = prefs {
            std::fs::create_dir_all(root.join("Local")).unwrap();
            std::fs::write(root.join(r"Local\preferences.yaml"), p).unwrap();
        }
        dir
    }

    #[test]
    fn parses_psd1() {
        let m = parse_project_psd1("@{\n  RecipesPath = 'Recipes'\n  HarnessPath = \"Tests\\RecipeHarness\"\n}");
        assert_eq!(m["RecipesPath"], "Recipes");
        assert_eq!(m["HarnessPath"], r"Tests\RecipeHarness");
    }

    #[test]
    fn classifies_unconfigured_clone() {
        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert_eq!(ws.kind, WorkspaceKind::Unconfigured);
        assert!(ws.is_writable());
    }

    #[test]
    fn classifies_development_only() {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert_eq!(ws.kind, WorkspaceKind::Development);
    }

    #[test]
    fn full_install_is_production_unless_trusted() {
        let d = fake_workspace(Some("defaultScopeTags: []\n"));
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert!(!ws.is_writable());
        assert!(ws.ensure_writable("save").is_err());
        let ws = Workspace::open(d.path(), &[], &[d.path().to_path_buf()]).unwrap();
        assert!(ws.is_writable());
    }

    #[test]
    fn listed_production_path_wins_over_development_only() {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(matches!(ws.kind, WorkspaceKind::Production { .. }));
    }

    #[test]
    fn lists_recipes_with_locations_and_groups() {
        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let recipes = ws.list_recipes();
        assert_eq!(recipes.len(), 3);
        assert_eq!(recipes[0].id, "brave");
        assert_eq!(recipes[0].location, RecipeLocation::Development);
        assert_eq!(recipes[0].groups, vec!["Development".to_string()]);
        assert!(recipes.iter().any(|r| r.id == "clion" && r.location == RecipeLocation::Shipped(Some("Jetbrains".into()))));
        assert!(ws.icon_exists("7zip.png"));
        assert!(!ws.icon_exists("nope.png"));
        assert_eq!(ws.find_recipe("brave").unwrap(), ws.development_recipe_path("brave"));
    }

    #[test]
    fn reads_meta_with_base_inheritance() {
        let d = fake_workspace(None);
        std::fs::write(
            d.path().join(r"Recipes\7zip.yaml"),
            "id: 7zip\ndisplayName: \"7-Zip\"\npublisher: Igor Pavlov # vendor\niconFile: 7zip.png\nurl: https://7-zip.org\ninstallScript: |\n  displayName: nested\n",
        )
        .unwrap();
        std::fs::write(d.path().join(r"Recipes\Development\brave.yaml"), "id: brave\nbase: 7zip\ndisplayName: Brave\n").unwrap();
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let recipes = ws.list_recipes();
        let seven = recipes.iter().find(|r| r.id == "7zip").unwrap();
        assert_eq!(seven.meta.display_name, "7-Zip");
        assert_eq!(seven.meta.publisher, "Igor Pavlov");
        let brave = recipes.iter().find(|r| r.id == "brave").unwrap();
        assert_eq!(brave.meta.display_name, "Brave");
        assert_eq!(brave.meta.icon_file, "7zip.png");
        assert_eq!(brave.meta.url, "https://7-zip.org");
    }

    #[test]
    fn rejects_non_yardstick_folder() {
        let d = tempfile::tempdir().unwrap();
        assert!(Workspace::open(d.path(), &[], &[]).is_err());
    }
}
