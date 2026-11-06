//! Batch intake: reads a request list (ticket export, spreadsheet, e-mail text…)
//! into uniform records, and pulls the software that needs packaging out of them.
//!
//! Two extractors share the [`BatchItem`] shape:
//! * [`quick_scan`] - offline heuristics, instant, used as a preview and when no
//!   agent is available;
//! * the agent (see [`crate::agent::AgentTask::Extract`]), which reads the
//!   [`digest`] this module writes and returns `items.json`.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::workspace::RecipeEntry;

/// Extensions [`read_document`] understands, for file dialogs.
pub const EXTENSIONS: &[&str] = &[
    "json", "jsonl", "ndjson", "yaml", "yml", "csv", "tsv", "xlsx", "xlsm", "xlsb", "xls", "ods", "txt", "md", "log",
    "eml", "html", "htm", "docx",
];

/// One request (ticket, spreadsheet row, text block) with the noise removed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Ticket number or row label, e.g. `TASK1302859` or `row 4`.
    pub reference: String,
    pub title: String,
    pub body: String,
    /// Every other useful field, in source order.
    pub fields: Vec<(String, String)>,
}

impl Record {
    pub fn text(&self) -> String {
        format!("{}\n{}", self.title, self.body)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Document {
    pub path: PathBuf,
    /// Human-readable description of what was recognised.
    pub format: String,
    pub records: Vec<Record>,
}

// ---- reading -------------------------------------------------------------

pub fn read_document(path: &Path) -> Result<Document> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let (format, records) = match ext.as_str() {
        "json" => {
            let text = crate::read_text(path)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => json_records(&v, "JSON"),
                // Some exports are JSON lines with a .json extension.
                Err(e) => json_lines(&text).with_context(|| format!("not valid JSON: {e}"))?,
            }
        }
        "jsonl" | "ndjson" => json_lines(&crate::read_text(path)?)?,
        "yaml" | "yml" => {
            let v: serde_yaml::Value = serde_yaml::from_str(&crate::read_text(path)?).context("not valid YAML")?;
            json_records(&serde_json::to_value(v).context("YAML with non-string keys")?, "YAML")
        }
        "csv" | "tsv" => {
            let delim = if ext == "tsv" { b'\t' } else { sniff_delimiter(&crate::read_text(path)?) };
            ("CSV".to_string(), table_records(csv_rows(path, delim)?))
        }
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => spreadsheet_records(path)?,
        "docx" => ("Word document".to_string(), text_records(&docx_text(path)?)),
        "html" | "htm" => ("HTML".to_string(), text_records(&html_to_text(&crate::read_text(path)?))),
        _ => {
            let bytes = std::fs::read(path)?;
            if bytes.iter().take(4096).filter(|b| **b == 0).count() > 8 {
                bail!("'{}' looks like a binary file this reader does not understand.", path.display());
            }
            let text = crate::read_text(path)?;
            // Pasted exports often lack the right extension.
            let trimmed = text.trim_start();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && let Ok(v) = serde_json::from_str::<Value>(&text)
            {
                json_records(&v, "JSON")
            } else if ext == "eml" || ext == "txt" || ext == "md" || ext == "log" || ext.is_empty() {
                ("text".to_string(), text_records(&text))
            } else {
                (format!("text (.{ext})"), text_records(&text))
            }
        }
    };
    Ok(Document { path: path.to_path_buf(), format, records: records.into_iter().filter(|r| !r.title.is_empty() || !r.body.is_empty() || !r.fields.is_empty()).collect() })
}

fn json_lines(text: &str) -> Result<(String, Vec<Record>)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        if let Value::Object(m) = v {
            out.push(object_record(&m, out.len()));
        }
    }
    Ok(("JSON lines".to_string(), out))
}

fn json_records(v: &Value, kind: &str) -> (String, Vec<Record>) {
    let (path, objects) = find_objects(v, "");
    let label = if is_servicenow(&objects) {
        format!("ServiceNow export ({kind})")
    } else if path.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}, records under `{path}`")
    };
    if objects.is_empty() {
        // Scalars or arrays of strings: treat as text.
        let text = match v {
            Value::Array(a) => a.iter().map(scalar_text).collect::<Vec<_>>().join("\n"),
            other => scalar_text(other),
        };
        return (label, text_records(&text));
    }
    (label, objects.iter().enumerate().map(|(i, o)| object_record(o, i)).collect())
}

/// The largest array of objects in the document (ServiceNow `records`, Graph
/// `value`, a bare array...), or the top-level object itself.
fn find_objects<'a>(v: &'a Value, path: &str) -> (String, Vec<&'a Map<String, Value>>) {
    match v {
        Value::Array(items) => {
            let objs: Vec<_> = items.iter().filter_map(Value::as_object).collect();
            if !objs.is_empty() {
                return (path.to_string(), objs);
            }
            (path.to_string(), Vec::new())
        }
        Value::Object(map) => {
            let mut best: (String, Vec<&Map<String, Value>>) = (String::new(), Vec::new());
            for (k, child) in map {
                if matches!(child, Value::Array(_) | Value::Object(_)) {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    let found = find_objects(child, &p);
                    if found.1.len() > best.1.len() && (found.1.len() > 1 || matches!(child, Value::Array(_))) {
                        best = found;
                    }
                }
            }
            if best.1.is_empty() { (path.to_string(), vec![map]) } else { best }
        }
        _ => (path.to_string(), Vec::new()),
    }
}

fn is_servicenow(objects: &[&Map<String, Value>]) -> bool {
    objects.first().is_some_and(|o| o.contains_key("sys_id") && o.contains_key("short_description"))
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // ServiceNow `display_value` objects and Jira `{name: ...}` references.
        Value::Object(m) => ["display_value", "displayName", "name", "value", "title"]
            .iter()
            .find_map(|k| m.get(*k).filter(|v| !v.is_object()).map(scalar_text))
            .unwrap_or_else(|| serde_json::to_string(m).unwrap_or_default()),
        Value::Array(a) => a.iter().map(scalar_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
    }
}

const REFERENCE_KEYS: &[&str] = &["number", "ticket", "ticketnumber", "ticketid", "key", "id", "requestid", "request", "case", "casenumber", "task", "ritm", "item", "ref", "reference"];
const TITLE_KEYS: &[&str] = &["shortdescription", "title", "summary", "subject", "software", "softwarename", "application", "applicationname", "app", "appname", "product", "name"];
const BODY_KEYS: &[&str] = &["description", "details", "body", "requestdetails", "notes", "comments", "commentsandworknotes", "worknotes", "justification", "message", "text"];

fn norm_key(k: &str) -> String {
    k.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Fields that only carry system identifiers or workflow state.
fn is_noise(key: &str, value: &str) -> bool {
    static HEX: OnceLock<Regex> = OnceLock::new();
    let hex = HEX.get_or_init(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());
    let k = key.to_ascii_lowercase();
    value.trim().is_empty()
        || hex.is_match(value.trim())
        || k.starts_with("x_")
        || k.starts_with("sn_")
        || k.starts_with("__")
        || (k.starts_with("sys_") && k != "sys_created_on")
        || matches!(
            k.as_str(),
            "upon_reject" | "upon_approval" | "made_sla" | "knowledge" | "escalation" | "reassignment_count" | "approval" | "active"
                | "time_worked" | "task_effective_number" | "route_reason" | "universal_request" | "u_source_table" | "contact_type" | "order"
        )
}

fn object_record(obj: &Map<String, Value>, index: usize) -> Record {
    let mut flat: Vec<(String, String)> = Vec::new();
    flatten(obj, "", &mut flat, 0);
    let mut rec = Record::default();
    let take = |flat: &mut Vec<(String, String)>, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|want| {
            let i = flat.iter().position(|(k, _)| norm_key(k.rsplit('.').next().unwrap_or(k)) == *want)?;
            Some(flat.remove(i).1)
        })
    };
    rec.reference = take(&mut flat, REFERENCE_KEYS).unwrap_or_else(|| format!("#{}", index + 1));
    rec.title = take(&mut flat, TITLE_KEYS).unwrap_or_default();
    let mut body = Vec::new();
    while let Some(b) = take(&mut flat, BODY_KEYS) {
        if !body.contains(&b) {
            body.push(b);
        }
    }
    rec.body = body.join("\n\n");
    rec.fields = flat.into_iter().filter(|(k, v)| !is_noise(k, v)).collect();
    rec
}

fn flatten(obj: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String)>, depth: usize) {
    for (k, v) in obj {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            Value::Object(m) if depth < 2 && !m.contains_key("display_value") && !m.contains_key("value") => flatten(m, &key, out, depth + 1),
            _ => {
                let text = scalar_text(v);
                if !text.trim().is_empty() && !is_noise(&key, &text) {
                    out.push((key, text));
                }
            }
        }
    }
}

fn sniff_delimiter(text: &str) -> u8 {
    let first = text.lines().next().unwrap_or("");
    b",;\t|".iter().copied().max_by_key(|d| first.bytes().filter(|b| b == d).count()).unwrap_or(b',')
}

fn csv_rows(path: &Path, delim: u8) -> Result<Vec<Vec<String>>> {
    let text = crate::read_text(path)?;
    let mut rdr = csv::ReaderBuilder::new().delimiter(delim).has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut rows = Vec::new();
    for r in rdr.records() {
        rows.push(r?.iter().map(|s| s.trim().to_string()).collect());
    }
    Ok(rows)
}

fn spreadsheet_records(path: &Path) -> Result<(String, Vec<Record>)> {
    use calamine::{Reader, open_workbook_auto};
    let mut wb = open_workbook_auto(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    let mut sheets = Vec::new();
    for (name, range) in wb.worksheets() {
        let rows: Vec<Vec<String>> = range.rows().map(|r| r.iter().map(|c| c.to_string().trim().to_string()).collect()).collect();
        let mut recs = table_records(rows);
        if recs.is_empty() {
            continue;
        }
        for r in &mut recs {
            r.reference = format!("{name} {}", r.reference);
        }
        sheets.push(format!("{name} ({})", recs.len()));
        out.extend(recs);
    }
    Ok((format!("spreadsheet: {}", sheets.join(", ")), out))
}

/// Rows with a header row become objects; a single text column becomes text.
fn table_records(rows: Vec<Vec<String>>) -> Vec<Record> {
    let rows: Vec<Vec<String>> = rows.into_iter().filter(|r| r.iter().any(|c| !c.is_empty())).collect();
    let Some(header_at) = rows.iter().position(|r| r.iter().filter(|c| !c.is_empty()).count() >= 2) else {
        let text = rows.iter().map(|r| r.join(" ")).collect::<Vec<_>>().join("\n");
        return text_records(&text);
    };
    let header: Vec<String> = rows[header_at].iter().enumerate().map(|(i, h)| if h.is_empty() { format!("column {}", i + 1) } else { h.clone() }).collect();
    rows[header_at + 1..]
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut m = Map::new();
            for (h, c) in header.iter().zip(row) {
                if !c.is_empty() {
                    m.insert(h.clone(), Value::String(c.clone()));
                }
            }
            let mut r = object_record(&m, i);
            if r.reference.starts_with('#') {
                r.reference = format!("row {}", header_at + i + 2);
            }
            r
        })
        .collect()
}

fn docx_text(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).context("not a .docx (zip) file")?;
    let mut xml = String::new();
    zip.by_name("word/document.xml").context("no word/document.xml")?.read_to_string(&mut xml)?;
    let xml = xml.replace("</w:p>", "\n").replace("<w:tab/>", "\t").replace("<w:br/>", "\n");
    Ok(html_entities(&strip_tags(&xml)))
}

fn strip_tags(s: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    TAG.get_or_init(|| Regex::new(r"(?s)<[^>]*>").unwrap()).replace_all(s, "").into_owned()
}

fn html_entities(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn html_to_text(html: &str) -> String {
    static DROP: OnceLock<Regex> = OnceLock::new();
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    let drop = DROP.get_or_init(|| Regex::new(r"(?is)<(script|style|head)\b.*?</(script|style|head)>").unwrap());
    let block = BLOCK.get_or_init(|| Regex::new(r"(?i)<(br|/p|/div|/li|/tr|/h[1-6])\b[^>]*>").unwrap());
    let s = drop.replace_all(html, "");
    let s = block.replace_all(&s, "\n");
    let s = Regex::new(r"(?i)<li\b[^>]*>").unwrap().replace_all(&s, "- ");
    let text = html_entities(&strip_tags(&s));
    text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

/// Splits free text into one record per ticket when it is a list of tickets
/// (lines starting with an id like `RITM0012345`), otherwise one record.
fn text_records(text: &str) -> Vec<Record> {
    static START: OnceLock<Regex> = OnceLock::new();
    let start = START.get_or_init(|| Regex::new(r"(?m)^\s*(?:#+\s*)?\b([A-Z]{2,8}-?\d{4,})\b[\s:|\-–]*(.*)$").unwrap());
    let starts: Vec<_> = start.captures_iter(text).map(|c| (c.get(0).unwrap().start(), c[1].to_string(), c[2].trim().to_string())).collect();
    if starts.len() >= 2 {
        let mut out = Vec::new();
        for (i, (at, reference, title)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map_or(text.len(), |s| s.0);
            let chunk = &text[*at..end];
            let body = chunk.split_once('\n').map(|(_, b)| b.trim().to_string()).unwrap_or_default();
            out.push(Record { reference: reference.clone(), title: title.clone(), body, fields: Vec::new() });
        }
        return out;
    }
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let (title, body) = match text.split_once('\n') {
        Some((t, b)) if t.len() <= 120 => (t.trim().trim_start_matches(['#', ' ']).to_string(), b.trim().to_string()),
        _ => (String::new(), text.to_string()),
    };
    vec![Record { reference: "#1".into(), title, body, fields: Vec::new() }]
}

/// Markdown handed to the agent: every record, the useful fields only.
pub fn digest(doc: &Document, max_chars: usize) -> String {
    let mut s = format!(
        "# Request list\n\nSource file: `{}` ({}), {} record(s).\n\n",
        doc.path.display(),
        doc.format,
        doc.records.len()
    );
    for (i, r) in doc.records.iter().enumerate() {
        let mut block = format!("## {} - {}\n\n", r.reference, if r.title.is_empty() { "(no title)" } else { &r.title });
        if !r.body.is_empty() {
            block += &format!("{}\n\n", r.body.trim());
        }
        for (k, v) in &r.fields {
            let v = if v.len() > 400 { format!("{}…", &v[..v.char_indices().nth(400).map_or(v.len(), |c| c.0)]) } else { v.clone() };
            block += &format!("- **{k}**: {}\n", v.replace('\n', " "));
        }
        block.push('\n');
        if s.len() + block.len() > max_chars {
            s += &format!("\n_[{} more record(s) truncated - read the source file for them]_\n", doc.records.len() - i);
            break;
        }
        s += &block;
    }
    s
}

// ---- extraction ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestKind {
    /// No recipe exists yet.
    #[default]
    New,
    /// A recipe exists but needs a newer version, source or switches.
    Update,
    /// A recipe already covers it (assignment or scoping is all that is needed).
    Existing,
    /// The request is not about packaging software.
    NotSoftware,
}

impl RequestKind {
    pub const ALL: [RequestKind; 4] = [RequestKind::New, RequestKind::Update, RequestKind::Existing, RequestKind::NotSoftware];

    pub fn label(self) -> &'static str {
        match self {
            RequestKind::New => "New recipe",
            RequestKind::Update => "Update recipe",
            RequestKind::Existing => "Already packaged",
            RequestKind::NotSoftware => "Not software",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemStatus {
    #[default]
    Pending,
    Queued,
    Running,
    Done,
    Failed,
    Skipped,
}

/// One piece of software to package (or a record explaining why there is none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BatchItem {
    /// Ticket / row the item came from.
    #[serde(alias = "reference", alias = "ticket")]
    pub source: String,
    #[serde(alias = "name", alias = "software", alias = "application")]
    pub app: String,
    pub publisher: String,
    pub version: String,
    /// Download links or installers the requester supplied.
    pub urls: Vec<String>,
    /// The vendor's product page, if known.
    #[serde(alias = "homepage")]
    pub vendor_url: String,
    pub request: RequestKind,
    /// Matching recipe id, when one exists.
    #[serde(alias = "recipe", alias = "existingRecipeId")]
    pub existing_recipe: String,
    pub suggested_id: String,
    pub platform: String,
    pub notes: String,
    /// 0..1, how sure the extractor is.
    pub confidence: f32,

    // Studio state, not part of the extractor's output.
    pub include: bool,
    pub status: ItemStatus,
    /// Recipe ids the agent reported for this item.
    pub recipes: Vec<String>,
    pub status_note: String,
    /// Studio-owned harness evidence, retained when the batch is reopened.
    pub checks: Vec<BatchCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchCheck {
    pub recipe: String,
    pub passed: bool,
    pub note: String,
    pub timestamp: String,
    pub summary_dir: Option<PathBuf>,
}

impl BatchItem {
    pub fn recipe_id(&self) -> String {
        if !self.existing_recipe.is_empty() {
            return self.existing_recipe.clone();
        }
        if !self.suggested_id.is_empty() {
            return self.suggested_id.clone();
        }
        crate::recipe::suggest_id(&self.app)
    }

    /// Notes handed to the agent along with the item.
    pub fn agent_notes(&self, record: Option<&Record>) -> String {
        let mut n = String::new();
        if !self.source.is_empty() {
            n += &format!("Requested in {}.\n", self.source);
        }
        if !self.publisher.is_empty() {
            n += &format!("Publisher: {}\n", self.publisher);
        }
        if !self.version.is_empty() {
            n += &format!("Version the requester mentioned: {}\n", self.version);
        }
        if !self.urls.is_empty() {
            n += &format!("Links from the request (research vendor internet downloads first; shared folders/software dropbox are fallback media only):\n{}\n", self.urls.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n"));
        }
        if !self.notes.is_empty() {
            n += &format!("Notes: {}\n", self.notes);
        }
        if let Some(r) = record {
            let body: String = r.body.chars().take(1500).collect();
            n += &format!("\nOriginal request ({}): {}\n{}\n", r.reference, r.title, body);
        }
        n
    }
}

/// A recipe the item probably refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct RecipeMatch {
    pub id: String,
    pub display_name: String,
    pub score: f32,
}

const STOPWORDS: &[&str] = &["the", "for", "and", "app", "apps", "application", "software", "softwares", "desktop", "client", "x64", "64", "bit", "win", "windows", "edition", "latest", "version", "installer", "setup", "inc", "llc", "ltd", "corp", "corporation"];

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .replace('+', "plus")
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty() && !STOPWORDS.contains(t))
        .map(str::to_string)
        .collect()
}

fn squash(s: &str) -> String {
    s.to_lowercase().replace('+', "plus").chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Ranks recipes by how well their id / displayName match `name`.
pub fn match_recipes(name: &str, recipes: &[RecipeEntry], limit: usize) -> Vec<RecipeMatch> {
    let want = squash(name);
    let want_tokens = tokens(name);
    if want.len() < 2 {
        return Vec::new();
    }
    let mut out: Vec<RecipeMatch> = Vec::new();
    for r in recipes {
        let mut best = 0f32;
        for key in [r.id.as_str(), r.meta.display_name.as_str()] {
            let k = squash(key);
            if k.is_empty() {
                continue;
            }
            let mut score = if k == want {
                1.0
            } else if (k.len() >= 4 && want.contains(&k)) || (want.len() >= 4 && k.contains(&want)) {
                let (a, b) = (k.len().min(want.len()) as f32, k.len().max(want.len()) as f32);
                0.55 + 0.4 * (a / b)
            } else {
                0.0
            };
            let kt = tokens(key);
            if !want_tokens.is_empty() && !kt.is_empty() {
                let inter = want_tokens.iter().filter(|t| kt.contains(t)).count() as f32;
                let union = (want_tokens.len() + kt.len()) as f32 - inter;
                let jaccard = inter / union;
                // Every requested word is in the recipe name ("Lockdown Browser" ->
                // "Respondus LockDown Browser").
                let contained = inter as usize == want_tokens.len() && (want_tokens.len() >= 2 || want.len() >= 6);
                score = score.max(jaccard * 0.9).max(if contained { 0.8 } else { 0.0 });
            }
            best = best.max(score);
        }
        // Prefer the base recipe over architecture variants (7zip over 7zip.arm64).
        if r.id.contains(".arm64") || r.id.ends_with("arm64") {
            best -= 0.05;
        }
        if best >= 0.6 {
            out.push(RecipeMatch { id: r.id.clone(), display_name: r.meta.display_name.clone(), score: best.min(1.0) });
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.len().cmp(&b.id.len())));
    out.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    out.truncate(limit);
    out
}

pub(crate) fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

pub fn find_urls(text: &str) -> Vec<String> {
    static URL: OnceLock<Regex> = OnceLock::new();
    let mut out: Vec<String> = Vec::new();
    for m in re(&URL, r#"https?://[^\s<>"'\]\)]+"#).find_iter(text) {
        let u = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string();
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// The highest dotted version number in `text` (outside URLs).
fn find_version(text: &str) -> Option<String> {
    static URL: OnceLock<Regex> = OnceLock::new();
    static VER: OnceLock<Regex> = OnceLock::new();
    let without_urls = re(&URL, r"https?://\S+").replace_all(text, " ");
    re(&VER, r"(?i)\b(?:v(?:ersion)?\s*)?(\d{1,4}(?:\.\d{1,5}){1,3})\b")
        .captures_iter(&without_urls)
        .map(|c| c[1].to_string())
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Does this record ask for software at all?
fn packaging_signal(title: &str, body: &str) -> f32 {
    static VERB: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    static NEG: OnceLock<Regex> = OnceLock::new();
    let verb = re(&VERB, r"(?i)\b(packag\w*|install\w*|deploy\w*|add|update|upgrade|publish|software|apps?|applications?)\b");
    let cue = re(&CUE, r"(?i)(company portal|software cent(er|re)|\binstall|\bdownload|\.msi\b|\.exe\b|\binstaller|available to install|make .{0,40} available|softwares?\b|new version|latest version|licens)");
    let neg = re(&NEG, r"(?i)\b(schedule|discussion|discuss|onboard\w*|autopilot|enrol\w*|manage devices|group tags?|scope (group|tag)|exemption|compliance|bitlocker|policy|policies|configuration profile|meeting)\b");
    let mut s = 0.0;
    if verb.is_match(title) {
        s += 0.5;
    }
    s += 0.15 * cue.find_iter(body).take(3).count() as f32;
    if neg.is_match(title) {
        s -= 0.6;
    }
    s
}

/// Cleans a ticket title down to the product it names, e.g.
/// "Intune/ConfigMgr - Package ArcGIS Pro with .NET 10 Desktop Runtime" ->
/// ["ArcGIS Pro", ".NET 10 Desktop Runtime"].
fn names_from_title(title: &str) -> (Option<&'static str>, Vec<String>) {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static VERB: OnceLock<Regex> = OnceLock::new();
    static TAIL: OnceLock<Regex> = OnceLock::new();
    let prefix = re(&PREFIX, r"(?i)^\s*(?:(?:intune|configmgr|sccm|mcm|memcm|endpoint|request|req|software|app)\s*[/&,]?\s*)+(?:[-:–|]\s*)?");
    let verb = re(&VERB, r"(?i)^(?:please\s+)?(package|packaging|add|install|deploy|update|upgrade|publish|request|new|make available)\b[:\s]*(?:of\s+|for\s+)?");
    let tail = re(&TAIL, r"(?i)\s+(?:for|on|in|to|via|into|across)\s+(?:the\s+)?(?:company portal|software center|cp|swc|intune|sccm|configmgr|all|our|my|\w+ (?:devices|computers|laptops|lab|labs|staff|students))\b.*$");
    let mut t = prefix.replace(title.trim(), "").trim().to_string();
    let mut v = None;
    if let Some(c) = verb.captures(&t) {
        v = Some(match c[1].to_lowercase().as_str() {
            "update" | "upgrade" => "update",
            _ => "new",
        });
        t = t[c.get(0).unwrap().end()..].to_string();
    }
    let t = tail.replace(&t, "").trim().trim_matches(['-', ':', '.', ' ']).to_string();
    if t.is_empty() {
        return (v, Vec::new());
    }
    let parts: Vec<String> = Regex::new(r"(?i)\s+(?:with|and|plus|\+|&)\s+|,\s*|;\s*")
        .unwrap()
        .split(&t)
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    (v, parts)
}

/// Titles like "Library Software" or "Printer apps/drivers" name a group, not a product.
fn is_generic(name: &str) -> bool {
    let t = tokens(name);
    let raw = name.to_lowercase();
    t.is_empty()
        || raw.ends_with("software")
        || raw.ends_with("softwares")
        || raw.ends_with("apps")
        || raw.contains("apps/")
        || raw.ends_with("applications")
        || raw.ends_with("programs")
        || raw.ends_with("drivers")
        || raw.ends_with("tools")
}

/// Short lines that read like a list of product names.
fn list_items(body: &str) -> Vec<String> {
    static BULLET: OnceLock<Regex> = OnceLock::new();
    let bullet = re(&BULLET, r"^\s*(?:[-*•·▪◦]|\d{1,2}[.)])\s+(.+?)\s*$");
    let mut bulleted = Vec::new();
    let mut blocks: Vec<Vec<String>> = vec![Vec::new()];
    for line in body.lines() {
        if let Some(c) = bullet.captures(line) {
            bulleted.push(c[1].to_string());
            continue;
        }
        let l = line.trim();
        let looks_like_name = !l.is_empty()
            && l.len() <= 60
            && l.split_whitespace().count() <= 7
            && !l.ends_with(['.', '?', '!', ':', ','])
            && !l.contains("://")
            && l.chars().next().is_some_and(|c| c.is_uppercase() || c.is_ascii_digit());
        if looks_like_name {
            blocks.last_mut().unwrap().push(l.to_string());
        } else {
            blocks.push(Vec::new());
        }
    }
    let mut items = bulleted;
    for b in blocks.into_iter().filter(|b| b.len() >= 3) {
        items.extend(b);
    }
    items.retain(|i| !looks_like_identifier(i) && !i.contains("://"));
    items
}

/// Serial numbers, device names and group names, which also come as lists.
fn looks_like_identifier(s: &str) -> bool {
    let s = s.trim();
    if s.contains(' ') {
        return false;
    }
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let upper = s.chars().filter(char::is_ascii_uppercase).count();
    let hyphens = s.matches('-').count();
    (s.len() >= 8 && digits >= 2 && (upper + digits) * 10 >= s.len() * 8) || hyphens >= 2 && upper * 2 >= s.len() || (digits >= 3 && s.chars().any(|c| c.is_ascii_lowercase()) && digits * 3 >= s.len())
}

/// Prefers the fuller product name from the body: "Capstone" -> "PASCO Capstone".
fn refine_name(name: &str, body: &str) -> String {
    let pattern = format!(r"\b((?:[A-Z][A-Za-z0-9+&.]*\s+){{1,2}}){}\b", regex::escape(name));
    if let Ok(r) = Regex::new(&pattern)
        && let Some(c) = r.captures(body)
    {
        let extra = c[1].trim();
        let skip = ["Please", "The", "Can", "We", "Package", "Install", "Add", "Need", "Make", "Update", "I", "A", "An", "Get", "This", "Our", "Intune", "Of"];
        let words: Vec<&str> = extra.split_whitespace().skip_while(|w| skip.contains(w)).collect();
        if !words.is_empty() && !words.iter().any(|w| skip.contains(w)) {
            return format!("{} {}", words.join(" "), name);
        }
    }
    name.to_string()
}

fn clean_name(raw: &str) -> String {
    static VER: OnceLock<Regex> = OnceLock::new();
    static REMARK: OnceLock<Regex> = OnceLock::new();
    let s = re(&VER, r"(?i)\s*(?:\(?\b(?:v(?:ersion)?\s*)?\d+(?:\.\d+){1,3}\)?|\(\s*(?:64|32)-?bit\s*\)|\blatest version\b|\bx64\b)").replace_all(raw, "");
    // "(likely already there)", "(if possible)" are remarks, "(Unity Version Control)" is not.
    let s = re(&REMARK, r"(?i)\s*\((?:[^)]*\b(?:likely|already|maybe|probably|possibly|if|needed|optional|please|thanks|see|same|current|newest|existing)\b[^)]*)\)").replace_all(&s, "");
    s.trim().trim_matches(['-', ':', '.', ',', '"', '\'', ' ']).to_string()
}

fn is_shared_link(u: &str) -> bool {
    let l = u.to_lowercase();
    ["dropbox.com", "box.com", "sharepoint.com", "onedrive", "1drv.ms", "drive.google.com", "wetransfer"].iter().any(|h| l.contains(h))
}

fn is_download_link(u: &str) -> bool {
    let l = u.to_lowercase();
    let path = l.split(['?', '#']).next().unwrap_or(&l);
    [".exe", ".msi", ".msix", ".zip", ".7z", ".dmg", ".pkg", ".appx"].iter().any(|e| path.ends_with(e))
}

pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?.split('@').next_back()?.split(':').next()?.to_lowercase();
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_string())
}

/// Offline extraction: one or more [`BatchItem`]s per record.
pub fn quick_scan(records: &[Record], recipes: &[RecipeEntry]) -> Vec<BatchItem> {
    let mut items = Vec::new();
    for rec in records {
        let signal = packaging_signal(&rec.title, &rec.body);
        let (verb, title_names) = names_from_title(if rec.title.is_empty() { rec.body.lines().next().unwrap_or("") } else { &rec.title });
        let mut names: Vec<String> = title_names.iter().filter(|n| !is_generic(n)).cloned().collect();
        let listed = list_items(&rec.body);
        let generic_title = names.is_empty();
        if generic_title || (signal >= 0.4 && listed.len() >= 2 && title_names.iter().any(|n| is_generic(n))) {
            names.extend(listed.iter().cloned());
        }
        let urls = find_urls(&rec.text());
        if signal < 0.3 || names.is_empty() {
            items.push(BatchItem {
                source: rec.reference.clone(),
                app: if rec.title.is_empty() { "(no software found)".into() } else { rec.title.clone() },
                request: RequestKind::NotSoftware,
                notes: if signal < 0.3 { "Does not read like a packaging request.".into() } else { "Asks for software but names no product; read the ticket.".into() },
                urls,
                confidence: if signal < 0.0 { 0.8 } else { 0.5 },
                include: false,
                ..Default::default()
            });
            continue;
        }
        let multi = names.len() > 1;
        let version = (!multi).then(|| find_version(&rec.text())).flatten().unwrap_or_default();
        let mut seen: Vec<String> = Vec::new();
        for raw in names {
            let mut app = clean_name(&raw);
            if app.is_empty() || seen.contains(&squash(&app)) {
                continue;
            }
            seen.push(squash(&app));
            if !multi && !generic_title {
                app = refine_name(&app, &rec.body);
            }
            let app_tokens: Vec<String> = tokens(&app).into_iter().filter(|t| t.len() >= 3).collect();
            let mine: Vec<String> = urls
                .iter()
                .filter(|u| {
                    if !multi {
                        return true;
                    }
                    let l = u.to_lowercase();
                    is_shared_link(u) || app_tokens.iter().any(|t| l.contains(t.as_str()))
                })
                .cloned()
                .collect();
            let vendor_url = mine.iter().find(|u| !is_shared_link(u) && !is_download_link(u)).cloned().unwrap_or_default();
            let matches = match_recipes(&app, recipes, 3);
            let best = matches.first().filter(|m| m.score >= 0.75);
            let request = match (best, verb) {
                (Some(_), Some("update")) => RequestKind::Update,
                (Some(_), _) if !version.is_empty() => RequestKind::Update,
                (Some(_), _) => RequestKind::Existing,
                (None, _) => RequestKind::New,
            };
            let mut notes = Vec::new();
            if mine.iter().any(|u| is_shared_link(u)) {
                notes.push("Requester supplied the installer through a shared link; the recipe may need manualDownload.".to_string());
            }
            if matches.len() > 1 || best.is_none() && !matches.is_empty() {
                notes.push(format!("Possible recipes: {}", matches.iter().map(|m| format!("{} ({:.0}%)", m.id, m.score * 100.0)).collect::<Vec<_>>().join(", ")));
            }
            let confidence = (0.45 + signal.min(1.0) * 0.3 + if best.is_some() { 0.15 } else { 0.0 } + if multi { -0.1 } else { 0.05 }).clamp(0.1, 0.95);
            items.push(BatchItem {
                source: rec.reference.clone(),
                suggested_id: best.map(|m| m.id.clone()).unwrap_or_else(|| crate::recipe::suggest_id(&app)),
                existing_recipe: best.map(|m| m.id.clone()).unwrap_or_default(),
                app,
                version: version.clone(),
                urls: mine,
                vendor_url,
                request,
                platform: if rec.text().to_lowercase().contains("mac") && !rec.text().to_lowercase().contains("windows") { String::new() } else { "Windows".into() },
                notes: notes.join(" "),
                confidence,
                include: matches!(request, RequestKind::New | RequestKind::Update),
                ..Default::default()
            });
        }
    }
    flag_duplicates(&mut items);
    items
}

fn flag_duplicates(items: &mut [BatchItem]) {
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate().filter(|(_, it)| it.request != RequestKind::NotSoftware) {
        by_name.entry(squash(&it.app)).or_default().push(i);
    }
    for idx in by_name.values().filter(|v| v.len() > 1) {
        let sources: Vec<String> = idx.iter().map(|i| items[*i].source.clone()).collect();
        for (n, i) in idx.iter().enumerate() {
            let others: Vec<&String> = sources.iter().filter(|s| **s != items[*i].source).collect();
            if !others.is_empty() {
                let note = format!("Also requested in {}.", others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
                let it = &mut items[*i];
                it.notes = if it.notes.is_empty() { note } else { format!("{} {note}", it.notes) };
                if n > 0 {
                    it.include = false;
                }
            }
        }
    }
}

/// Validates and normalises an agent's `items.json` against the library.
pub fn parse_agent_items(text: &str, recipes: &[RecipeEntry]) -> Result<Vec<BatchItem>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Wrapped { items: Vec<BatchItem> },
        Bare(Vec<BatchItem>),
    }
    // Agents sometimes wrap the JSON in a fence.
    let body = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let mut items = match serde_json::from_str::<Shape>(body).context("items.json is not in the expected shape")? {
        Shape::Wrapped { items } | Shape::Bare(items) => items,
    };
    for it in &mut items {
        it.app = it.app.trim().to_string();
        it.urls.retain(|u| u.starts_with("http"));
        if !it.existing_recipe.is_empty() && !recipes.iter().any(|r| r.id.eq_ignore_ascii_case(&it.existing_recipe)) {
            it.notes = format!("{} (Agent named recipe '{}', which is not in the library.)", it.notes, it.existing_recipe).trim().to_string();
            it.existing_recipe.clear();
        }
        if it.existing_recipe.is_empty()
            && it.request != RequestKind::NotSoftware
            && let Some(m) = match_recipes(&it.app, recipes, 1).into_iter().find(|m| m.score >= 0.9)
        {
            it.existing_recipe = m.id;
            if it.request == RequestKind::New {
                it.request = RequestKind::Existing;
            }
        }
        if let Some(r) = recipes.iter().find(|r| r.id.eq_ignore_ascii_case(&it.existing_recipe)) {
            it.existing_recipe = r.id.clone();
        }
        if it.suggested_id.is_empty() || !crate::recipe::is_valid_id(&it.suggested_id) {
            it.suggested_id = if it.existing_recipe.is_empty() { crate::recipe::suggest_id(&it.app) } else { it.existing_recipe.clone() };
        }
        it.confidence = if it.confidence > 1.0 { it.confidence / 100.0 } else { it.confidence }.clamp(0.0, 1.0);
        it.include = matches!(it.request, RequestKind::New | RequestKind::Update);
        it.status = ItemStatus::Pending;
    }
    items.retain(|i| !i.app.is_empty());
    flag_duplicates(&mut items);
    Ok(items)
}

/// The JSON shape the agent is asked to write (kept next to the parser so the
/// two cannot drift).
pub const AGENT_SCHEMA: &str = r#"{
  "items": [
    {
      "source": "TASK1234567",              // ticket/row reference the item came from
      "app": "QGIS",                        // product name as the vendor writes it
      "publisher": "QGIS.org",
      "version": "3.40.1",                  // only if the requester asked for a specific version, else ""
      "urls": ["https://..."],              // links from the request (downloads, shared folders)
      "vendorUrl": "https://qgis.org",      // the vendor's product or download page (research it if missing)
      "request": "new",                     // new | update | existing | not-software
      "existingRecipe": "qgis",             // id of the matching recipe in Recipes\ (any sub-folder), or ""
      "suggestedId": "qgis",                // lowercase id for a new recipe
      "platform": "Windows",                // Windows | macOS | unknown
      "notes": "Licensed per seat; installer only on the requester's Dropbox.",
      "confidence": 0.9
    }
  ]
}"#;

// ---- batches on disk -----------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Batch {
    pub id: String,
    pub source: PathBuf,
    pub format: String,
    pub created: String,
    /// "quick scan" or the agent backend that extracted the items.
    pub extracted_by: String,
    pub records: Vec<Record>,
    pub items: Vec<BatchItem>,
}

impl Batch {
    pub fn root() -> PathBuf {
        crate::settings::Settings::default_path().parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir).join("batches")
    }

    pub fn dir(&self) -> PathBuf {
        Self::root().join(&self.id)
    }

    pub fn new(doc: Document, recipes: &[RecipeEntry]) -> Self {
        let stem: String = doc.path.file_stem().and_then(|s| s.to_str()).unwrap_or("batch").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let stamp = timestamp();
        let items = quick_scan(&doc.records, recipes);
        Batch {
            id: format!("{}-{}", stamp.replace([':', ' '], "").replace('-', ""), stem.trim_matches('-').chars().take(40).collect::<String>()),
            source: doc.path,
            format: doc.format,
            created: stamp,
            extracted_by: "quick scan".into(),
            records: doc.records,
            items,
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("batch.json"), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let text = crate::read_text(&dir.join("batch.json"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Saved batches, newest first.
    pub fn list() -> Vec<(String, PathBuf)> {
        let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(Self::root())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("batch.json").is_file())
            .map(|p| (p.file_name().unwrap_or_default().to_string_lossy().into_owned(), p))
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v
    }

    pub fn record(&self, reference: &str) -> Option<&Record> {
        self.records.iter().find(|r| r.reference == reference)
    }

    pub const AGENT_OUTPUT: &'static str = "agent-items.json";

    /// Writes `digest.md` (the requests) and `recipes.md` (what already exists)
    /// into `dir` (normally [`Batch::dir`]) for an extraction agent, removing stale output.
    pub fn prepare_agent(&self, dir: &Path, recipes: &[RecipeEntry]) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let doc = Document { path: self.source.clone(), format: self.format.clone(), records: self.records.clone() };
        std::fs::write(dir.join("digest.md"), digest(&doc, 600_000))?;
        let mut list = String::from("# Existing Yardstick recipes\n\nid | display name | publisher\n---|---|---\n");
        let mut seen = HashSet::new();
        for r in recipes {
            if seen.insert(r.id.to_lowercase()) {
                list += &format!("{} | {} | {}\n", r.id, r.meta.display_name, r.meta.publisher);
            }
        }
        std::fs::write(dir.join("recipes.md"), list)?;
        let _ = std::fs::remove_file(dir.join(Self::AGENT_OUTPUT));
        Ok(())
    }

    /// Replaces the items with the agent's (the caller saves).
    pub fn load_agent_items(&mut self, dir: &Path, recipes: &[RecipeEntry], backend: &str) -> Result<usize> {
        let path = dir.join(Self::AGENT_OUTPUT);
        let text = crate::read_text(&path).with_context(|| format!("the agent did not write {}", path.display()))?;
        let items = parse_agent_items(&text, recipes)?;
        self.items = items;
        self.extracted_by = backend.to_string();
        Ok(self.items.len())
    }

    /// Markdown table for pasting back into tickets.
    pub fn report(&self) -> String {
        let mut s = String::from("| Ticket | Software | Version | Outcome | Recipe | Notes |\n|---|---|---|---|---|---|\n");
        for it in &self.items {
            let outcome = if !it.checks.is_empty() {
                it.checks.iter().map(|c| format!("{}: {}", c.recipe, c.note)).collect::<Vec<_>>().join("; ")
            } else { match it.status {
                ItemStatus::Done => "recipe ready for live test".to_string(),
                ItemStatus::Failed => "agent failed".to_string(),
                ItemStatus::Skipped => "skipped".to_string(),
                _ => it.request.label().to_string(),
            }};
            let recipe = if it.recipes.is_empty() { it.existing_recipe.clone() } else { it.recipes.join(", ") };
            let cell = |s: &str| s.replace('|', "/").replace('\n', " ");
            s += &format!("| {} | {} | {} | {} | {} | {} |\n", cell(&it.source), cell(&it.app), cell(&it.version), cell(&outcome), cell(&recipe), cell(&it.notes));
        }
        s
    }
}

/// UTC `YYYY-MM-DD HH:MM:SS`.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{RecipeLocation, RecipeMeta};

    /// `cargo test -- --ignored real_sample --nocapture` with YS_SAMPLE pointing at an export.
    #[test]
    #[ignore = "needs a local export and the production workspace"]
    fn real_sample() {
        let path = std::env::var("YS_SAMPLE").unwrap_or_else(|_| r"c:\Intune\OneOffPackager\to-package\Exported ticket list Nov 2.json".into());
        let ws = crate::workspace::Workspace::open(Path::new(r"C:\Intune\Yardstick"), &[PathBuf::from(r"C:\Intune\Yardstick")], &[]).unwrap();
        let recipes = ws.list_recipes();
        let doc = read_document(Path::new(&path)).unwrap();
        eprintln!("format {} records {} recipes {}", doc.format, doc.records.len(), recipes.len());
        for i in quick_scan(&doc.records, &recipes) {
            eprintln!(
                "{:<12} {:<5} {:<34} v{:<12} {:<10} {:<28} conf {:.2} urls {} | {}",
                i.source,
                i.include,
                i.app,
                i.version,
                i.request.label(),
                i.existing_recipe,
                i.confidence,
                i.urls.len(),
                i.notes
            );
        }
        eprintln!("{}", digest(&doc, 3000));
    }

    fn recipe(id: &str, name: &str) -> RecipeEntry {
        RecipeEntry {
            id: id.into(),
            path: PathBuf::from(format!(r"C:\x\{id}.yaml")),
            location: RecipeLocation::Shipped(None),
            groups: Vec::new(),
            meta: RecipeMeta { display_name: name.into(), ..Default::default() },
        }
    }

    fn library() -> Vec<RecipeEntry> {
        vec![
            recipe("arcgispro", "ArcGIS Pro"),
            recipe("almaofflinecirculation", "Alma Offline Circulation"),
            recipe("qgis", "QGIS"),
            recipe("dotnetdesktopruntime10", ".NET Desktop Runtime 10"),
            recipe("respondusldb", "Respondus LockDown Browser"),
            recipe("7zip", "7-Zip"),
            recipe("7zip.arm64", "7-Zip"),
        ]
    }

    /// Shaped like a real ServiceNow sc_task export.
    const SNOW: &str = r#"{"records":[
      {"number":"TASK1","short_description":"Intune - Package Library Software","sys_id":"e62af2d0c36cf6d0150330ff050131aa","description":"We have a few library-specific apps. Please package these apps:\n\n- Alma Offline Circulation\n- ArcGIS Pro\n- ERDAS Imagine\n- MarcEdit\n- QGIS\n\nDropbox Link (Installers) - https://www.dropbox.com/scl/fo/abc?dl=0\n\nThank you","priority":"3","upon_reject":"cancel"},
      {"number":"TASK2","short_description":"Intune - Schedule time to talk","description":"I am looking into adding all of the computers to Intune. I would like to schedule time.","sys_id":"x"},
      {"number":"TASK3","short_description":"Intune - Package Capstone","description":"Please update the deployed version of PASCO Capstone. The version in our system is 1.11.1.5 , their current version is 2.13.4\n\nhttps://www.pasco.com/products/software/capstone#downloads-panel"},
      {"number":"TASK4","short_description":"Intune - Manage devices for SILC","description":"There are (5) computers we'd like to manage:\nMBAFVFG48L2Q67L7\nMBAFVFG4AOSQ6L7\nMBAFVFJ2BYAQ6LR\nDH135macmpres"},
      {"number":"TASK5","short_description":"Intune/ConfigMgr - Package ArcGIS Pro with .NET 10 Desktop Runtime","description":"Can Arcgis pro latest version be packaged with .Net 10 desktop runtime?"},
      {"number":"TASK6","short_description":"Intune - MIX228 Software","description":"Good morning. I wanted to put this ticket in for tracking purposes.\nMIX228 computers will need these specific softwares scoped at your earliest convenience. Thank you!\n\nAutodesk Revit\nAvid Media Composer\nClaude Desktop\nDCP-O-Matic\nNablet HD"},
      {"number":"TASK7","short_description":"Intune - Update Lockdown Browser","description":"I need a group created for the TRIO laptops to get the latest version of the lockdown browser."},
      {"number":"TASK8","short_description":"Intune - BFIT Scope Group","description":"Issues with assigning configurations to groups.\nBF-INTUNE-DVC-TEST-2\nBF-INTUNE-DVC-TEST-Julie\nBF-INTUNE-DVC-TYPE-Shared"},
      {"number":"TASK9","short_description":"Intune - Package Mplus","description":"I need a new version of Mplus demo packaged. This one is version 9.1.0.0. Here is the download link: https://www.statmodel.com/download/Mplus%20Version%209.1%20Demo%20(64-bit).msi"}
    ]}"#;

    fn scan() -> Vec<BatchItem> {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("export.json");
        std::fs::write(&p, SNOW).unwrap();
        let doc = read_document(&p).unwrap();
        assert_eq!(doc.format, "ServiceNow export (JSON)");
        assert_eq!(doc.records.len(), 9);
        assert_eq!(doc.records[0].reference, "TASK1");
        assert!(doc.records[0].fields.iter().all(|(k, _)| k != "sys_id" && k != "upon_reject"));
        quick_scan(&doc.records, &library())
    }

    fn find<'a>(items: &'a [BatchItem], app: &str) -> &'a BatchItem {
        items.iter().find(|i| i.app == app).unwrap_or_else(|| panic!("{app} not found in {:#?}", items.iter().map(|i| &i.app).collect::<Vec<_>>()))
    }

    #[test]
    fn servicenow_list_items_and_matches() {
        let items = scan();
        let alma = find(&items, "Alma Offline Circulation");
        assert_eq!(alma.existing_recipe, "almaofflinecirculation");
        assert_eq!(alma.request, RequestKind::Existing);
        assert!(alma.urls.iter().any(|u| u.contains("dropbox")));
        let erdas = find(&items, "ERDAS Imagine");
        assert_eq!(erdas.request, RequestKind::New);
        assert_eq!(erdas.suggested_id, "erdasimagine");
        assert!(erdas.notes.contains("manualDownload"));
        assert!(items.iter().filter(|i| i.source == "TASK1").count() == 5);
    }

    #[test]
    fn single_app_titles_versions_and_refinement() {
        let items = scan();
        let cap = find(&items, "PASCO Capstone");
        assert_eq!(cap.version, "2.13.4");
        assert_eq!(cap.request, RequestKind::New);
        assert!(cap.vendor_url.contains("pasco.com"));
        let mplus = items.iter().find(|i| i.source == "TASK9").unwrap();
        assert!(mplus.app.starts_with("Mplus"));
        assert_eq!(mplus.version, "9.1.0.0");
        let ldb = items.iter().find(|i| i.source == "TASK7").unwrap();
        assert_eq!(ldb.existing_recipe, "respondusldb");
        assert_eq!(ldb.request, RequestKind::Update);
    }

    #[test]
    fn splits_with_and_flags_duplicates() {
        let items = scan();
        let t5: Vec<&BatchItem> = items.iter().filter(|i| i.source == "TASK5").collect();
        assert_eq!(t5.len(), 2, "{t5:#?}");
        assert_eq!(t5[0].existing_recipe, "arcgispro");
        assert_eq!(t5[1].existing_recipe, "dotnetdesktopruntime10");
        let arc: Vec<&BatchItem> = items.iter().filter(|i| i.existing_recipe == "arcgispro").collect();
        assert_eq!(arc.len(), 2);
        assert!(arc.iter().all(|i| i.notes.contains("Also requested in")));
    }

    #[test]
    fn bare_lists_and_non_software() {
        let items = scan();
        for app in ["Autodesk Revit", "Avid Media Composer", "Claude Desktop", "DCP-O-Matic", "Nablet HD"] {
            assert_eq!(find(&items, app).source, "TASK6");
        }
        for t in ["TASK2", "TASK4", "TASK8"] {
            let mine: Vec<_> = items.iter().filter(|i| i.source == t).collect();
            assert_eq!(mine.len(), 1, "{t}: {mine:#?}");
            assert_eq!(mine[0].request, RequestKind::NotSoftware, "{t}");
            assert!(!mine[0].include);
        }
    }

    #[test]
    fn recipe_matching_prefers_base_and_handles_containment() {
        let lib = library();
        assert_eq!(match_recipes("7-Zip", &lib, 2)[0].id, "7zip");
        assert_eq!(match_recipes("Lockdown Browser", &lib, 1)[0].id, "respondusldb");
        assert!(match_recipes("Zeiss Zen", &lib, 1).is_empty());
    }

    #[test]
    fn reads_csv_text_and_ticket_lists() {
        let d = tempfile::tempdir().unwrap();
        let csv = d.path().join("r.csv");
        std::fs::write(&csv, "Ticket;Software;Notes\nRITM1;Notepad++;latest\nRITM2;QGIS;\n").unwrap();
        let doc = read_document(&csv).unwrap();
        assert_eq!(doc.records.len(), 2);
        assert_eq!(doc.records[0].reference, "RITM1");
        assert_eq!(doc.records[0].title, "Notepad++");

        let txt = d.path().join("r.txt");
        std::fs::write(&txt, "RITM0001 - Package Zoom\nfor the lab\nRITM0002: Add Blender\nplease\n").unwrap();
        let doc = read_document(&txt).unwrap();
        assert_eq!(doc.records.len(), 2);
        assert_eq!(doc.records[1].title, "Add Blender");
        let items = quick_scan(&doc.records, &[]);
        assert_eq!(items.iter().map(|i| i.app.as_str()).collect::<Vec<_>>(), vec!["Zoom", "Blender"]);

        let html = d.path().join("r.html");
        std::fs::write(&html, "<html><head><style>x{}</style></head><body><p>Please install:</p><ul><li>Zoom</li><li>Slack</li></ul></body></html>").unwrap();
        let doc = read_document(&html).unwrap();
        assert!(doc.records[0].body.contains("- Slack") || doc.records[0].title.contains("install"));
    }

    #[test]
    fn agent_items_are_validated() {
        let lib = library();
        let text = "```json\n{\"items\":[{\"source\":\"T1\",\"app\":\"QGIS\",\"request\":\"new\",\"confidence\":90},{\"source\":\"T2\",\"app\":\"Foo\",\"existingRecipe\":\"nope\",\"request\":\"update\"}]}\n```";
        let items = parse_agent_items(text, &lib).unwrap();
        assert_eq!(items[0].existing_recipe, "qgis");
        assert_eq!(items[0].request, RequestKind::Existing);
        assert!((items[0].confidence - 0.9).abs() < 1e-6);
        assert!(items[1].existing_recipe.is_empty());
        assert!(items[1].notes.contains("not in the library"));
        assert_eq!(items[1].suggested_id, "foo");
    }

    #[test]
    fn batch_round_trip_and_report() {
        let doc = Document { path: PathBuf::from(r"C:\t\x y.json"), format: "JSON".into(), records: vec![Record { reference: "T1".into(), title: "Package Zoom".into(), ..Default::default() }] };
        let b = Batch::new(doc, &[]);
        assert!(b.id.ends_with("-x-y"), "{}", b.id);
        assert_eq!(b.items[0].app, "Zoom");
        let json = serde_json::to_string(&b).unwrap();
        let back: Batch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items, b.items);
        assert!(b.report().contains("| T1 | Zoom |"));
        assert_eq!(timestamp().len(), 19);
    }

    #[test]
    fn harness_evidence_survives_reopen_and_overrides_agent_claims_in_report() {
        let mut b = Batch::new(Document {
            path: PathBuf::from("batch.txt"), format: "text".into(),
            records: vec![Record { reference: "T1".into(), title: "Package Zoom".into(), ..Default::default() }],
        }, &[]);
        b.items[0].status = ItemStatus::Done;
        b.items[0].checks.push(BatchCheck {
            recipe: "zoom".into(), passed: false, note: "Live check failed | missing detection".into(),
            timestamp: timestamp(), summary_dir: Some(PathBuf::from(r"C:\test\evidence")),
        });
        let back: Batch = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back.items[0].checks, b.items[0].checks);
        assert!(back.report().contains("zoom: Live check failed / missing detection"));
        assert!(!back.report().contains("recipe ready for live test"));
        let old: BatchItem = serde_json::from_str(r#"{"app":"Zoom","status":"done"}"#).unwrap();
        assert!(old.checks.is_empty());
    }

    #[test]
    fn agent_handoff_files() {
        let d = tempfile::tempdir().unwrap();
        let doc = Document { path: PathBuf::from(r"C:\t\x.json"), format: "JSON".into(), records: vec![Record { reference: "T1".into(), title: "Package Zoom".into(), ..Default::default() }] };
        let recipes = vec![recipe("qgis", "QGIS")];
        let mut b = Batch::new(doc, &recipes);
        std::fs::write(d.path().join(Batch::AGENT_OUTPUT), "stale").unwrap();
        b.prepare_agent(d.path(), &recipes).unwrap();
        assert!(std::fs::read_to_string(d.path().join("digest.md")).unwrap().contains("## T1 - Package Zoom"));
        assert!(std::fs::read_to_string(d.path().join("recipes.md")).unwrap().contains("qgis | QGIS"));
        assert!(b.load_agent_items(d.path(), &recipes, "x").is_err(), "stale output must be removed");
        std::fs::write(d.path().join(Batch::AGENT_OUTPUT), r#"{"items":[{"source":"T1","app":"QGIS","request":"new"},{"source":"T1","app":"Zoom Workplace","request":"new"}]}"#).unwrap();
        assert_eq!(b.load_agent_items(d.path(), &recipes, "GitHub Copilot CLI").unwrap(), 2);
        assert_eq!(b.items[0].existing_recipe, "qgis");
        assert_eq!(b.items[0].request, RequestKind::Existing);
        assert_eq!(b.extracted_by, "GitHub Copilot CLI");
    }
}
