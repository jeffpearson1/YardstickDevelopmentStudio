//! The plugins of this session: what is installed, their sidebar tabs, page
//! state, and the work-item locks held by running steps. Installing and
//! running live in `ystudio_core::plugin`; the pages in `views::plugin_page`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ystudio_core::plugin::{self, Installed, Lock, Registry};

use crate::app::View;
use crate::views::plugin_page::PageState;
use crate::views::update::CURRENT_VERSION;

pub struct Host {
    pub dir: PathBuf,
    pub registry: Registry,
    /// Plugin ids in tab order. `View::Plugin(i)` indexes this. Entries stay
    /// for the session, so an uninstall never shifts another tab's index.
    pub tabs: Vec<String>,
    pub pages: HashMap<String, PageState>,
    /// Work-item locks of running steps, by job id; dropped when the job ends.
    pub locks: HashMap<u64, Lock>,
}

impl Host {
    pub fn load(dir: &Path) -> Self {
        let mut host = Self { dir: dir.to_path_buf(), registry: Registry::default(), tabs: Vec::new(), pages: HashMap::new(), locks: HashMap::new() };
        host.reload();
        host
    }

    /// Re-reads the plugins folder after an install, update or uninstall.
    pub fn reload(&mut self) {
        self.registry = Registry::discover(&self.dir, CURRENT_VERSION);
        for p in &self.registry.plugins {
            if !self.tabs.contains(&p.manifest.id) {
                self.tabs.push(p.manifest.id.clone());
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Installed> {
        self.registry.get(id)
    }

    pub fn view_of(&self, id: &str) -> Option<View> {
        self.tabs.iter().position(|t| t == id).map(|i| View::Plugin(i as u16))
    }

    pub fn page(&mut self, id: &str) -> &mut PageState {
        self.pages.entry(id.to_string()).or_default()
    }
}

impl Default for Host {
    fn default() -> Self {
        Self { dir: plugin::default_dir(), registry: Registry::default(), tabs: Vec::new(), pages: HashMap::new(), locks: HashMap::new() }
    }
}
