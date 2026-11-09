//! Settings and history, kept in %APPDATA%\FontRescueRust.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_library: String,
    pub auto_install: bool,
    pub notify_missing: bool,
    pub start_with_windows: bool,
    pub setup_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_library: String::new(),
            auto_install: true,
            notify_missing: true,
            start_with_windows: true,
            setup_done: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledItem {
    pub name: String,
    pub family: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub project: String,
    pub path: String,
    pub app: String,
    pub at: f64,
    pub requested: usize,
    pub installed: Vec<InstalledItem>,
    pub not_found: Vec<String>,
    pub errors: Vec<String>,
    pub detail: String,
}

#[derive(Debug)]
pub struct Store {
    pub dir: PathBuf,
    pub settings: Settings,
    pub history: Vec<HistoryEntry>,
}

impl Store {
    pub fn load() -> Self {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        let dir = PathBuf::from(appdata).join("FontRescueRust");
        let _ = std::fs::create_dir_all(&dir);

        let settings = std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
            .unwrap_or_default();

        let mut history: Vec<HistoryEntry> = std::fs::read_to_string(dir.join("history.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // one row per project: drop older copies left by earlier versions
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        history.retain(|e| seen.insert(e.path.replace('/', "\\").to_ascii_lowercase()));
        history.truncate(200);

        Store { dir, settings, history }
    }

    pub fn save_settings(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.settings) {
            let _ = std::fs::write(self.dir.join("settings.json"), text);
        }
    }

    /// One row per project: re-reporting a project refreshes its row instead of
    /// stacking a duplicate.
    pub fn add_history(&mut self, entry: HistoryEntry) {
        let same_project = |a: &HistoryEntry, b: &HistoryEntry| {
            a.path.replace('/', "\\").eq_ignore_ascii_case(&b.path.replace('/', "\\"))
        };
        if let Some(index) = self
            .history
            .iter()
            .position(|existing| same_project(existing, &entry))
        {
            self.history.remove(index);
        }
        self.history.insert(0, entry);
        self.history.truncate(200);
        if let Ok(text) = serde_json::to_string_pretty(&self.history) {
            let _ = std::fs::write(self.dir.join("history.json"), text);
        }
    }

    pub fn fixed_count(&self) -> usize {
        self.history
            .iter()
            .filter(|h| !h.installed.is_empty())
            .count()
    }

    pub fn last_fix(&self) -> f64 {
        self.history
            .iter()
            .find(|h| !h.installed.is_empty())
            .map(|h| h.at)
            .unwrap_or(0.0)
    }
}
