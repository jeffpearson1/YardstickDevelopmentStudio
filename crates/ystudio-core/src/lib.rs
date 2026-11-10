//! Headless engine for Yardstick Development Studio.
//!
//! Everything here is GUI-agnostic and unit tested: the GUI crate only renders
//! state owned by these types and forwards user intent to them.

pub mod agent;
pub mod elevation;
pub mod fields;
pub mod git;
pub mod harness;
pub mod icons;
pub mod intake;
pub mod lint;
pub mod process;
pub mod promote;
pub mod recipe;
pub mod results;
pub mod settings;
pub mod setup;
pub mod update;
pub mod usage;
pub mod validation;
pub mod workspace;

pub use workspace::{Workspace, WorkspaceKind};

pub const DEFAULT_REPO_URL: &str = "https://github.com/jeffpearson1/Yardstick";
pub const DEFAULT_PRODUCTION_PATH: &str = r"C:\Intune\Yardstick";

/// Lower-cased, separator-normalised, trailing-slash-free path for comparisons.
pub fn normalize_path(path: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut s = resolved.to_string_lossy().replace('/', "\\");
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    while s.ends_with('\\') && s.len() > 3 {
        s.pop();
    }
    s.to_lowercase()
}

pub fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    normalize_path(a) == normalize_path(b)
}

/// Reads a text file, tolerating a UTF-8 BOM (Windows PowerShell 5.1 writes one).
pub fn read_text(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}
