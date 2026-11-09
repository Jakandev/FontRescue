//! FontRescue core: reading project fonts, matching and installing them.

pub mod adobe;
pub mod fonts;
pub mod project;
pub mod store;
pub mod watcher;

use std::path::Path;

use fonts::{FontIndex, Installed};
use project::ProjectInfo;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Planned {
    pub name: String,
    pub face: Option<fonts::Face>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub project: ProjectInfo,
    pub missing: Vec<String>,
    pub resolved: Vec<Planned>,
    pub not_found: Vec<String>,
}

/// Work out what a project needs without touching the system.
pub fn plan(
    project_path: &Path,
    index: &mut FontIndex,
    installed: &Installed,
    cache_dir: &Path,
) -> Report {
    let info = project::project_fonts(project_path);
    let missing: Vec<String> = info
        .fonts
        .iter()
        .filter(|f| !installed.has(f))
        .cloned()
        .collect();

    let mut resolved = Vec::new();
    let mut not_found = Vec::new();
    for name in &missing {
        match index.find(name, cache_dir) {
            Some(face) => resolved.push(Planned {
                name: name.clone(),
                face: Some(face),
            }),
            None => not_found.push(name.clone()),
        }
    }

    Report {
        project: info,
        missing,
        resolved,
        not_found,
    }
}
