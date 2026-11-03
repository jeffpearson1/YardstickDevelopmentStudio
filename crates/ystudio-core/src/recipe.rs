//! An ordered, editable view of one recipe YAML file.
//!
//! The YAML text is the source of truth. The Designer parses it into a
//! [`RecipeDoc`], edits fields, and writes it back with [`RecipeDoc::to_yaml`],
//! which keeps the original key order, writes scripts as literal blocks, and
//! appends new keys in catalog order. Comments cannot survive a round trip, so
//! callers check [`has_comments`] and warn first.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use crate::fields::{self, FieldKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecipeDoc {
    entries: Vec<(String, Value)>,
}

impl RecipeDoc {
    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let value: Value = serde_yaml::from_str(text).context("recipe is not valid YAML")?;
        match value {
            Value::Mapping(map) => Self::from_mapping(map),
            Value::Null => Ok(Self::default()),
            _ => bail!("a recipe must be a YAML mapping of field: value"),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(&crate::read_text(path)?)
    }

    fn from_mapping(map: Mapping) -> Result<Self> {
        let mut entries = Vec::with_capacity(map.len());
        for (k, v) in map {
            let key = match k {
                Value::String(s) => s,
                other => serde_yaml::to_string(&other)?.trim().to_string(),
            };
            entries.push((key, v));
        }
        Ok(Self { entries })
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Case-insensitive lookup, matching PowerShell hashtable semantics.
    pub fn get_ci(&self, key: &str) -> Option<&Value> {
        self.get(key).or_else(|| self.entries.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v))
    }

    /// The value rendered as a string the way PowerShell would see it.
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get_ci(key).and_then(scalar_to_string)
    }

    /// A field is "set" when present and not blank (the validator's rule).
    pub fn is_set(&self, key: &str) -> bool {
        self.get_str(key).is_some_and(|s| !s.trim().is_empty())
            || self.get_ci(key).is_some_and(|v| matches!(v, Value::Sequence(s) if !s.is_empty()) || v.is_mapping())
    }

    pub fn set(&mut self, key: &str, value: Value) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
            return;
        }
        // Insert after the last existing key that precedes this one canonically,
        // so wizard-added fields land in a sensible place.
        let idx = fields::canonical_index(key);
        let pos = self
            .entries
            .iter()
            .rposition(|(k, _)| fields::canonical_index(k) <= idx)
            .map(|p| p + 1)
            .unwrap_or(if idx == usize::MAX { self.entries.len() } else { 0 });
        self.entries.insert(pos.min(self.entries.len()), (key.to_string(), value));
    }

    /// Sets a field from editor text, choosing the YAML type from the catalog.
    /// Blank text removes the field, so it inherits from preferences.yaml.
    pub fn set_from_text(&mut self, key: &str, text: &str) {
        let kind = fields::spec(key).map(|s| s.kind).unwrap_or(FieldKind::Text);
        if text.trim().is_empty() && !matches!(kind, FieldKind::Script) {
            self.remove(key);
            return;
        }
        if matches!(kind, FieldKind::Script) && text.trim().is_empty() {
            self.remove(key);
            return;
        }
        let value = match kind {
            FieldKind::Bool => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Value::Bool(true),
                "false" | "no" | "0" => Value::Bool(false),
                _ => Value::String(text.to_string()),
            },
            FieldKind::Int => text
                .trim()
                .parse::<i64>()
                .map(|n| Value::Number(n.into()))
                .unwrap_or_else(|_| Value::String(text.to_string())),
            FieldKind::List => Value::Sequence(
                text.lines()
                    .flat_map(|l| l.split(','))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            ),
            FieldKind::Complex => serde_yaml::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())),
            FieldKind::Script => {
                let mut s = text.replace("\r\n", "\n");
                if !s.ends_with('\n') {
                    s.push('\n');
                }
                Value::String(s)
            }
            FieldKind::Text | FieldKind::Enum(_) => Value::String(text.trim_end_matches(['\r', '\n']).to_string()),
        };
        self.set(key, value);
    }

    /// Text for an editor widget (lists one-per-line, complex values as YAML).
    pub fn text_for_editor(&self, key: &str) -> String {
        match self.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::Sequence(seq)) if seq.iter().all(|v| scalar_to_string(v).is_some()) => {
                seq.iter().filter_map(scalar_to_string).collect::<Vec<_>>().join("\n")
            }
            Some(v @ (Value::Sequence(_) | Value::Mapping(_))) => serde_yaml::to_string(v).unwrap_or_default(),
            Some(v) => scalar_to_string(v).unwrap_or_default(),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|(k, _)| k != key);
    }

    /// Fills in fields missing here from a `base:` recipe, like Merge-RecipeWithBase.
    pub fn merged_with_base(&self, base: &RecipeDoc) -> RecipeDoc {
        let mut merged = base.clone();
        merged.remove("base");
        for (k, v) in &self.entries {
            if k == "base" {
                continue;
            }
            merged.set(k, v.clone());
        }
        merged
    }

    pub fn to_yaml(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.entries {
            emit_entry(&mut out, key, value);
        }
        out
    }
}

pub fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Null => Some(String::new()),
        Value::Tagged(t) => scalar_to_string(&t.value),
        _ => None,
    }
}

fn emit_entry(out: &mut String, key: &str, value: &Value) {
    let key_repr = scalar_repr(key);
    match value {
        Value::Null => out.push_str(&format!("{key_repr}:\n")),
        Value::String(s) if s.contains('\n') => emit_block(out, &key_repr, s),
        Value::String(_) | Value::Bool(_) | Value::Number(_) => {
            let text = match value {
                Value::String(s) => scalar_repr(s),
                other => scalar_to_string(other).unwrap_or_default(),
            };
            out.push_str(&format!("{key_repr}: {text}\n"));
        }
        Value::Sequence(seq) if seq.iter().all(|v| matches!(v, Value::String(s) if !s.contains('\n')) || v.is_number() || v.is_bool()) => {
            if seq.is_empty() {
                out.push_str(&format!("{key_repr}: []\n"));
            } else {
                out.push_str(&format!("{key_repr}:\n"));
                for item in seq {
                    let text = match item {
                        Value::String(s) => scalar_repr(s),
                        other => scalar_to_string(other).unwrap_or_default(),
                    };
                    out.push_str(&format!("  - {text}\n"));
                }
            }
        }
        other => {
            let mut map = Mapping::new();
            map.insert(Value::String(key.to_string()), other.clone());
            out.push_str(&serde_yaml::to_string(&map).unwrap_or_default());
        }
    }
}

fn emit_block(out: &mut String, key_repr: &str, s: &str) {
    let s = s.replace("\r\n", "\n");
    let chomp = if s.ends_with('\n') { "" } else { "-" };
    let body = s.strip_suffix('\n').unwrap_or(&s);
    let indent_hint = if body.starts_with(' ') { "2" } else { "" };
    out.push_str(&format!("{key_repr}: |{indent_hint}{chomp}\n"));
    for line in body.split('\n') {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Plain when YAML would read it back unchanged, quoted otherwise.
fn scalar_repr(s: &str) -> String {
    let plain_ok = !s.is_empty()
        && !s.starts_with([' ', '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
        && !s.ends_with([' ', ':'])
        && !s.contains(": ")
        && !s.contains(" #")
        && !s.contains('\t')
        && matches!(serde_yaml::from_str::<Value>(s), Ok(Value::String(ref back)) if back == s);
    if plain_ok {
        return s.to_string();
    }
    serde_yaml::to_string(&Value::String(s.to_string()))
        .map(|t| t.trim_end_matches('\n').to_string())
        .unwrap_or_else(|_| format!("'{}'", s.replace('\'', "''")))
}

/// True when the YAML has comment lines that a structured round trip would drop.
pub fn has_comments(text: &str) -> bool {
    let mut in_block: Option<usize> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if let Some(block_indent) = in_block {
            if trimmed.is_empty() || indent > block_indent {
                continue;
            }
            in_block = None;
        }
        if trimmed.starts_with('#') {
            return true;
        }
        let without_comment = trimmed.split(" #").next().unwrap_or("");
        if without_comment.len() != trimmed.len() && !trimmed.contains('"') && !trimmed.contains('\'') {
            return true;
        }
        if trimmed.trim_end().ends_with('|') || trimmed.trim_end().ends_with("|-") || trimmed.trim_end().ends_with('>') {
            in_block = Some(indent);
        }
    }
    false
}

/// A new development recipe from a template, with identity filled in.
pub fn new_from_template(template_text: &str, id: &str, display_name: &str, publisher: &str) -> Result<RecipeDoc> {
    let mut doc = RecipeDoc::parse(template_text)?;
    doc.set("id", Value::String(id.to_string()));
    if !display_name.is_empty() {
        doc.set("displayName", Value::String(display_name.to_string()));
    }
    if !publisher.is_empty() {
        doc.set("publisher", Value::String(publisher.to_string()));
    }
    if !doc.is_set("iconFile") {
        doc.set("iconFile", Value::String(format!("{id}.png")));
    }
    // Templates carry empty placeholders for optional scripts; drop them so the
    // recipe stays minimal and inherits defaults.
    let empties: Vec<String> = doc
        .entries
        .iter()
        .filter(|(k, v)| {
            let blank = matches!(v, Value::Null) || matches!(v, Value::String(s) if s.trim().is_empty());
            blank && !fields::ALWAYS_REQUIRED.contains(&k.as_str()) && k != "url"
        })
        .map(|(k, _)| k.clone())
        .collect();
    for k in empties {
        doc.remove(&k);
    }
    Ok(doc)
}

/// A recipe id is lowercase letters, digits, dots, dashes and underscores.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Suggests an id from a display name: "Notepad++ 8" -> "notepadplusplus8".
pub fn suggest_id(display_name: &str) -> String {
    display_name
        .replace('+', "plus")
        .replace('#', "sharp")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Sets a top-level scalar with a line edit, so comments and formatting
/// survive. Replaces an existing `key:` line, otherwise inserts after
/// `publisher:` / `displayName:` / `id:`, otherwise appends.
pub fn set_top_level_scalar(text: &str, key: &str, value: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let needs_quotes = value.is_empty()
        || value.contains([':', '#', '\'', '"', '{', '}', '[', ']', ',', '&', '*', '!', '|', '>', '%', '@', '`'])
        || value.starts_with([' ', '-', '?'])
        || value.ends_with(' ');
    let rendered = if needs_quotes { format!("'{}'", value.replace('\'', "''")) } else { value.to_string() };
    let line = format!("{key}: {rendered}");
    let is_key = |l: &str, k: &str| l.strip_prefix(k).is_some_and(|rest| rest.trim_start().starts_with(':'));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(i) = lines.iter().position(|l| is_key(l, key)) {
        lines[i] = line;
    } else if let Some(i) = ["publisher", "displayName", "id"].iter().find_map(|k| lines.iter().position(|l| is_key(l, k))) {
        // Skip a block scalar or continuation that belongs to the anchor key.
        let mut at = i + 1;
        while at < lines.len() && lines[at].starts_with([' ', '\t']) && !lines[at].trim().is_empty() {
            at += 1;
        }
        lines.insert(at, line);
    } else {
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines.push(line);
    }
    let mut out = lines.join(nl);
    if text.ends_with('\n') || text.is_empty() {
        out += nl;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_top_level_scalar_keeps_comments() {
        let t = "# keep me\nid: foo\npublisher: Foo Inc\nurl: x # trailing\n";
        assert_eq!(set_top_level_scalar(t, "iconFile", "foo.png"), "# keep me\nid: foo\npublisher: Foo Inc\niconFile: foo.png\nurl: x # trailing\n");
        let t2 = "id: foo\r\niconFile: old.png\r\nsteps:\r\n  iconFile: nested\r\n";
        assert_eq!(set_top_level_scalar(t2, "iconFile", "new file.png"), "id: foo\r\niconFile: new file.png\r\nsteps:\r\n  iconFile: nested\r\n");
        assert_eq!(set_top_level_scalar("url: x", "iconFile", "a#b.png"), "url: x\niconFile: 'a#b.png'");
        // iconFileX is a different key.
        assert_eq!(set_top_level_scalar("iconFileX: 1\n", "iconFile", "a.png"), "iconFileX: 1\niconFile: a.png\n");
    }

    const SEVEN_ZIP: &str = r#"url: https://github.com/ip7z/7zip
urlRedirects: false
preDownloadScript: |
  using module .\Modules\Custom\GithubDownloader.psm1
  $downloadFileRegex = "7z[0-9]{3,5}-x64\.msi$"
  $Version = $downloader.LatestVersion
installScript: msiexec /i <filename> /qn /norestart
uninstallScript: msiexec /x <productcode> /qn /norestart
iconFile: 7zip.png
id: 7zip
displayName: 7-Zip
publisher: Igor Pavlov
softwareName: "7-Zip*"
detectionType: msi
"#;

    #[test]
    fn round_trips_without_semantic_change() {
        let doc = RecipeDoc::parse(SEVEN_ZIP).unwrap();
        let emitted = doc.to_yaml();
        let back = RecipeDoc::parse(&emitted).unwrap();
        assert_eq!(doc, back, "emitted:\n{emitted}");
        assert!(emitted.starts_with("url: https://github.com/ip7z/7zip\n"));
        assert!(emitted.contains("preDownloadScript: |\n  using module"));
        assert_eq!(doc.keys().next(), Some("url"));
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(scalar_repr("msi"), "msi");
        assert_eq!(scalar_repr("msiexec /i <filename> /qn"), "msiexec /i <filename> /qn");
        assert_ne!(scalar_repr("true"), "true");
        assert_ne!(scalar_repr("7-Zip*: x"), "7-Zip*: x");
        assert_ne!(scalar_repr("1.0"), "1.0");
        assert_ne!(scalar_repr(""), "");
        let tricky = ["*glob", "a #b", "- x", "{x}", "100", "null", "~"];
        for t in tricky {
            let yaml = format!("k: {}\n", scalar_repr(t));
            let v: Value = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(v["k"], Value::String(t.into()), "{yaml}");
        }
    }

    #[test]
    fn set_inserts_in_canonical_position() {
        let mut doc = RecipeDoc::parse("id: x\ndetectionType: msi\n").unwrap();
        doc.set_from_text("publisher", "Contoso");
        doc.set_from_text("installExperience", "user");
        let keys: Vec<_> = doc.keys().collect();
        assert_eq!(keys, ["id", "publisher", "installExperience", "detectionType"]);
    }

    #[test]
    fn typed_setters() {
        let mut doc = RecipeDoc::default();
        doc.set_from_text("urlRedirects", "true");
        doc.set_from_text("numVersionsToKeep", "3");
        doc.set_from_text("defaultDeploymentGroups", "a, b\nc");
        doc.set_from_text("preDownloadScript", "$Version = '1'");
        assert_eq!(doc.get("urlRedirects"), Some(&Value::Bool(true)));
        assert_eq!(doc.get("numVersionsToKeep"), Some(&Value::Number(3.into())));
        assert_eq!(doc.text_for_editor("defaultDeploymentGroups"), "a\nb\nc");
        assert!(doc.to_yaml().contains("preDownloadScript: |\n  $Version = '1'\n"));
        doc.set_from_text("urlRedirects", "");
        assert!(!doc.contains("urlRedirects"));
    }

    #[test]
    fn block_scalars_preserve_leading_spaces_and_blank_lines() {
        let mut doc = RecipeDoc::default();
        let script = "  indented first\n\nsecond\n";
        doc.set("downloadScript", Value::String(script.into()));
        doc.set("postRunScript", Value::String("no trailing newline".into()));
        doc.set("detectionScript", Value::String("a\nb".into()));
        let back = RecipeDoc::parse(&doc.to_yaml()).unwrap();
        assert_eq!(back.get_str("downloadScript").unwrap(), script);
        assert_eq!(back.get_str("detectionScript").unwrap(), "a\nb");
    }

    #[test]
    fn complex_values_fall_back_to_serde() {
        let text = "id: f\nadditionalDetectionRules:\n  - detectionType: registry\n    registryDetectionKey: HKLM\\x\n";
        let doc = RecipeDoc::parse(text).unwrap();
        assert_eq!(RecipeDoc::parse(&doc.to_yaml()).unwrap(), doc);
    }

    #[test]
    fn detects_comments() {
        assert!(has_comments("id: x\n# comment\n"));
        assert!(has_comments("id: x # trailing\n"));
        assert!(!has_comments("preDownloadScript: |\n  # powershell comment\n  $a = 1\nid: x\n"));
        assert!(!has_comments("url: https://x/#anchor\n"));
    }

    #[test]
    fn template_instantiation() {
        let tpl = "url: \nurlRedirects: false\npreDownloadScript: |\n\ninstallScript: msiexec /i <filename> /qn\nid: \ndisplayName: \niconFile: \ndetectionType: msi\n";
        let doc = new_from_template(tpl, "contoso", "Contoso App", "Contoso").unwrap();
        assert_eq!(doc.get_str("id").unwrap(), "contoso");
        assert_eq!(doc.get_str("iconFile").unwrap(), "contoso.png");
        assert!(!doc.contains("preDownloadScript"));
        assert!(doc.contains("url"));
    }

    #[test]
    fn base_merge() {
        let base = RecipeDoc::parse("id: base\npublisher: JetBrains\ndetectionType: msi\n").unwrap();
        let child = RecipeDoc::parse("base: base\nid: clion\n").unwrap();
        let merged = child.merged_with_base(&base);
        assert_eq!(merged.get_str("id").unwrap(), "clion");
        assert_eq!(merged.get_str("publisher").unwrap(), "JetBrains");
        assert!(!merged.contains("base"));
    }

    #[test]
    fn ids() {
        assert!(is_valid_id("7zip.arm64"));
        assert!(!is_valid_id("SevenZip"));
        assert_eq!(suggest_id("Notepad++ 8"), "notepadplusplus8");
    }
}
