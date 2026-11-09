//! Reading the fonts out of After Effects and Premiere project files.
//!
//! `.aep` is a RIFX container (usually gzipped). Every font it uses is written
//! into an embedded PostScript resource:
//!
//! ```text
//! /99 /CoolTypeFont /0 << /0 (FEFF <name as UTF-16BE>) /2 0 ...
//! ```
//!
//! `.prproj` carries no font names at all, so Premiere is handled through the
//! `.mogrt` templates a project links to (zip -> project.aegraphic zip -> .aep).

use std::io::Read;
use std::path::{Path, PathBuf};

use regex::Regex;

use serde::Serialize;

/// Names that are placeholders and never worth resolving.
const SKIP: [&str; 3] = ["adobeinvisfont", "adobesongstdlight", "notdef"];

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    pub path: String,
    pub source: String,
    pub detail: String,
    pub fonts: Vec<String>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn inflate(raw: &[u8]) -> Vec<u8> {
    if raw.len() > 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(raw).read_to_end(&mut out).is_ok() {
            return out;
        }
    }
    raw.to_vec()
}

pub fn read_project(path: &Path) -> Vec<u8> {
    match std::fs::read(path) {
        Ok(raw) => inflate(&raw),
        Err(_) => Vec::new(),
    }
}

/// PostScript names referenced by an After Effects project.
pub fn fonts_in_aep(data: &[u8]) -> Vec<String> {
    const MARKER: &[u8] = b"/CoolTypeFont";
    let mut out = Vec::new();
    let mut cursor = 0usize;

    while cursor < data.len() {
        let Some(rel) = find(&data[cursor..], MARKER) else { break };
        let start = cursor + rel;
        let end = (start + 400).min(data.len());
        let seg = &data[start..end];

        if let Some(open) = find(seg, b"(\xFE\xFF") {
            let body = open + 3;
            if let Some(close) = seg[body..].iter().position(|&b| b == b')') {
                let raw = &seg[body..body + close];
                let usable = raw.len() - (raw.len() % 2);
                let mut name = String::new();
                for pair in raw[..usable].chunks_exact(2) {
                    let unit = u16::from_be_bytes([pair[0], pair[1]]);
                    if let Some(c) = char::from_u32(unit as u32) {
                        name.push(c);
                    }
                }
                let name = name.trim().to_string();
                if !name.is_empty() && name.chars().count() < 120 {
                    out.push(name);
                }
            }
        }
        cursor = start + MARKER.len();
    }
    out
}


/// After Effects bytes out of a `.mogrt` or a bare `.aegraphic`.
///
/// Both are zip archives: a `.mogrt` holds `project.aegraphic` (itself a zip
/// with an `.aep` inside), a `.aegraphic` holds the `.aep` directly.
pub fn aep_bytes_from_graphic(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut data = {
        let mut zip = zip::ZipArchive::new(file).ok()?;
        let mut found: Option<Vec<u8>> = None;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).ok()?;
            let name = entry.name().to_ascii_lowercase();
            if name.ends_with(".aep") || name.ends_with(".aegraphic") {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).ok()?;
                found = Some(buf);
                if name.ends_with(".aep") {
                    break; // a direct .aep is the best possible hit
                }
            }
        }
        found?
    };

    if data.starts_with(b"PK\x03\x04") {
        data = aep_bytes_from_zip(data)?;
    }
    Some(inflate(&data))
}

fn aep_bytes_from_zip(data: Vec<u8>) -> Option<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).ok()?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).ok()?;
        if entry.name().to_ascii_lowercase().ends_with(".aep") {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).ok()?;
            return Some(buf);
        }
    }
    None
}

pub fn fonts_in_graphic(path: &Path) -> Vec<String> {
    match aep_bytes_from_graphic(path) {
        Some(data) => fonts_in_aep(&data),
        None => Vec::new(),
    }
}

pub fn fonts_in_mogrt(path: &Path) -> Vec<String> {
    fonts_in_graphic(path)
}

/// `.mogrt` files a Premiere project points at, resolved on disk.
pub fn linked_mogrts(prproj: &Path, xml: &str, limit: usize) -> Vec<PathBuf> {
    let re = regex::Regex::new(r#"(?i)([^<>":]+\.mogrt)"#).expect("static pattern");
    let base = prproj.parent().unwrap_or_else(|| Path::new("."));
    let mut found: Vec<PathBuf> = Vec::new();

    for cap in re.captures_iter(xml) {
        let raw = cap[1].trim();
        if raw.is_empty() {
            continue;
        }
        let direct = PathBuf::from(raw);
        let candidates = [
            direct.clone(),
            base.join(raw.trim_start_matches(".\\").trim_start_matches("./")),
            base.join(direct.file_name().unwrap_or_default()),
        ];
        for candidate in candidates {
            if candidate.is_file() {
                if !found.iter().any(|p| p == &candidate) {
                    found.push(candidate);
                }
                break;
            }
        }
        if found.len() >= limit {
            break;
        }
    }
    found
}

fn clean(mut names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names.drain(..) {
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let squashed: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if SKIP.iter().any(|s| *s == squashed) {
            continue;
        }
        if !out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}


/// `.aegraphic` and `.mogrt` files a Premiere project references.
fn referenced_graphics(prproj: &Path, xml: &str, limit: usize) -> Vec<PathBuf> {
    let re = Regex::new(r#"(?i)([A-Za-z]:[\\/][^<>"]*\.(?:aegraphic|mogrt))"#)
        .expect("static pattern");
    let base = prproj.parent().unwrap_or_else(|| Path::new("."));
    let mut found: Vec<PathBuf> = Vec::new();

    for cap in re.captures_iter(xml) {
        let raw = cap[1].to_string();
        let mut candidates = vec![PathBuf::from(&raw)];
        let undoubled = raw.replace("\\\\", "\\");
        if undoubled != raw {
            candidates.push(PathBuf::from(undoubled));
        }
        if let Some(name) = Path::new(&raw).file_name() {
            candidates.push(base.join(name));
        }
        if let Some(path) = candidates.into_iter().find(|c| c.is_file()) {
            if !found.iter().any(|p| p == &path) {
                found.push(path);
            }
        }
        if found.len() >= limit {
            break;
        }
    }
    found
}

pub fn project_fonts(path: &Path) -> ProjectInfo {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let mut info = ProjectInfo {
        path: path.display().to_string(),
        source: ext.clone(),
        detail: String::new(),
        fonts: Vec::new(),
    };

    if !path.is_file() {
        info.detail = "file not found".into();
        return info;
    }

    match ext.as_str() {
        "aep" => {
            info.fonts = fonts_in_aep(&read_project(path));
            info.detail = "After Effects project".into();
        }
        "mogrt" => {
            info.fonts = fonts_in_mogrt(path);
            info.detail = "Motion Graphics template".into();
        }
        "prproj" => {
            let data = read_project(path);
            let xml = String::from_utf8_lossy(&data).to_string();
            let mut fonts = Vec::new();

            // Premiere writes its text styles into graphics, which it keeps on
            // disk as .aegraphic (or links as .mogrt). Both hold an .aep inside.
            let graphics = referenced_graphics(path, &xml, 60);
            for graphic in &graphics {
                fonts.extend(fonts_in_graphic(graphic));
            }
            info.fonts = fonts;
            info.detail = if graphics.is_empty() {
                "Premiere project · no text graphics found".into()
            } else {
                format!("Premiere project · {} graphic(s) read", graphics.len())
            };
        }
        _ => {
            info.detail = "unsupported file type".into();
            return info;
        }
    }

    info.fonts = clean(info.fonts);
    info
}
