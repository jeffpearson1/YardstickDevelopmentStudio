//! Instant, in-process recipe checks.
//!
//! Ports the rules of `Test-RecipeSchema` (Modules\YardstickSupport.psm1) so the
//! editor can show problems as you type, and adds authoring checks taken from
//! the yardstick-recipes skill. The PowerShell validator stays authoritative:
//! the Test Bench still runs it before anything else.

use regex::Regex;

use crate::fields::{self, ALWAYS_REQUIRED, PLACEHOLDER_FIELDS, PLACEHOLDERS, RESERVED_VARIABLES, RETIRED_FIELDS, VALIDATED_ENUMS};
use crate::recipe::{RecipeDoc, scalar_to_string};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub field: Option<String>,
    pub message: String,
    /// True for rules the PowerShell validator also enforces.
    pub from_validator: bool,
}

impl Diagnostic {
    fn new(severity: Severity, field: Option<&str>, message: impl Into<String>, from_validator: bool) -> Self {
        Self { severity, field: field.map(str::to_string), message: message.into(), from_validator }
    }
}

pub struct LintContext<'a> {
    /// The file stem, which `id` must equal.
    pub file_id: Option<&'a str>,
    /// Returns true when an icon file exists in Icons\.
    pub icon_exists: Option<&'a dyn Fn(&str) -> bool>,
    /// The `base:` recipe, if this recipe inherits one and it was found.
    pub base: Option<&'a RecipeDoc>,
    /// True for recipes under Recipes\Development (scopeTags rule).
    pub shared_recipe: bool,
}

impl Default for LintContext<'_> {
    fn default() -> Self {
        Self { file_id: None, icon_exists: None, base: None, shared_recipe: true }
    }
}

/// Lints YAML text. Parse failures become a single error.
pub fn lint_text(text: &str, ctx: &LintContext) -> Vec<Diagnostic> {
    match RecipeDoc::parse(text) {
        Ok(doc) => lint(&doc, ctx),
        Err(e) => vec![Diagnostic::new(Severity::Error, None, format!("{e:#}"), true)],
    }
}

pub fn lint(own: &RecipeDoc, ctx: &LintContext) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let merged;
    let doc = match ctx.base {
        Some(base) => {
            merged = own.merged_with_base(base);
            &merged
        }
        None => {
            if own.is_set("base") {
                out.push(Diagnostic::new(
                    Severity::Hint,
                    Some("base"),
                    "Base recipe not found here, so inherited fields could not be checked.",
                    false,
                ));
            }
            own
        }
    };

    let script = |k: &str| doc.get_str(k).unwrap_or_default();
    let download_scripts = [script("preDownloadScript"), script("downloadScript"), script("postDownloadScript")].join("\n");
    let assigned_in_scripts =
        |var: &str| Regex::new(&format!(r"(?i)\${}\s*=", regex::escape(var))).unwrap().is_match(&download_scripts);
    let base_unresolved = own.is_set("base") && ctx.base.is_none();

    // Reserved runner variables
    let reserved = Regex::new(&format!(r"(?im)^\s*\$({})\s*=", RESERVED_VARIABLES.join("|"))).unwrap();
    for field in ["preDownloadScript", "downloadScript", "postDownloadScript"] {
        for cap in reserved.captures_iter(&script(field)) {
            out.push(Diagnostic::new(
                Severity::Warning,
                Some(field),
                format!("Assigns reserved runner variable '${}' - it overwrites Yardstick's own folder variable. Rename it.", &cap[1]),
                true,
            ));
        }
    }

    // Direct GitHub calls
    let direct_call = Regex::new(r"(?im)^.*\b(Invoke-RestMethod|Invoke-WebRequest|irm|iwr|curl(\.exe)?)\b.*").unwrap();
    for (host, helper) in [("api.github.com", "Invoke-YardstickGitHubApi"), ("raw.githubusercontent.com", "Invoke-YardstickGitHubRaw")] {
        let mut refs = vec![regex::escape(host)];
        if script("url").contains(host) {
            refs.push(r"\$url\b".into());
        }
        let var_assign = Regex::new(&format!(r#"(?im)\$(\w+)\s*=\s*["'][^"'\r\n]*{}"#, regex::escape(host))).unwrap();
        for c in var_assign.captures_iter(&download_scripts) {
            refs.push(format!(r"\${}\b", regex::escape(&c[1])));
        }
        let target = Regex::new(&format!("(?i)({})", refs.join("|"))).unwrap();
        for field in ["preDownloadScript", "downloadScript", "postDownloadScript"] {
            if direct_call.find_iter(&script(field)).any(|m| target.is_match(m.as_str())) {
                out.push(Diagnostic::new(
                    Severity::Warning,
                    Some(field),
                    format!("Calls {host} directly. Use {helper} so the stored token and rate limits are respected."),
                    true,
                ));
            }
        }
    }

    // Always-required fields
    if !base_unresolved {
        for field in ALWAYS_REQUIRED {
            if !doc.is_set(field) && !assigned_in_scripts(field) {
                out.push(Diagnostic::new(Severity::Error, Some(field), format!("Missing required field '{field}'"), true));
            }
        }
        let has = |k: &str| doc.get_ci(k).is_some() || assigned_in_scripts(k);
        if !has("installScript") && !has("powerShellInstallScript") {
            out.push(Diagnostic::new(
                Severity::Error,
                Some("installScript"),
                "Missing install script: provide installScript or powerShellInstallScript",
                true,
            ));
        }
        if !has("uninstallScript") && !has("powerShellUninstallScript") {
            out.push(Diagnostic::new(
                Severity::Error,
                Some("uninstallScript"),
                "Missing uninstall script: provide uninstallScript or powerShellUninstallScript",
                true,
            ));
        }
    }

    // Enumerations
    for (field, allowed) in VALIDATED_ENUMS {
        if let Some(v) = doc.get_str(field).filter(|v| !v.trim().is_empty())
            && !allowed.iter().any(|a| a.eq_ignore_ascii_case(v.trim()))
        {
            out.push(Diagnostic::new(
                Severity::Error,
                Some(field),
                format!("Invalid value '{v}' for '{field}'. Allowed: {}", allowed.join(", ")),
                true,
            ));
        }
    }

    // Detection-type requirements
    let detection_type = doc.get_str("detectionType").unwrap_or_default().trim().to_ascii_lowercase();
    for field in fields::detection_requirements(&detection_type) {
        if !doc.is_set(field) && !assigned_in_scripts(field) {
            out.push(Diagnostic::new(
                Severity::Error,
                Some(field),
                format!("Missing required field '{field}' for detectionType '{detection_type}'"),
                true,
            ));
        }
    }

    // additionalDetectionRules
    if let Some(rules) = doc.get_ci("additionalDetectionRules").filter(|v| !v.is_null()) {
        if detection_type == "script" {
            out.push(Diagnostic::new(
                Severity::Error,
                Some("additionalDetectionRules"),
                "additionalDetectionRules cannot be combined with a script detection rule",
                true,
            ));
        }
        match rules.as_sequence() {
            None => out.push(Diagnostic::new(
                Severity::Error,
                Some("additionalDetectionRules"),
                "additionalDetectionRules must be a list of rules",
                true,
            )),
            Some(seq) => {
                for (i, rule) in seq.iter().enumerate() {
                    let t = rule.get("detectionType").and_then(scalar_to_string).unwrap_or_default().to_ascii_lowercase();
                    if t != "file" && t != "registry" {
                        out.push(Diagnostic::new(
                            Severity::Error,
                            Some("additionalDetectionRules"),
                            format!("Rule {} must have detectionType file or registry (got '{t}')", i + 1),
                            true,
                        ));
                    }
                }
            }
        }
    }

    // Unknown, retired and miscased fields (on the recipe's own keys)
    for key in own.keys() {
        let lower = key.to_ascii_lowercase();
        if let Some((_, msg)) = RETIRED_FIELDS.iter().find(|(k, _)| *k == lower) {
            out.push(Diagnostic::new(Severity::Warning, Some(key), *msg, true));
        } else if let Some(spec) = fields::spec_ci(key) {
            if spec.name != key {
                out.push(Diagnostic::new(
                    Severity::Warning,
                    Some(key),
                    format!("Field '{key}' has incorrect casing - expected '{}'", spec.name),
                    true,
                ));
            }
        } else {
            out.push(Diagnostic::new(
                Severity::Warning,
                Some(key),
                format!("Unknown field '{key}' - not used by Yardstick, possibly a typo"),
                true,
            ));
        }
    }

    // id / file name
    if let (Some(file_id), Some(id)) = (ctx.file_id, doc.get_str("id"))
        && id != file_id
    {
        out.push(Diagnostic::new(
            Severity::Error,
            Some("id"),
            format!("id '{id}' must match the file name '{file_id}' exactly (case-sensitive)"),
            true,
        ));
    }
    if let Some(id) = doc.get_str("id").filter(|s| !s.is_empty())
        && !crate::recipe::is_valid_id(&id)
    {
        out.push(Diagnostic::new(Severity::Warning, Some("id"), "id should be lowercase letters, digits, '.', '-' or '_'", false));
    }

    // Icon
    if let (Some(exists), Some(icon)) = (ctx.icon_exists, doc.get_str("iconFile").filter(|s| !s.trim().is_empty()))
        && !exists(&icon)
    {
        out.push(Diagnostic::new(
            Severity::Error,
            Some("iconFile"),
            format!("Icon '{icon}' not found in Icons\\. Extract one with Export-ExeIcon.ps1 or Get-YardstickIcon.ps1."),
            true,
        ));
    }

    // ---- Skill-derived authoring checks ----

    let version_assign = Regex::new(r"(?im)^\s*\$version\s*=").unwrap();
    let version_in_pre = version_assign.is_match(&script("preDownloadScript"));
    if !doc.is_set("version") && !version_in_pre && !base_unresolved {
        let late = version_assign.is_match(&script("downloadScript")) || version_assign.is_match(&script("postDownloadScript"));
        let msg = if late {
            "$version is assigned in downloadScript/postDownloadScript - too late. Version is validated right after preDownloadScript; set it there."
        } else {
            "No version source: set `version:` or assign $version in preDownloadScript."
        };
        out.push(Diagnostic::new(if late { Severity::Error } else { Severity::Warning }, Some("preDownloadScript"), msg, false));
    }

    if !doc.is_set("url") && !doc.is_set("downloadScript") && !assigned_in_scripts("url") && !is_true(doc, "manualDownload") && !base_unresolved {
        out.push(Diagnostic::new(
            Severity::Warning,
            Some("url"),
            "No download source: set url, assign $url, write a downloadScript, or use manualDownload: true.",
            false,
        ));
    }

    if ctx.shared_recipe && own.contains("scopeTags") {
        out.push(Diagnostic::new(
            Severity::Warning,
            Some("scopeTags"),
            "scopeTags are tenant-specific; put them in Local\\recipeOverrides.yaml keyed by recipe id.",
            false,
        ));
    }

    let guid = Regex::new(r"\{?[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\}?").unwrap();
    if detection_type == "msi" && guid.is_match(&script("uninstallScript")) {
        out.push(Diagnostic::new(
            Severity::Warning,
            Some("uninstallScript"),
            "Hard-coded product code. Use <productcode>; it changes with every MSI build.",
            false,
        ));
    }

    let placeholder = Regex::new(r"<([a-z]+)>").unwrap();
    for field in PLACEHOLDER_FIELDS {
        for cap in placeholder.captures_iter(&script(field)) {
            let name = &cap[1];
            if !PLACEHOLDERS.contains(&name) {
                out.push(Diagnostic::new(
                    Severity::Hint,
                    Some(field),
                    format!("'<{name}>' is not a Yardstick placeholder (only <filename>, <version>, <productcode>)."),
                    false,
                ));
            }
        }
    }
    if detection_type != "msi" && !own.is_set("base") {
        for field in PLACEHOLDER_FIELDS {
            if script(field).contains("<productcode>") {
                out.push(Diagnostic::new(
                    Severity::Warning,
                    Some(field),
                    "<productcode> is only populated for MSI payloads; make sure the payload is an MSI.",
                    false,
                ));
            }
        }
    }

    if let Some(lock) = doc.get_str("versionLock").filter(|s| !s.is_empty())
        && !Regex::new(r"^[0-9xX.]+$").unwrap().is_match(&lock)
    {
        out.push(Diagnostic::new(
            Severity::Error,
            Some("versionLock"),
            "versionLock may only contain digits, '.' and 'x'.",
            false,
        ));
    }

    if detection_type == "script" {
        let ds = script("detectionScript");
        if !ds.is_empty() && !(ds.contains("exit 0") || ds.to_lowercase().contains("write-output")) {
            out.push(Diagnostic::new(
                Severity::Hint,
                Some("detectionScript"),
                "Detection scripts should Write-Output 'Detected'; exit 0 on match and exit 1 otherwise.",
                false,
            ));
        }
    }

    if doc.get_ci("urlRedirects").is_some_and(|v| v.is_string()) {
        out.push(Diagnostic::new(Severity::Hint, Some("urlRedirects"), "urlRedirects should be a boolean (true/false).", false));
    }

    out.sort_by_key(|d| d.severity);
    out
}

fn is_true(doc: &RecipeDoc, key: &str) -> bool {
    doc.get_str(key).is_some_and(|s| s.eq_ignore_ascii_case("true"))
}

pub fn count(diags: &[Diagnostic], severity: Severity) -> usize {
    diags.iter().filter(|d| d.severity == severity).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"url: https://example.com/app.msi
urlRedirects: true
preDownloadScript: |
  $Version = "1.2.3"
installScript: msiexec /i <filename> /qn /norestart
uninstallScript: msiexec /x <productcode> /qn /norestart
iconFile: app.png
id: app
displayName: App
publisher: Contoso
description: An app
detectionType: msi
"#;

    fn ctx<'a>(icon: &'a dyn Fn(&str) -> bool) -> LintContext<'a> {
        LintContext { file_id: Some("app"), icon_exists: Some(icon), base: None, shared_recipe: true }
    }

    fn errors(text: &str) -> Vec<String> {
        let icon = |_: &str| true;
        lint_text(text, &ctx(&icon)).into_iter().filter(|d| d.severity == Severity::Error).map(|d| d.message).collect()
    }

    #[test]
    fn clean_recipe_has_no_errors_or_warnings() {
        let icon = |_: &str| true;
        let diags = lint_text(GOOD, &ctx(&icon));
        assert!(diags.iter().all(|d| d.severity == Severity::Hint), "{diags:#?}");
    }

    #[test]
    fn missing_required_and_install() {
        let e = errors("id: app\n");
        assert!(e.iter().any(|m| m.contains("'displayName'")));
        assert!(e.iter().any(|m| m.contains("install script")));
        assert!(e.iter().any(|m| m.contains("uninstall script")));
    }

    #[test]
    fn required_satisfied_by_script_variable() {
        let text = GOOD.replace("publisher: Contoso\n", "").replace("$Version = \"1.2.3\"", "$Version = \"1.2.3\"\n  $publisher = 'X'");
        assert!(errors(&text).is_empty(), "{:?}", errors(&text));
    }

    #[test]
    fn enum_and_detection_requirements() {
        let e = errors(&GOOD.replace("detectionType: msi", "detectionType: file\ninstallExperience: machine"));
        assert!(e.iter().any(|m| m.contains("Invalid value 'machine'")));
        assert!(e.iter().any(|m| m.contains("fileDetectionPath")));
    }

    #[test]
    fn id_must_match_filename() {
        assert!(errors(&GOOD.replace("id: app", "id: App")).iter().any(|m| m.contains("must match")));
    }

    #[test]
    fn version_set_too_late() {
        let text = GOOD.replace("preDownloadScript", "downloadScript");
        assert!(errors(&text).iter().any(|m| m.contains("too late")));
    }

    #[test]
    fn warnings_for_casing_unknown_reserved_github() {
        let icon = |_: &str| true;
        let text = GOOD
            .replace("urlRedirects", "urlredirects")
            .replace("$Version = \"1.2.3\"", "$Version = \"1.2.3\"\n  $temp = 'x'\n  Invoke-RestMethod https://api.github.com/repos/a/b")
            + "colour: blue\nscopeTags:\n  - x\n";
        let msgs: Vec<_> = lint_text(&text, &ctx(&icon)).into_iter().map(|d| d.message).collect();
        let has = |s: &str| msgs.iter().any(|m| m.contains(s));
        assert!(has("incorrect casing"));
        assert!(has("Unknown field 'colour'"));
        assert!(has("reserved runner variable '$temp'"));
        assert!(has("Invoke-YardstickGitHubApi"));
        assert!(has("scopeTags"));
    }

    #[test]
    fn missing_icon_and_hardcoded_guid() {
        let no_icon = |_: &str| false;
        let text = GOOD.replace("<productcode>", "{12345678-1234-1234-1234-123456789012}");
        let msgs: Vec<_> = lint_text(&text, &ctx(&no_icon)).into_iter().map(|d| d.message).collect();
        assert!(msgs.iter().any(|m| m.contains("Icon 'app.png' not found")));
        assert!(msgs.iter().any(|m| m.contains("Hard-coded product code")));
    }

    #[test]
    fn additional_rules_rejected_with_script() {
        let text = GOOD.replace("detectionType: msi", "detectionType: script\ndetectionScript: |\n  exit 0\nadditionalDetectionRules:\n  - detectionType: msi\n");
        let e = errors(&text);
        assert!(e.iter().any(|m| m.contains("cannot be combined")));
        assert!(e.iter().any(|m| m.contains("Rule 1")));
    }

    #[test]
    fn base_inheritance() {
        let base = RecipeDoc::parse(GOOD).unwrap();
        let child = RecipeDoc::parse("base: app\nid: app\n").unwrap();
        let icon = |_: &str| true;
        let with_base = LintContext { base: Some(&base), ..ctx(&icon) };
        assert!(lint(&child, &with_base).iter().all(|d| d.severity != Severity::Error));
        let without = lint(&child, &ctx(&icon));
        assert!(without.iter().all(|d| d.severity != Severity::Error), "{without:#?}");
        assert!(without.iter().any(|d| d.message.contains("Base recipe not found")));
    }

    #[test]
    fn parse_error_reported() {
        let e = errors("id: [unclosed\n");
        assert_eq!(e.len(), 1);
    }
}
