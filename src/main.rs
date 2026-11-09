//! FontRescue — the desktop app.
//!
//! Tray-first: the window is only a settings panel. A background thread watches
//! After Effects and Premiere window titles, reads the fonts a newly opened
//! project asks for, installs what is missing from the user's own archive, and
//! pops a notification.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

use fontrescue::adobe::{self, AdobeApp};
use fontrescue::fonts::{self, FontIndex, Installed};
use fontrescue::store::{HistoryEntry, InstalledItem, Store};
use fontrescue::watcher;

const TOAST_W: f64 = 396.0;
const TOAST_H: f64 = 150.0;
const TOAST_LIFETIME: Duration = Duration::from_secs(14);

/// Where the "Buy me a coffee" button sends people.
const DONATE_URL: &str = "https://paypal.me/maitfx";

// ---------------------------------------------------------------------------
// engine
// ---------------------------------------------------------------------------

struct Engine {
    store: Mutex<Store>,
    index: Mutex<FontIndex>,
    installed: Mutex<Installed>,
    busy: Mutex<String>,
    last_fix: Mutex<f64>,
    open_now: Mutex<HashMap<String, String>>,
    toast: Mutex<Option<ToastData>>,
    toast_gen: AtomicU64,
    running: AtomicBool,
}

#[derive(Debug, Clone, Serialize)]
struct ToastData {
    title: String,
    body: String,
    path: String,
    ok: bool,
    miss: usize,
    /// Only After Effects needs the project reopening; Premiere picks the
    /// fonts up on its own as soon as they are installed.
    reopen: bool,
}

#[derive(Serialize)]
struct WatchingItem {
    app: String,
    project: String,
}

#[derive(Serialize)]
struct Stats {
    fonts: usize,
    fixed: usize,
    last: String,
    installed: usize,
}

#[derive(Serialize)]
struct LibraryInfo {
    path: String,
    count: usize,
    ready: bool,
}

#[derive(Serialize)]
struct SettingsView {
    auto_install: bool,
    notify_missing: bool,
    start_with_windows: bool,
}

#[derive(Serialize)]
struct StatePayload {
    watching: Vec<WatchingItem>,
    stats: Stats,
    library: LibraryInfo,
    settings: SettingsView,
    setup_done: bool,
    busy: String,
    history: Vec<HistoryEntry>,
    version: String,
}

/// A notification only makes sense when something actually happened:
/// fonts were installed, or fonts are missing and the user asked to hear about it.
fn should_notify(entry: &HistoryEntry, notify_missing: bool) -> bool {
    if !entry.installed.is_empty() {
        return true;
    }
    notify_missing && (!entry.not_found.is_empty() || !entry.errors.is_empty())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn human_ago(then: f64) -> String {
    if then <= 0.0 {
        return "never".into();
    }
    let secs = (now_secs() - then).max(0.0) as u64;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

impl Engine {
    fn new() -> Self {
        let store = Store::load();
        let cache = store.dir.clone();
        Engine {
            index: Mutex::new(FontIndex::load(&cache)),
            store: Mutex::new(store),
            installed: Mutex::new(Installed::default()),
            busy: Mutex::new(String::new()),
            last_fix: Mutex::new(0.0),
            open_now: Mutex::new(HashMap::new()),
            toast: Mutex::new(None),
            toast_gen: AtomicU64::new(0),
            running: AtomicBool::new(true),
        }
    }

    fn cache_dir(&self) -> std::path::PathBuf {
        self.store.lock().unwrap().dir.clone()
    }

    fn state(&self) -> StatePayload {
        let store = self.store.lock().unwrap();
        let index = self.index.lock().unwrap();
        let installed = self.installed.lock().unwrap();
        let busy = self.busy.lock().unwrap().clone();
        let last_fix = *self.last_fix.lock().unwrap();

        let watching: Vec<WatchingItem> = watcher::open_windows()
            .into_iter()
            .map(|w| WatchingItem {
                project: watcher::project_name_from_title(&w.title).unwrap_or_default(),
                app: w.app,
            })
            .filter(|w| !w.project.is_empty())
            .collect();

        let history: Vec<HistoryEntry> = store.history.iter().take(12).cloned().collect();
        let last = if last_fix > 0.0 {
            last_fix
        } else {
            store.last_fix()
        };

        StatePayload {
            watching,
            stats: Stats {
                fonts: index.count,
                fixed: store.fixed_count(),
                last: human_ago(last),
                installed: installed.count,
            },
            library: LibraryInfo {
                ready: !store.settings.font_library.is_empty() && index.count > 0,
                path: store.settings.font_library.clone(),
                count: index.count,
            },
            settings: SettingsView {
                auto_install: store.settings.auto_install,
                notify_missing: store.settings.notify_missing,
                start_with_windows: store.settings.start_with_windows,
            },
            setup_done: store.settings.setup_done,
            busy,
            history,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Index the font library. `force` re-walks the folders; without it an
    /// index built for this same folder is reused as-is, so starting the app
    /// never costs a drive scan.
    fn index_library(&self, force: bool) {
        let folder = {
            let store = self.store.lock().unwrap();
            store.settings.font_library.clone()
        };
        if folder.is_empty() {
            return;
        }
        if !force {
            let index = self.index.lock().unwrap();
            if index.count > 0 && index.folder.eq_ignore_ascii_case(&folder) {
                return;
            }
        }
        *self.busy.lock().unwrap() = "Indexing font library…".into();
        // build into a fresh index so the window keeps answering while we walk
        let cache = self.cache_dir();
        let mut fresh = FontIndex::load(&cache);
        fresh.walk(Path::new(&folder), &cache);
        *self.index.lock().unwrap() = fresh;
        *self.busy.lock().unwrap() = String::new();
    }

    fn handle_project(&self, path: &Path, app_name: &str) -> Option<HistoryEntry> {
        let cache = self.cache_dir();
        let installed = self.installed.lock().unwrap();
        let mut index = self.index.lock().unwrap();
        let report = fontrescue::plan(path, &mut index, &installed, &cache);
        drop(index);
        drop(installed);

        let auto = self.store.lock().unwrap().settings.auto_install;
        let mut installed_items: Vec<InstalledItem> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for item in &report.resolved {
            let Some(face) = &item.face else { continue };
            if !auto {
                installed_items.push(InstalledItem {
                    name: item.name.clone(),
                    family: face.family.clone(),
                    path: face.path.clone(),
                });
                continue;
            }
            match fonts::install(face) {
                Ok(dest) => {
                    self.installed.lock().unwrap().remember(face);
                    installed_items.push(InstalledItem {
                        name: item.name.clone(),
                        family: face.family.clone(),
                        path: dest.display().to_string(),
                    });
                }
                Err(e) => errors.push(format!("{}: {e}", item.name)),
            }
        }

        let entry = HistoryEntry {
            project: path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: path.display().to_string(),
            app: app_name.to_string(),
            at: now_secs(),
            requested: report.project.fonts.len(),
            installed: installed_items,
            not_found: report.not_found.clone(),
            errors,
            detail: report.project.detail.clone(),
        };

        if !entry.installed.is_empty() {
            // "Last fix" is about fonts actually installed, not reports
            *self.last_fix.lock().unwrap() = now_secs();
        }
        if !entry.installed.is_empty() || !entry.not_found.is_empty() || !entry.errors.is_empty() {
            self.store.lock().unwrap().add_history(entry.clone());
        }
        Some(entry)
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_state(engine: State<'_, Arc<Engine>>) -> StatePayload {
    engine.state()
}

#[tauri::command]
fn get_toast(engine: State<'_, Arc<Engine>>) -> Option<ToastData> {
    engine.toast.lock().unwrap().clone()
}

#[tauri::command]
async fn pick_folder(app: AppHandle, engine: State<'_, Arc<Engine>>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let picked = app.dialog().file().blocking_pick_folder();
    let Some(picked) = picked else { return Ok(None) };
    let path = picked.to_string();

    {
        let mut store = engine.store.lock().unwrap();
        store.settings.font_library = path.clone();
        store.save_settings();
    }
    engine.index_library(true);
    Ok(Some(path))
}

#[tauri::command]
fn set_setting(engine: State<'_, Arc<Engine>>, key: String, value: bool) -> bool {
    {
        let mut store = engine.store.lock().unwrap();
        match key.as_str() {
            "auto_install" => store.settings.auto_install = value,
            "notify_missing" => store.settings.notify_missing = value,
            "start_with_windows" => store.settings.start_with_windows = value,
            _ => return false,
        }
        store.save_settings();
    }
    if key == "start_with_windows" {
        apply_startup(value);
    }
    true
}

#[tauri::command]
fn finish_setup(engine: State<'_, Arc<Engine>>) -> bool {
    let mut store = engine.store.lock().unwrap();
    store.settings.setup_done = true;
    store.save_settings();
    true
}

#[tauri::command]
fn rescan_library(engine: State<'_, Arc<Engine>>) -> bool {
    engine.index_library(true);
    true
}

#[tauri::command]
fn open_project(app: AppHandle, path: String) -> bool {
    let _ = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &path])
        .spawn();
    if let Some(win) = app.get_webview_window("toast") {
        let _ = win.close();
    }
    true
}

#[tauri::command]
fn dismiss_toast(app: AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("toast") {
        let _ = win.close();
    }
    true
}

/// Resize the notification to its content and keep it pinned bottom-right.
#[tauri::command]
fn fit_toast(app: AppHandle, height: f64) -> bool {
    let Some(win) = app.get_webview_window("toast") else {
        return false;
    };
    let scale = win.scale_factor().unwrap_or(1.0);
    let width = win
        .inner_size()
        .map(|s| s.width)
        .unwrap_or((TOAST_W * scale) as u32);
    let height = (height.clamp(80.0, 400.0) * scale).round().max(60.0) as u32;

    let _ = win.set_size(tauri::PhysicalSize::new(width, height));

    if let Ok(Some(monitor)) = win.current_monitor() {
        let size = monitor.size();
        let origin = monitor.position();
        let x = origin.x + size.width as i32 - width as i32 - (20.0 * scale) as i32;
        let y = origin.y + size.height as i32 - height as i32 - (56.0 * scale) as i32;
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
    true
}

/// Shrink or grow the window to exactly fit its content, within reason.
#[tauri::command]
fn fit_window(app: AppHandle, height: f64) -> bool {
    let Some(win) = app.get_webview_window("main") else {
        return false;
    };
    let scale = win.scale_factor().unwrap_or(1.0);
    let Ok(current) = win.inner_size() else {
        return false;
    };
    let current_height = current.height as f64 / scale;

    let available = win
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| (m.size().height as f64 / m.scale_factor()) - 140.0)
        .unwrap_or(760.0);

    let target = height.clamp(320.0, available.max(360.0));
    if (target - current_height).abs() > 8.0 {
        // keep the exact width the user has, only change the height
        let _ = win.set_size(tauri::PhysicalSize::new(
            current.width,
            (target * scale).round() as u32,
        ));
    }
    true
}

/// Put text on the Windows clipboard (no extra plugin needed).
#[cfg(windows)]
fn copy_to_clipboard(text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let target = GlobalLock(handle) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                let _ = GlobalUnlock(handle);
                let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(handle.0));
            }
        }
        let _ = CloseClipboard();
        true
    }
}

#[cfg(not(windows))]
fn copy_to_clipboard(_text: &str) -> bool {
    false
}

#[tauri::command]
fn copy_text(text: String) -> bool {
    copy_to_clipboard(&text)
}

/// Open the main window (used by the tray and by clicking the notification).
#[tauri::command]
fn show_window(app: AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    true
}

/// Open the donation page in the user's default browser.
#[tauri::command]
fn open_donate() -> bool {
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", DONATE_URL])
        .spawn()
        .is_ok()
}

#[tauri::command]
fn hide_window(app: AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    true
}

#[tauri::command]
fn quit_app(app: AppHandle, engine: State<'_, Arc<Engine>>) -> bool {
    engine.running.store(false, Ordering::SeqCst);
    app.exit(0);
    true
}

// ---------------------------------------------------------------------------
// toast
// ---------------------------------------------------------------------------

fn toast_position(app: &AppHandle) -> (f64, f64) {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let scale = monitor.scale_factor();
        let width = monitor.size().width as f64 / scale;
        let height = monitor.size().height as f64 / scale;
        return (width - TOAST_W - 20.0, height - TOAST_H - 60.0);
    }
    (1400.0, 700.0)
}

fn show_toast(app: &AppHandle, engine: &Arc<Engine>, entry: &HistoryEntry) {
    let installed = entry.installed.len();
    let missing = entry.not_found.len() + entry.errors.len();
    let premiere = entry.app.to_ascii_lowercase().contains("premiere")
        || entry.path.to_ascii_lowercase().ends_with(".prproj");

    let data = if installed > 0 && missing == 0 {
        ToastData {
            title: format!("Fonts ready for {}", entry.project),
            body: if premiere {
                format!(
                    "{} font{} installed. Premiere Pro uses {} right away.",
                    installed,
                    if installed == 1 { "" } else { "s" },
                    if installed == 1 { "it" } else { "them" }
                )
            } else {
                format!(
                    "{} font{} installed. Reopen the project so {} can load {}.",
                    installed,
                    if installed == 1 { "" } else { "s" },
                    entry.app,
                    if installed == 1 { "it" } else { "them" }
                )
            },
            path: entry.path.clone(),
            ok: true,
            miss: 0,
            reopen: !premiere,
        }
    } else if installed > 0 {
        ToastData {
            title: format!("{installed} installed, {missing} still missing"),
            body: format!(
                "{} — not on disk: {}",
                entry.project,
                entry.not_found.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ),
            path: entry.path.clone(),
            ok: true,
            miss: missing,
            reopen: !premiere,
        }
    } else {
        let names = if entry.not_found.is_empty() {
            entry
                .errors
                .iter()
                .map(|e| e.split(':').next().unwrap_or(e).to_string())
                .collect::<Vec<_>>()
        } else {
            entry.not_found.clone()
        };
        ToastData {
            title: format!("{} font{} not found", missing, if missing == 1 { "" } else { "s" }),
            body: format!(
                "{} needs {}",
                entry.project,
                names.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ),
            path: entry.path.clone(),
            ok: false,
            miss: missing,
            reopen: false,
        }
    };

    *engine.toast.lock().unwrap() = Some(data);

    if let Some(old) = app.get_webview_window("toast") {
        let _ = old.destroy();
    }
    let (x, y) = toast_position(app);
    let built = WebviewWindowBuilder::new(app, "toast", WebviewUrl::App("toast.html".into()))
        .title("FontRescue")
        .inner_size(TOAST_W, TOAST_H)
        .position(x, y)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .resizable(false)
        .background_color(tauri::utils::config::Color(25, 29, 35, 255))
        .shadow(true)
        .build();

    match built {
        Ok(win) => {
            // only auto-close if this notification is still the current one
            let generation = engine.toast_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let owner = engine.clone();
            let lifetime = std::env::var("FONTRESCUE_TOAST_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(TOAST_LIFETIME);
            std::thread::spawn(move || {
                std::thread::sleep(lifetime);
                if owner.toast_gen.load(Ordering::SeqCst) == generation {
                    let _ = win.close();
                }
            });
        }
        Err(e) => eprintln!("[fontrescue] toast window failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// background watcher
// ---------------------------------------------------------------------------

fn watch_loop(app: AppHandle, engine: Arc<Engine>) {
    // read what Windows already has (fast), then index the archive in parallel
    engine.installed.lock().unwrap().refresh();
    {
        let indexing = engine.clone();
        std::thread::spawn(move || indexing.index_library(false));
    }

    while engine.running.load(Ordering::SeqCst) {
        // Which project is open in each Adobe app right now?
        let mut seen: Vec<(String, String)> = Vec::new();
        for window in watcher::open_windows() {
            let Some(name) = watcher::project_name_from_title(&window.title) else {
                continue;
            };
            let which = if window.app.starts_with("Premiere") {
                AdobeApp::Premiere
            } else {
                AdobeApp::AfterEffects
            };
            // Prefer a path printed in the title (Premiere shows the full one);
            // otherwise ask Adobe's recent-project list.
            let resolved = watcher::project_path_from_title(&window.title)
                .or_else(|| adobe::resolve(which, &name));
            let Some(path) = resolved else {
                continue;
            };
            let key = path.display().to_string().replace('/', "\\");
            if !seen.iter().any(|(app_name, p)| app_name == &window.app && p == &key) {
                seen.push((window.app.clone(), key));
            }
        }

        // A project we have not already acted on this opening gets handled once —
        // even if the same project was fixed before. While it stays open, silence.
        for (app_name, key) in &seen {
            let already = {
                let open_now = engine.open_now.lock().unwrap();
                open_now.get(app_name) == Some(key)
            };
            if already {
                continue;
            }
            engine
                .open_now
                .lock()
                .unwrap()
                .insert(app_name.clone(), key.clone());

            if let Some(entry) = engine.handle_project(Path::new(key), app_name) {
                let notify_missing = engine.store.lock().unwrap().settings.notify_missing;
                if should_notify(&entry, notify_missing) {
                    show_toast(&app, &engine, &entry);
                }
            }
        }

        // forget projects that were closed, so reopening the same one notifies again
        engine.open_now.lock().unwrap().retain(|app_name, path| {
            seen.iter().any(|(a, p)| a == app_name && p == path)
        });

        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------
// windows startup entry
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn apply_startup(on: bool) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_SET_VALUE)
    else {
        return;
    };
    if on {
        let _ = key.set_value("FontRescue", &format!("\"{}\"", exe.display()));
    } else {
        let _ = key.delete_value("FontRescue");
    }
}

#[cfg(not(windows))]
fn apply_startup(_on: bool) {}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Keep only one copy running: a second launch exits quietly.
fn claim_single_instance(dir: &std::path::Path) -> bool {
    let path = dir.join("instance.lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    else {
        return true;
    };
    match file.try_lock() {
        Ok(()) => {
            // hold the lock for the whole process lifetime
            Box::leak(Box::new(file));
            true
        }
        Err(_) => false,
    }
}

fn main() {
    let engine = Arc::new(Engine::new());
    if !claim_single_instance(&engine.store.lock().unwrap().dir.clone()) {
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(engine.clone())
        .setup({
            let engine = engine.clone();
            move |app| {
                let handle = app.handle().clone();

                // tray
                let open = MenuItem::with_id(app, "open", "Open FontRescue", true, None::<&str>)?;
                let rescan =
                    MenuItem::with_id(app, "rescan", "Re-scan font library", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&open, &rescan, &quit])?;

                let tray_engine = engine.clone();
                let mut tray = TrayIconBuilder::with_id("fontrescue")
                    .tooltip("FontRescue — watching projects")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(move |app, event| match event.id.as_ref() {
                        "open" => {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        "rescan" => {
                            tray_engine.index_library(true);
                        }
                        "quit" => {
                            tray_engine.running.store(false, Ordering::SeqCst);
                            app.exit(0);
                        }
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                            let app = tray.app_handle();
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray = tray.icon(icon.clone());
                }
                tray.build(app)?;

                // show the window on first run (or when forced for development)
                let setup_done = engine.store.lock().unwrap().settings.setup_done;
                let force_show = std::env::var("FONTRESCUE_SHOW").map(|v| v == "1").unwrap_or(false);
                if !setup_done || force_show {
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.show();
                    }
                }

                if engine.store.lock().unwrap().settings.start_with_windows {
                    apply_startup(true);
                }

                // dev hook: FONTRESCUE_FAKE_TOAST=premiere|aftereffects previews the
                // notification without touching any project or font
                if let Ok(kind) = std::env::var("FONTRESCUE_FAKE_TOAST") {
                    let fake_engine = engine.clone();
                    let fake_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        let premiere = kind.to_ascii_lowercase().starts_with("premiere");
                        let entry = HistoryEntry {
                            project: if premiere {
                                "sample project.prproj".to_string()
                            } else {
                                "sample project.aep".to_string()
                            },
                            path: if premiere {
                                "C:\\Projects\\Demo\\sample project.prproj".to_string()
                            } else {
                                "C:\\Projects\\Demo\\sample project.aep".to_string()
                            },
                            app: if premiere { "Premiere Pro".to_string() } else { "After Effects".to_string() },
                            at: 0.0,
                            requested: 2,
                            installed: vec![
                                InstalledItem {
                                    name: "Gilroy-ExtraBold".to_string(),
                                    family: "Gilroy".to_string(),
                                    path: String::new(),
                                },
                                InstalledItem {
                                    name: "Gilroy-Light".to_string(),
                                    family: "Gilroy".to_string(),
                                    path: String::new(),
                                },
                            ],
                            not_found: Vec::new(),
                            errors: Vec::new(),
                            detail: String::new(),
                        };
                        show_toast(&fake_handle, &fake_engine, &entry);
                    });
                }

                // dev hook: FONTRESCUE_SIMULATE=<project> acts as if that project was opened
                if let Ok(sim_path) = std::env::var("FONTRESCUE_SIMULATE") {
                    let sim_engine = engine.clone();
                    let sim_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        sim_engine.installed.lock().unwrap().refresh();
                        sim_engine.index_library(true);
                        let sim_app = if sim_path.to_ascii_lowercase().ends_with(".prproj") {
                            "Premiere Pro"
                        } else {
                            "After Effects"
                        };
                        if let Some(entry) = sim_engine.handle_project(Path::new(&sim_path), sim_app)
                        {
                            let notify_missing =
                                sim_engine.store.lock().unwrap().settings.notify_missing;
                            println!(
                                "[fontrescue] simulate: {} installed, {} missing -> notify={}",
                                entry.installed.len(),
                                entry.not_found.len(),
                                should_notify(&entry, notify_missing)
                            );
                            if should_notify(&entry, notify_missing) {
                                show_toast(&sim_handle, &sim_engine, &entry);
                            }
                        }
                    });
                }

                std::thread::spawn(move || watch_loop(handle, engine));
                Ok(())
            }
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_toast,
            pick_folder,
            set_setting,
            finish_setup,
            rescan_library,
            open_project,
            dismiss_toast,
            hide_window,
            show_window,
            copy_text,
            open_donate,
            fit_window,
            fit_toast,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running FontRescue");
}
