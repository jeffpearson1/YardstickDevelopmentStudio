//! Self-update from GitHub Releases.
//!
//! The latest release's x64 MSI is downloaded, checked against the SHA-256
//! digest GitHub publishes for it, and installed by a small helper that waits
//! for the Studio to exit, runs the MSI (a major upgrade, so it replaces the
//! installed version in place) and starts the Studio again.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::harness::ps_quote;
use crate::process::ProcessSpec;

pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/jeffpearson1/YardstickDevelopmentStudio/releases/latest";
/// Install folder from `installer\Product.wxs`.
pub const INSTALL_FOLDER_NAME: &str = "Yardstick Development Studio";
pub const EXE_NAME: &str = "yardstick-studio.exe";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading `v`, e.g. `0.2.0`.
    pub version: String,
    pub notes: String,
    pub html_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub size: u64,
    /// Lower-case hex SHA-256 of the asset, from GitHub's `digest` field.
    pub sha256: Option<String>,
}

/// Picks the x64 MSI out of a GitHub "latest release" response.
pub fn parse_release(json: &str) -> Result<Release> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing the release")?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("release has no tag"))?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        bail!("the latest release is a draft or pre-release");
    }
    let asset = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.to_ascii_lowercase().ends_with("-x64.msi")))
        .ok_or_else(|| anyhow!("release {tag} has no x64 MSI"))?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: v["body"].as_str().unwrap_or_default().to_string(),
        html_url: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset_name: asset["name"].as_str().unwrap_or_default().to_string(),
        asset_url: asset["browser_download_url"].as_str().ok_or_else(|| anyhow!("MSI asset has no download URL"))?.to_string(),
        size: asset["size"].as_u64().unwrap_or(0),
        sha256,
    })
}

/// Numeric `major.minor.patch` comparison. A pre-release suffix (`1.2.0-rc1`)
/// sorts before the plain version.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn key(v: &str) -> Option<([u64; 3], bool)> {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let (core, pre) = match v.split_once(['-', '+']) {
            Some((c, _)) => (c, true),
            None => (v, false),
        };
        let mut parts = [0u64; 3];
        for (i, p) in core.split('.').enumerate() {
            if i >= 3 {
                break;
            }
            parts[i] = p.parse().ok()?;
        }
        Some((parts, !pre))
    }
    match (key(candidate), key(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent(concat!("YardstickDevelopmentStudio-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The latest release, if it is newer than `current`.
pub fn check(current: &str) -> Result<Option<Release>> {
    let mut resp = agent()
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!("checking for updates: {e}"))?;
    let body = resp.body_mut().with_config().limit(4 * 1024 * 1024).read_to_string().map_err(|e| anyhow!("checking for updates: {e}"))?;
    let release = parse_release(&body)?;
    Ok(is_newer(&release.version, current).then_some(release))
}

pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("YardstickStudio").join("updates")
}

/// Downloads the MSI into `dir`, reporting bytes received through `progress`,
/// and verifies its size and SHA-256. Returns the verified file.
pub fn download(release: &Release, dir: &Path, progress: &AtomicU64) -> Result<PathBuf> {
    let Some(expected) = &release.sha256 else {
        bail!("Release {} publishes no SHA-256 digest for its MSI, so it cannot be verified.", release.version);
    };
    if release.asset_name.contains(['/', '\\']) || !release.asset_name.to_ascii_lowercase().ends_with(".msi") {
        bail!("Unexpected installer name '{}'.", release.asset_name);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(&release.asset_name);
    let partial = dest.with_extension("msi.part");
    let mut resp = agent().get(&release.asset_url).call().map_err(|e| anyhow!("downloading {}: {e}", release.asset_name))?;
    let mut reader = resp.body_mut().with_config().limit(512 * 1024 * 1024).reader();
    let mut file = std::fs::File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| anyhow!("downloading {}: {e}", release.asset_name))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        total += n as u64;
        progress.store(total, Ordering::Relaxed);
    }
    file.flush()?;
    drop(file);
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if (release.size != 0 && total != release.size) || actual != *expected {
        let _ = std::fs::remove_file(&partial);
        bail!("The downloaded installer failed verification (SHA-256 {actual}, expected {expected}). Nothing was installed.");
    }
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&partial, &dest).with_context(|| format!("saving {}", dest.display()))?;
    Ok(dest)
}

/// Where the MSI installs the Studio.
pub fn installed_exe() -> PathBuf {
    let pf = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(pf).join(INSTALL_FOLDER_NAME).join(EXE_NAME)
}

/// The hidden helper that installs `msi` once process `pid` has exited, then
/// starts the Studio again with `--updated <msiexec exit code> <log>`.
/// `current_exe` is relaunched if the installed copy is missing (e.g. UAC was declined on a first install).
pub fn installer_helper(msi: &Path, pid: u32, current_exe: &Path) -> ProcessSpec {
    let log = msi.with_extension("install.log");
    let q = |p: &Path| ps_quote(&p.to_string_lossy());
    let script = format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         $msiArgs = '/i \"{msi}\" /passive /norestart /l*v \"{log}\"'; \
         try {{ $p = Start-Process -FilePath msiexec.exe -ArgumentList $msiArgs -Verb RunAs -Wait -PassThru; $code = $p.ExitCode }} \
         catch {{ $code = 1602 }}; \
         $exe = if (Test-Path -LiteralPath {installed}) {{ {installed} }} else {{ {current} }}; \
         Start-Process -FilePath $exe -ArgumentList '--updated', $code, ('\"' + {log_q} + '\"')",
        msi = msi.to_string_lossy().replace('\'', "''"),
        log = log.to_string_lossy().replace('\'', "''"),
        installed = q(&installed_exe()),
        current = q(current_exe),
        log_q = q(&log),
    );
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let ps = PathBuf::from(system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    ProcessSpec::new("Install update", ps, &std::env::temp_dir()).args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
}

/// What an msiexec exit code means for the user.
pub fn describe_msi_exit(code: i32) -> Option<&'static str> {
    match code {
        0 => None,
        3010 => Some("Updated; Windows needs a restart to finish."),
        1602 => Some("The update was cancelled."),
        1618 => Some("Another installation was in progress; try the update again later."),
        1638 => Some("This version is already installed."),
        _ => Some("The update failed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"tag_name":"v0.2.0","draft":false,"prerelease":false,"html_url":"https://github.com/x/y/releases/tag/v0.2.0","body":"Notes",
        "assets":[{"name":"notes.txt","browser_download_url":"https://x/notes.txt","size":1},
                  {"name":"YardstickDevelopmentStudio-0.2.0-x64.msi","browser_download_url":"https://x/a.msi","size":42,
                   "digest":"sha256:0754B845C19D10AA20C7322A9CE68D4CB01BC30E7962D43C389B16AA007DF5BB"}]}"#;

    #[test]
    fn parses_release_and_picks_msi() {
        let r = parse_release(JSON).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.asset_url, "https://x/a.msi");
        assert_eq!(r.size, 42);
        assert_eq!(r.sha256.as_deref(), Some("0754b845c19d10aa20c7322a9ce68d4cb01bc30e7962d43c389b16aa007df5bb"));
        assert!(parse_release(&JSON.replace("x64.msi", "x64.zip")).is_err());
        assert!(parse_release(&JSON.replace("\"prerelease\":false", "\"prerelease\":true")).is_err());
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn helper_waits_installs_and_relaunches() {
        let spec = installer_helper(Path::new(r"C:\Temp\It's\a.msi"), 1234, Path::new(r"C:\dev\yardstick-studio.exe"));
        let script = spec.args.last().unwrap();
        assert!(script.contains("Wait-Process -Id 1234"));
        assert!(script.contains("-Verb RunAs"));
        assert!(script.contains(r#"/i "C:\Temp\It''s\a.msi""#));
        assert!(script.contains("'--updated'"));
        assert!(script.contains(r"'C:\dev\yardstick-studio.exe'"));
    }

    #[test]
    fn helper_script_parses() {
        let spec = installer_helper(Path::new(r"C:\Temp\It's dir\a.msi"), 1234, Path::new(r"C:\dev dir\yardstick-studio.exe"));
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("helper.ps1");
        std::fs::write(&file, spec.args.last().unwrap()).unwrap();
        let check = format!(
            "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$null, [ref]$e); $e.Count",
            ps_quote(&file.to_string_lossy())
        );
        let out = crate::process::run_capture(&ProcessSpec::new("parse", spec.program.clone(), d.path()).args(["-NoProfile", "-NonInteractive", "-Command", &check])).unwrap();
        assert_eq!(out.trim(), "0", "{}", spec.args.last().unwrap());
    }

    #[test]
    fn refuses_unverifiable_download() {
        let mut r = parse_release(JSON).unwrap();
        r.sha256 = None;
        let d = tempfile::tempdir().unwrap();
        assert!(download(&r, d.path(), &AtomicU64::new(0)).is_err());
    }

    #[test]
    #[ignore = "network"]
    fn live_latest_release_is_well_formed() {
        let body = agent().get(LATEST_RELEASE_API).call().unwrap().body_mut().read_to_string().unwrap();
        let r = parse_release(&body).unwrap();
        assert!(r.sha256.is_some(), "{r:?}");
    }

    #[test]
    #[ignore = "network"]
    fn live_download_verifies() {
        let body = agent().get(LATEST_RELEASE_API).call().unwrap().body_mut().read_to_string().unwrap();
        let r = parse_release(&body).unwrap();
        let d = tempfile::tempdir().unwrap();
        let progress = AtomicU64::new(0);
        let msi = download(&r, d.path(), &progress).unwrap();
        assert_eq!(std::fs::metadata(&msi).unwrap().len(), r.size);
        assert_eq!(progress.load(Ordering::Relaxed), r.size);
        let mut bad = r.clone();
        bad.sha256 = Some("0".repeat(64));
        assert!(download(&bad, d.path(), &progress).unwrap_err().to_string().contains("verification"));
    }
}
