//! The recipe field catalog. Mirrors `$knownFields`, `$validEnumerations` and
//! `$detectionTypeFields` in `Modules\YardstickSupport.psm1` (Test-RecipeSchema),
//! plus authoring guidance from the yardstick-recipes skill.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Identity,
    Source,
    Scripts,
    Install,
    Detection,
    Lifecycle,
    Advanced,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::Identity,
        Section::Source,
        Section::Scripts,
        Section::Install,
        Section::Detection,
        Section::Lifecycle,
        Section::Advanced,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Identity => "Identity",
            Section::Source => "Source & version",
            Section::Scripts => "Download scripts",
            Section::Install => "Install & uninstall",
            Section::Detection => "Detection",
            Section::Lifecycle => "Lifecycle & assignments",
            Section::Advanced => "Advanced",
        }
    }

    pub fn guidance(self) -> &'static str {
        match self {
            Section::Identity => {
                "Name the app. `id` must equal the file name exactly (lowercase). The icon must exist in Icons\\ \
                 under the exact iconFile name."
            }
            Section::Source => {
                "Escalate only as far as you must: static url + urlRedirects -> preDownloadScript scraping a vendor \
                 API -> GithubDownloader/JetbrainsDownloader -> reading the downloaded file -> full downloadScript. \
                 Licensed media goes in the software dropbox (manualDownload: true)."
            }
            Section::Scripts => {
                "$version must be set in YAML or preDownloadScript - setting it in downloadScript is too late and is \
                 the most common recipe bug. Never assign $temp, $buildSpace, $scripts, $recipes, $icons, $tools, \
                 $secrets, $prefs or $applications. Use Invoke-YardstickGitHubApi for api.github.com."
            }
            Section::Install => {
                "Simple MSI: msiexec /i <filename> /qn /norestart. Use powerShellInstallScript for ordering, \
                 prerequisites or exit-code mapping. <filename>, <version> and <productcode> are expanded for you."
            }
            Section::Detection => {
                "msi for a stable ProductCode (never hard-code the GUID); file+version for a known versioned EXE; \
                 registry for a stable ARP value; script for anything dynamic. Prefer -ge comparisons."
            }
            Section::Lifecycle => {
                "Supersedence and auto-update default from preferences.yaml - set them only deliberately. \
                 Never put scopeTags in a shared recipe; use Local\\recipeOverrides.yaml."
            }
            Section::Advanced => "Rarely needed. Everything here inherits sensible defaults.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// A PowerShell or cmd script block, emitted as a YAML literal block.
    Script,
    Enum(&'static [&'static str]),
    Bool,
    Int,
    /// A list of strings (group ids, scope tags).
    List,
    /// Structured data (additionalDetectionRules) - edited as raw YAML.
    Complex,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub name: &'static str,
    pub section: Section,
    pub kind: FieldKind,
    pub help: &'static str,
    /// Shown in the Designer by default even when empty.
    pub common: bool,
}

pub const DETECTION_TYPES: &[&str] = &["msi", "file", "script", "registry"];
pub const INSTALL_EXPERIENCES: &[&str] = &["system", "user"];
pub const RESTART_BEHAVIORS: &[&str] = &["allow", "suppress", "force", "basedOnReturnCode"];
pub const FILE_DETECTION_METHODS: &[&str] = &["version", "exists", "modified", "created", "size"];
pub const REGISTRY_DETECTION_METHODS: &[&str] = &["exists", "notExists", "string", "integer", "version"];
pub const OPERATORS: &[&str] = &["equal", "notEqual", "greaterThan", "greaterThanOrEqual", "lessThan", "lessThanOrEqual"];
pub const FILE_TYPES: &[&str] = &["msi", "exe", "zip"];

/// Fields the validator enforces as enumerations (case-insensitive).
pub const VALIDATED_ENUMS: &[(&str, &[&str])] = &[
    ("detectionType", DETECTION_TYPES),
    ("installExperience", INSTALL_EXPERIENCES),
    ("restartBehavior", RESTART_BEHAVIORS),
    ("fileDetectionMethod", FILE_DETECTION_METHODS),
    ("registryDetectionMethod", REGISTRY_DETECTION_METHODS),
];

pub const ALWAYS_REQUIRED: &[&str] = &["id", "displayName", "detectionType", "iconFile", "description", "publisher"];

/// Fields required for each `detectionType`.
pub fn detection_requirements(detection_type: &str) -> &'static [&'static str] {
    match detection_type.to_ascii_lowercase().as_str() {
        "file" => &["fileDetectionPath", "fileDetectionMethod", "fileDetectionName"],
        "registry" => &["registryDetectionMethod", "registryDetectionKey"],
        "script" => &["detectionScript"],
        _ => &[],
    }
}

/// Runner variables a pre/download/post script must never assign.
pub const RESERVED_VARIABLES: &[&str] = &[
    "temp",
    "buildspace",
    "scripts",
    "published",
    "recipes",
    "icons",
    "tools",
    "secrets",
    "softwaredropbox",
    "softwarearchive",
    "prefs",
    "applications",
];

pub const RETIRED_FIELDS: &[(&str, &str)] = &[
    (
        "dependentapplicationblacklist",
        "Field 'dependentApplicationBlacklist' has been renamed to 'dependentApplicationBlocklist'.",
    ),
    ("availablegroups", "Field 'availableGroups' has no effect. Use 'defaultDeploymentGroups'."),
    ("requiredgroups", "Field 'requiredGroups' has no effect. Add Required assignments in Intune."),
];

pub const PLACEHOLDERS: &[&str] = &["filename", "version", "productcode"];

/// Fields whose values have `<filename>`/`<version>`/`<productcode>` expanded.
pub const PLACEHOLDER_FIELDS: &[&str] = &[
    "installScript",
    "powerShellInstallScript",
    "uninstallScript",
    "powerShellUninstallScript",
    "detectionScript",
    "registryDetectionKey",
];

macro_rules! f {
    ($name:literal, $section:ident, $kind:expr, $common:literal, $help:literal) => {
        FieldSpec { name: $name, section: Section::$section, kind: $kind, help: $help, common: $common }
    };
}

use FieldKind::*;

/// Every field Yardstick understands, in canonical order.
pub const FIELDS: &[FieldSpec] = &[
    // Identity
    f!("id", Identity, Text, true, "Must equal the file name exactly, lowercase (e.g. 7zip for 7zip.yaml)."),
    f!("displayName", Identity, Text, true, "Name shown in Intune and Company Portal."),
    f!("publisher", Identity, Text, true, "Vendor name."),
    f!("description", Identity, Text, true, "One-line description shown in Company Portal."),
    f!("iconFile", Identity, Text, true, "File name in Icons\\ (.png/.jpg, <=512x512, <=750KB)."),
    f!("owner", Identity, Text, false, "Optional Intune owner field."),
    // Source
    f!("url", Source, Text, true, "Download URL or the landing page/API your preDownloadScript reads."),
    f!("urlRedirects", Source, Bool, true, "true when the vendor 302s to a versioned artifact; $url is the resolved URL afterwards."),
    f!("version", Source, Text, false, "Static version. Usually set in preDownloadScript instead."),
    f!("fileName", Source, Text, true, "Name of the final installer file."),
    f!("fileType", Source, Enum(FILE_TYPES), false, "Annotation only (msi/exe/zip); nothing reads it."),
    f!("softwareName", Source, Text, false, "Annotation only; match sibling recipes."),
    f!("manualDownload", Source, Bool, false, "Licensed media staged in the software dropbox instead of downloaded."),
    f!("manualDownloadFolder", Source, Text, false, "Dropbox sub-folder for manualDownload recipes."),
    f!("versionLock", Source, Text, false, "Pin versions: digits, dots and x wildcards, e.g. 3.11.x."),
    f!("minRunInterval", Source, Int, false, "Whole days between runs, for vendors that throttle requests."),
    // Scripts
    f!("preDownloadScript", Scripts, Script, true, "Runs first. The only script that may set $version. Resolve $url/$fileName here."),
    f!("downloadScript", Scripts, Script, false, "Full custom download (auth, cookies, Selenium). Working dir is $BuildSpace\\$id\\$version."),
    f!("postDownloadScript", Scripts, Script, false, "Inspect/extract the payload; may reassign $fileName."),
    f!("postRunScript", Scripts, Script, false, "Runs in the main loop's finally; must tolerate a recipe that never downloaded."),
    // Install
    f!("installScript", Install, Text, true, "cmd.exe command line, e.g. msiexec /i <filename> /qn /norestart."),
    f!("uninstallScript", Install, Text, true, "cmd.exe command line, e.g. msiexec /x <productcode> /qn /norestart."),
    f!("powerShellInstallScript", Install, Script, false, "Becomes install.ps1; $PSScriptRoot is the package root."),
    f!("powerShellUninstallScript", Install, Script, false, "Becomes uninstall.ps1."),
    f!("installExperience", Install, Enum(INSTALL_EXPERIENCES), true, "system (default) or user."),
    f!("restartBehavior", Install, Enum(RESTART_BEHAVIORS), true, "suppress is the norm."),
    f!("maximumInstallationTimeInMinutes", Install, Int, false, "Raise for long installers."),
    f!("minOSVersion", Install, Text, true, "W10_20H2 is the common floor."),
    f!("architecture", Install, Text, false, "Target architecture (x64, arm64, ...)."),
    f!("is32BitApp", Install, Bool, false, "Mark the app as 32-bit."),
    f!("allowUserUninstall", Install, Bool, false, "Allow uninstall from Company Portal."),
    // Detection
    f!("detectionType", Detection, Enum(DETECTION_TYPES), true, "msi | file | registry | script."),
    f!("fileDetectionPath", Detection, Text, false, "Folder containing the detected file."),
    f!("fileDetectionName", Detection, Text, false, "File name to detect."),
    f!("fileDetectionMethod", Detection, Enum(FILE_DETECTION_METHODS), false, "version is usual."),
    f!("fileDetectionOperator", Detection, Enum(OPERATORS), false, "Comparison operator, e.g. greaterThanOrEqual."),
    f!("fileDetectionVersion", Detection, Text, false, "Defaults to $version; set when the file version is padded (ToString(4))."),
    f!("fileDetectionDateTime", Detection, Text, false, "For modified/created methods."),
    f!("fileDetectionValue", Detection, Text, false, "For the size method."),
    f!("registryDetectionMethod", Detection, Enum(REGISTRY_DETECTION_METHODS), false, "exists | notExists | string | integer | version."),
    f!("registryDetectionKey", Detection, Text, false, "Full key path; placeholders allowed."),
    f!("registryDetectionValueName", Detection, Text, false, "Value name to compare."),
    f!("registryDetectionValue", Detection, Text, false, "Expected value."),
    f!("registryDetectionOperator", Detection, Enum(OPERATORS), false, "Comparison operator."),
    f!("detectionScript", Detection, Script, false, "Write-Output 'Detected'; exit 0 on match, 'Not Detected'; exit 1 otherwise."),
    f!("detectionScriptFileExtension", Detection, Text, false, "Defaults to ps1."),
    f!("detectionScriptRunAs32Bit", Detection, Bool, false, "Run the detection script as 32-bit."),
    f!("detectionScriptEnforceSignatureCheck", Detection, Bool, false, "Require a signed detection script."),
    f!("additionalDetectionRules", Detection, Complex, false, "Extra file/registry rules that must all match. Not allowed with a script primary."),
    // Lifecycle
    f!("supersedence", Lifecycle, Bool, false, "Newest version supersedes the ones Yardstick keeps."),
    f!("uninstallPreviousVersion", Lifecycle, Bool, false, "true = Replace, false = Update in place."),
    f!("autoUpdateOnAssignment", Lifecycle, Bool, false, "Intune native auto-update (Available assignments only)."),
    f!("autoUpdate", Lifecycle, Bool, false, "Legacy auto-update toggle."),
    f!("groupSkipAutoUpdates", Lifecycle, List, false, "Group ids that never auto-update (unions with preferences)."),
    f!("numVersionsToKeep", Lifecycle, Int, false, "How many versions stay in Intune."),
    f!("defaultDeploymentGroups", Lifecycle, List, false, "Available assignment groups for each new version."),
    f!("deadlineDateOffset", Lifecycle, Int, false, "Days until the Required deadline."),
    f!("availableDateOffset", Lifecycle, Int, false, "Days until the app becomes available."),
    f!("dependentApplicationBlocklist", Lifecycle, List, false, "Apps whose dependency links are never updated."),
    f!("allowDependentLinkUpdates", Lifecycle, Bool, false, "Update dependency links on other apps."),
    f!("dependentLinkUpdateEnabled", Lifecycle, Bool, false, "Enable dependent link updates."),
    f!("dependentLinkUpdateRetryCount", Lifecycle, Int, false, "Retries for dependent link updates."),
    f!("dependentLinkUpdateRetryDelaySeconds", Lifecycle, Int, false, "Delay between retries."),
    f!("dependentLinkUpdateTimeoutSeconds", Lifecycle, Int, false, "Timeout for dependent link updates."),
    f!("scopeTags", Lifecycle, List, false, "Tenant-specific - belongs in Local\\recipeOverrides.yaml, not here."),
    // Advanced
    f!("base", Advanced, Text, false, "Inherit every field from another recipe id, overriding only what differs."),
    f!("displayVersion", Advanced, Text, false, "Version string shown in Intune if different from $version."),
];

pub fn spec(name: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.name == name)
}

/// Case-insensitive lookup; returns the canonical spec.
pub fn spec_ci(name: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.name.eq_ignore_ascii_case(name))
}

pub fn fields_in(section: Section) -> impl Iterator<Item = &'static FieldSpec> {
    FIELDS.iter().filter(move |f| f.section == section)
}

/// Canonical position, used to order new keys the Designer adds.
pub fn canonical_index(name: &str) -> usize {
    FIELDS.iter().position(|f| f.name.eq_ignore_ascii_case(name)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The validator's `$knownFields` list, copied verbatim. If Yardstick adds a
    /// field, this test is the reminder to add it to the catalog too.
    const VALIDATOR_KNOWN_FIELDS: &[&str] = &[
        "url", "urlRedirects", "id", "version", "fileDetectionVersion", "displayName",
        "displayVersion", "fileName", "fileDetectionPath", "preDownloadScript",
        "downloadScript", "postDownloadScript", "postRunScript", "installScript",
        "uninstallScript", "powerShellInstallScript", "powerShellUninstallScript",
        "scopeTags", "owner", "maximumInstallationTimeInMinutes", "minOSVersion",
        "installExperience", "restartBehavior",
        "defaultDeploymentGroups", "allowUserUninstall", "is32BitApp", "architecture",
        "deadlineDateOffset", "availableDateOffset", "allowDependentLinkUpdates",
        "detectionType", "fileDetectionMethod", "fileDetectionName",
        "fileDetectionOperator", "fileDetectionDateTime", "fileDetectionValue",
        "registryDetectionMethod", "registryDetectionKey", "registryDetectionValueName",
        "registryDetectionValue", "registryDetectionOperator", "additionalDetectionRules", "detectionScript",
        "detectionScriptFileExtension", "detectionScriptRunAs32Bit",
        "detectionScriptEnforceSignatureCheck", "iconFile", "description", "publisher",
        "versionLock", "numVersionsToKeep", "fileType", "softwareName",
        "manualDownload", "manualDownloadFolder", "minRunInterval",
        "dependentApplicationBlocklist", "dependentLinkUpdateEnabled",
        "dependentLinkUpdateRetryCount", "dependentLinkUpdateRetryDelaySeconds",
        "dependentLinkUpdateTimeoutSeconds",
        "supersedence", "uninstallPreviousVersion", "autoUpdateOnAssignment", "autoUpdate",
        "groupSkipAutoUpdates",
        "base",
    ];

    #[test]
    fn catalog_matches_validator() {
        for k in VALIDATOR_KNOWN_FIELDS {
            assert!(spec(k).is_some(), "catalog is missing validator field {k}");
        }
        for f in FIELDS {
            assert!(VALIDATOR_KNOWN_FIELDS.contains(&f.name), "catalog field {} unknown to validator", f.name);
        }
        assert_eq!(FIELDS.len(), VALIDATOR_KNOWN_FIELDS.len());
    }

    #[test]
    fn detection_requirements_match_validator() {
        assert_eq!(detection_requirements("FILE").len(), 3);
        assert_eq!(detection_requirements("script"), &["detectionScript"]);
        assert!(detection_requirements("msi").is_empty());
    }
}
