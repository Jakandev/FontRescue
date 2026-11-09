//! Command-line harness: proves the parsing and matching before any UI exists.
//!
//!   fontrescue-probe index  <font folder>
//!   fontrescue-probe check  <project file> [more projects...]
//!   fontrescue-probe install <font folder> <project file>

use std::path::PathBuf;
use std::time::Instant;

use fontrescue::fonts::{self, FontIndex, Installed};
use fontrescue::{adobe, project, watcher};

fn cache_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    PathBuf::from(appdata).join("FontRescueRust")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: fontrescue-probe <index|check|install|watch> ...");
        std::process::exit(2);
    }
    let cache = cache_dir();
    let _ = std::fs::create_dir_all(&cache);

    match args[0].as_str() {
        "watch" => {
            for window in watcher::open_windows() {
                let which = if window.app.to_ascii_lowercase().contains("premiere") {
                    adobe::AdobeApp::Premiere
                } else {
                    adobe::AdobeApp::AfterEffects
                };
                let from_title = watcher::project_path_from_title(&window.title);
                let name = watcher::project_name_from_title(&window.title);
                println!("app={} title={:?}", window.app, window.title);
                println!("   name={:?}", name);
                println!("   title_path={:?}", from_title);
                match from_title.or_else(|| adobe::resolve(which, name.as_deref().unwrap_or_default())) {
                    Some(path) => {
                        let info = project::project_fonts(&path);
                        println!("   resolved={:?} fonts={} detail={}", path, info.fonts.len(), info.detail);
                    }
                    None => println!("   NOT RESOLVED"),
                }
            }
        }
        "index" => {
            let folder = PathBuf::from(&args[1]);
            let mut index = FontIndex::load(&cache);
            let start = Instant::now();
            let n = index.walk(&folder, &cache);
            println!(
                "indexed {n} font files in {:.2}s ({} distinct names)",
                start.elapsed().as_secs_f64(),
                index.by_stem.len()
            );
        }
        "check" => {
            let mut index = FontIndex::load(&cache);
            if index.count == 0 {
                eprintln!("index is empty — run `fontrescue-probe index <folder>` first");
            }
            let start = Instant::now();
            let mut installed = Installed::default();
            installed.refresh();
            println!(
                "installed: {} faces in {:.2}s",
                installed.count,
                start.elapsed().as_secs_f64()
            );

            for project in &args[1..] {
                let t = Instant::now();
                let report =
                    fontrescue::plan(std::path::Path::new(project), &mut index, &installed, &cache);
                println!("\n{}", report.project.path);
                println!("  {}", report.project.detail);
                println!(
                    "  {} fonts requested, {} already installed, {} missing  ({:.0} ms)",
                    report.project.fonts.len(),
                    report.project.fonts.len() - report.missing.len(),
                    report.missing.len(),
                    t.elapsed().as_secs_f64() * 1000.0
                );
                for item in &report.resolved {
                    if let Some(face) = &item.face {
                        println!(
                            "    FOUND  {:<28} -> {} / {}  [{}]",
                            item.name,
                            face.family,
                            face.subfamily,
                            PathBuf::from(&face.path)
                                .file_name()
                                .map(|f| f.to_string_lossy().to_string())
                                .unwrap_or_default()
                        );
                    }
                }
                for name in &report.not_found {
                    println!("    MISSING  {name}");
                }
            }
        }
        "install" => {
            let folder = PathBuf::from(&args[1]);
            let project = PathBuf::from(&args[2]);
            let mut index = FontIndex::load(&cache);
            if index.count == 0 {
                index.walk(&folder, &cache);
            }
            let mut installed = Installed::default();
            installed.refresh();
            let report = fontrescue::plan(&project, &mut index, &installed, &cache);
            for item in &report.resolved {
                if let Some(face) = &item.face {
                    match fonts::install(face) {
                        Ok(dest) => println!("installed {} -> {}", item.name, dest.display()),
                        Err(e) => println!("failed    {}: {e}", item.name),
                    }
                }
            }
            for name in &report.not_found {
                println!("not found {name}");
            }
        }
        "recent" => {
            use fontrescue::adobe::{self, AdobeApp};
            let which = args.get(1).map(|s| s.as_str()).unwrap_or("ae");
            let app = if which.starts_with("prem") {
                AdobeApp::Premiere
            } else {
                AdobeApp::AfterEffects
            };
            for (i, p) in adobe::recent_projects(app).iter().enumerate() {
                let exists = if p.is_file() { "" } else { "  (missing)" };
                println!("{i:2}  {}{exists}", p.display());
            }
            if let Some(name) = args.get(2) {
                println!("\nresolve {name:?} -> {:?}", adobe::resolve(app, name));
            }
        }
        "faces" => {
            for path in &args[1..] {
                let faces = fonts::read_faces(std::path::Path::new(path));
                println!("{path} -> {} face(s)", faces.len());
                for f in faces {
                    println!("   {} | {} | {} | {}", f.family, f.subfamily, f.full, f.ps);
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }
}
