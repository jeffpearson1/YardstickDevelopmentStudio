//! The plugins this repository ships must pass the Studio's own manifest checks,
//! so a manifest change cannot reach a release that the Studio would refuse.

use std::path::Path;

use ystudio_core::plugin::Manifest;

#[test]
fn every_shipped_plugin_manifest_is_valid() {
    let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    let mut checked = 0;
    for entry in std::fs::read_dir(&plugins).unwrap().flatten() {
        let dir = entry.path();
        let manifest = dir.join("plugin.yaml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).unwrap();
        let parsed = Manifest::parse(&text).unwrap_or_else(|e| panic!("{}: {e:#}", manifest.display()));
        parsed.validate(&|rel: &str| dir.join(rel).exists()).unwrap_or_else(|e| panic!("{}: {e:#}", manifest.display()));
        checked += 1;
    }
    assert!(checked >= 2, "found {checked} plugins in {}", plugins.display());
}
