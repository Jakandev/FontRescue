//! Finding a newly opened project by looking at Adobe's window titles.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct OpenWindow {
    pub app: String,
    pub exe: String,
    pub title: String,
}

fn app_for(exe: &str) -> Option<&'static str> {
    if exe.contains("afterfx") || exe.contains("aftereffects") {
        Some("After Effects")
    } else if exe.contains("premiere") {
        Some("Premiere Pro")
    } else {
        None
    }
}

/// The project file name inside a window title, if there is one.
pub fn project_name_from_title(title: &str) -> Option<String> {
    let lower = title.to_ascii_lowercase();
    let end = lower
        .find(".aep")
        .map(|i| i + 4)
        .or_else(|| lower.find(".prproj").map(|i| i + 7))?;
    let start = title[..end]
        .rfind(|c| c == '\\' || c == '/' || c == ':' || c == '|')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut name = title[start..end].trim();

    // "Adobe After Effects 2026 - Project.aep" -> "Project.aep"
    if !name.contains('\\') && !name.contains('/') {
        if let Some(pos) = name.find(" - ") {
            let stripped = name[pos + 3..].trim();
            if !stripped.is_empty() {
                name = stripped;
            }
        }
    }

    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// The full path when the window title carries one.
///
/// Premiere puts the whole path in its title; After Effects elides long paths
/// with " ... ", in which case this returns None and the recent-project list is
/// used instead.
pub fn project_path_from_title(title: &str) -> Option<PathBuf> {
    let lower = title.to_ascii_lowercase();
    let end = lower
        .find(".aep")
        .map(|i| i + 4)
        .or_else(|| lower.find(".prproj").map(|i| i + 7))?;

    let bytes = title.as_bytes();
    let mut start = None;
    for i in 0..end.saturating_sub(2) {
        if bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
        {
            let boundary = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'\\'
                    || bytes[i - 1] == b'/');
            if boundary {
                start = Some(i);
                break;
            }
        }
    }

    let candidate = PathBuf::from(title[start?..end].trim());
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(windows)]
pub fn open_windows() -> Vec<OpenWindow> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<OpenWindow>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; (len + 1) as usize];
            let written = GetWindowTextW(hwnd, &mut buf);
            if written <= 0 {
                return BOOL(1);
            }
            let title = String::from_utf16_lossy(&buf[..written as usize]);
            if project_name_from_title(&title).is_none() {
                return BOOL(1);
            }

            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return BOOL(1);
            }
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return BOOL(1);
            };
            let mut path_buf = vec![0u16; 1024];
            let mut size = path_buf.len() as u32;
            let exe = if QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(path_buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
            {
                let full = String::from_utf16_lossy(&path_buf[..size as usize]);
                Path::new(&full)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let _ = windows::Win32::Foundation::CloseHandle(handle);

            if let Some(app) = app_for(&exe) {
                out.push(OpenWindow {
                    app: app.to_string(),
                    exe,
                    title,
                });
            }
        }
        BOOL(1)
    }

    let mut found: Vec<OpenWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(callback),
            LPARAM(&mut found as *mut Vec<OpenWindow> as isize),
        );
    }
    found
}

#[cfg(not(windows))]
pub fn open_windows() -> Vec<OpenWindow> {
    Vec::new()
}
