//! `MicrosoftGame.Config` XML manifest parser for Xbox Series X|S titles.
//!
//! Supports UTF-8 (with or without BOM) and UTF-16 LE BOM manifests, extracting:
//! - `<Identity Name=... Publisher=... Version=.../>`
//! - `<TitleId>` (raw hex string and parsed `u32`) and `<StoreId>`
//! - `<ShellVisuals DefaultDisplayName=.../>`
//! - `<Executable>` entries (prioritizing `XboxGen9` / `Scarlett` targets)
//! - `<PersistentLocalStorage><SizeMB>` (`S:\` quota)
//! - `<XboxAnacondaTitleMemory>` (`Standard` vs `Advanced`) & `<XboxLockhartTitleMemory>`

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Io(String),
    Parse(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) => write!(f, "manifest I/O error: {m}"),
            Self::Parse(m) => write!(f, "manifest parse error: {m}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameExecutableEntry {
    pub name: String,
    pub id: String,
    pub target_device_family: String,
    pub alias: String,
    pub is_dev_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleMemoryMode {
    Standard,
    Advanced,
}

impl Default for TitleMemoryMode {
    fn default() -> Self {
        Self::Standard
    }
}

impl TitleMemoryMode {
    /// Approximate usable title RAM budget on Xbox Series X (Anaconda).
    pub fn anaconda_budget_bytes(self) -> u64 {
        match self {
            Self::Standard => 13_500 * 1024 * 1024, // ~13.5 GiB standard title partition
            Self::Advanced => 14_000 * 1024 * 1024, // ~14.0 GiB expanded mode
        }
    }
}

/// Parsed Xbox GDK `MicrosoftGame.Config` package manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameConfig {
    pub config_version: String,
    pub identity_name: String,
    pub publisher: String,
    pub version: String,
    pub title_id: String,
    pub title_id_u32: Option<u32>,
    pub store_id: String,
    pub display_name: String,
    pub executables: Vec<GameExecutableEntry>,
    pub persistent_local_storage_mb: Option<u32>,
    pub anaconda_memory_mode: TitleMemoryMode,
    pub lockhart_memory_mode: TitleMemoryMode,
}

impl GameConfig {
    /// Load `MicrosoftGame.Config` from a specific file path.
    pub fn load_file(path: &Path) -> Result<Self, ManifestError> {
        let raw = fs::read(path).map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        let text = decode_xml_bytes(&raw)?;
        Self::parse_xml(&text)
    }

    /// Locate and load `MicrosoftGame.Config` (case-insensitive) inside an unpacked package root.
    pub fn load_from_package_dir(dir: &Path) -> Result<(Self, PathBuf), ManifestError> {
        let direct = dir.join("MicrosoftGame.Config");
        if direct.is_file() {
            return Ok((Self::load_file(&direct)?, direct));
        }
        let entries = fs::read_dir(dir)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.eq_ignore_ascii_case("MicrosoftGame.Config") {
                        return Ok((Self::load_file(&p)?, p));
                    }
                }
            }
        }
        Err(ManifestError::Io(format!(
            "MicrosoftGame.Config not found in {}",
            dir.display()
        )))
    }

    /// Parse raw XML string into a [`GameConfig`].
    pub fn parse_xml(xml: &str) -> Result<Self, ManifestError> {
        if !xml.contains("<Game") {
            return Err(ManifestError::Parse(
                "missing <Game> root element in MicrosoftGame.Config".into(),
            ));
        }

        let mut cfg = Self::default();

        if let Some(tag) = extract_tag_block(xml, "Game", 0).map(|(t, _)| t) {
            cfg.config_version = extract_attr(tag, "configVersion").unwrap_or_default();
        }
        if let Some(tag) = extract_tag_block(xml, "Identity", 0).map(|(t, _)| t) {
            cfg.identity_name = extract_attr(tag, "Name").unwrap_or_default();
            cfg.publisher = extract_attr(tag, "Publisher").unwrap_or_default();
            cfg.version = extract_attr(tag, "Version").unwrap_or_default();
        }
        if let Some(tid) = extract_element_text(xml, "TitleId") {
            cfg.title_id_u32 = parse_title_id(&tid);
            cfg.title_id = tid;
        }
        if let Some(sid) = extract_element_text(xml, "StoreId") {
            cfg.store_id = sid;
        }
        if let Some(tag) = extract_tag_block(xml, "ShellVisuals", 0).map(|(t, _)| t) {
            cfg.display_name = extract_attr(tag, "DefaultDisplayName").unwrap_or_default();
        }

        let mut cursor = 0;
        while let Some((exe_tag, next)) = extract_tag_block(xml, "Executable", cursor) {
            cursor = next;
            let name = extract_attr(exe_tag, "Name").unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let id = extract_attr(exe_tag, "Id").unwrap_or_default();
            let target_device_family = extract_attr(exe_tag, "TargetDeviceFamilyForXboxOne")
                .or_else(|| extract_attr(exe_tag, "TargetDeviceFamily"))
                .unwrap_or_default();
            let alias = extract_attr(exe_tag, "Alias").unwrap_or_default();
            let is_dev_only = extract_attr(exe_tag, "IsDevOnly")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            cfg.executables.push(GameExecutableEntry {
                name,
                id,
                target_device_family,
                alias,
                is_dev_only,
            });
        }

        if let Some(pls_mb) = extract_element_text(xml, "SizeMB") {
            if let Ok(mb) = pls_mb.parse::<u32>() {
                cfg.persistent_local_storage_mb = Some(mb);
            }
        }
        if let Some(mode) = extract_element_text(xml, "XboxAnacondaTitleMemory") {
            if mode.eq_ignore_ascii_case("Advanced") {
                cfg.anaconda_memory_mode = TitleMemoryMode::Advanced;
            }
        }
        if let Some(mode) = extract_element_text(xml, "XboxLockhartTitleMemory") {
            if mode.eq_ignore_ascii_case("Advanced") {
                cfg.lockhart_memory_mode = TitleMemoryMode::Advanced;
            }
        }

        Ok(cfg)
    }

    /// Return the primary Xbox Series X|S (`XboxGen9` / `Scarlett`) executable, falling back
    /// to the first non-dev-only entry or the first listed entry.
    pub fn primary_executable(&self) -> Option<&GameExecutableEntry> {
        if let Some(exe) = self.executables.iter().find(|e| {
            let t = e.target_device_family.to_ascii_lowercase();
            (t == "xboxgen9" || t == "scarlett") && !e.is_dev_only
        }) {
            return Some(exe);
        }
        if let Some(exe) = self.executables.iter().find(|e| {
            let t = e.target_device_family.to_ascii_lowercase();
            t == "xboxgen9" || t == "scarlett"
        }) {
            return Some(exe);
        }
        self.executables
            .iter()
            .find(|e| !e.is_dev_only)
            .or_else(|| self.executables.first())
    }

    /// Resolve the primary executable path relative to an unpacked package root directory.
    pub fn resolve_primary_executable_path(&self, package_dir: &Path) -> Option<PathBuf> {
        let exe = self.primary_executable()?;
        let rel = exe.name.replace('\\', "/");
        let direct = package_dir.join(&rel);
        if direct.is_file() {
            return Some(direct);
        }
        // Fallback: case-insensitive match in root directory
        let file_only = Path::new(&rel).file_name()?.to_str()?;
        if let Ok(entries) = fs::read_dir(package_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                        if n.eq_ignore_ascii_case(file_only) {
                            return Some(p);
                        }
                    }
                }
            }
        }
        Some(direct)
    }
}

fn decode_xml_bytes(raw: &[u8]) -> Result<String, ManifestError> {
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let u16s: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&u16s)
            .map_err(|e| ManifestError::Parse(format!("invalid UTF-16LE XML: {e}")));
    }
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let u16s: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&u16s)
            .map_err(|e| ManifestError::Parse(format!("invalid UTF-16BE XML: {e}")));
    }
    let slice = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        raw
    };
    Ok(String::from_utf8_lossy(slice).into_owned())
}

fn parse_title_id(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u32::from_str_radix(hex, 16).ok()
}

fn decode_xml_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn find_tag_end(xml: &str, mut pos: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    while pos < bytes.len() {
        match bytes[pos] {
            b'>' => return Some(pos),
            b'"' | b'\'' => {
                let q = bytes[pos];
                pos += 1;
                while pos < bytes.len() && bytes[pos] != q {
                    pos += 1;
                }
            }
            _ => {}
        }
        pos += 1;
    }
    None
}

fn extract_tag_block<'a>(xml: &'a str, element: &str, from: usize) -> Option<(&'a str, usize)> {
    let prefix = format!("<{element}");
    let mut search = from;
    while let Some(rel) = xml[search..].find(&prefix) {
        let pos = search + rel;
        let after = pos + prefix.len();
        if after < xml.len() {
            let next_b = xml.as_bytes()[after];
            if next_b.is_ascii_whitespace() || next_b == b'>' || next_b == b'/' {
                let end = find_tag_end(xml, after)?;
                return Some((&xml[pos..=end], end + 1));
            }
        }
        search = after;
    }
    None
}

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut pos = tag.find(|c: char| c.is_ascii_whitespace())?;
    while pos < bytes.len() {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] == b'>' || bytes[pos] == b'/' {
            return None;
        }
        let name_start = pos;
        while pos < bytes.len()
            && !bytes[pos].is_ascii_whitespace()
            && bytes[pos] != b'='
            && bytes[pos] != b'>'
            && bytes[pos] != b'/'
        {
            pos += 1;
        }
        let name = &tag[name_start..pos];
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'=' {
            return None;
        }
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || (bytes[pos] != b'"' && bytes[pos] != b'\'') {
            return None;
        }
        let quote = bytes[pos];
        pos += 1;
        let val_start = pos;
        while pos < bytes.len() && bytes[pos] != quote {
            pos += 1;
        }
        if pos >= bytes.len() {
            return None;
        }
        let val = &tag[val_start..pos];
        pos += 1;
        if name == attr_name {
            return Some(decode_xml_entities(val));
        }
    }
    None
}

fn extract_element_text(xml: &str, element: &str) -> Option<String> {
    let (_, after_open) = extract_tag_block(xml, element, 0)?;
    let close = format!("</{element}>");
    let end = xml[after_open..].find(&close)?;
    Some(decode_xml_entities(xml[after_open..after_open + end].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="SampleScarlettTitle" Publisher="CN=XsxEmu" Version="1.2.3.0"/>
  <TitleId>7A5F1234</TitleId>
  <StoreId>9NBLGGH4TEST</StoreId>
  <ShellVisuals DefaultDisplayName="Sample &amp; Test Game"/>
  <ExecutableList>
    <Executable Name="GameOne.exe" Id="One" TargetDeviceFamily="XboxOne"/>
    <Executable Name="Binaries\Scarlett\GameXSX.exe" Id="Gen9" TargetDeviceFamily="XboxGen9"/>
  </ExecutableList>
  <PersistentLocalStorage>
    <SizeMB>2048</SizeMB>
  </PersistentLocalStorage>
  <XboxAnacondaTitleMemory>Advanced</XboxAnacondaTitleMemory>
</Game>"#;

    #[test]
    fn parses_xbox_series_x_manifest() {
        let cfg = GameConfig::parse_xml(SAMPLE_MANIFEST).unwrap();
        assert_eq!(cfg.config_version, "1");
        assert_eq!(cfg.identity_name, "SampleScarlettTitle");
        assert_eq!(cfg.title_id, "7A5F1234");
        assert_eq!(cfg.title_id_u32, Some(0x7A5F_1234));
        assert_eq!(cfg.store_id, "9NBLGGH4TEST");
        assert_eq!(cfg.display_name, "Sample & Test Game");
        assert_eq!(cfg.persistent_local_storage_mb, Some(2048));
        assert_eq!(cfg.anaconda_memory_mode, TitleMemoryMode::Advanced);
        let primary = cfg.primary_executable().unwrap();
        assert_eq!(primary.name, r"Binaries\Scarlett\GameXSX.exe");
        assert_eq!(primary.target_device_family, "XboxGen9");
    }

    #[test]
    fn decodes_utf16le_bom_manifest() {
        let mut bytes = vec![0xFFu8, 0xFEu8];
        for cu in SAMPLE_MANIFEST.encode_utf16() {
            bytes.extend_from_slice(&cu.to_le_bytes());
        }
        let text = decode_xml_bytes(&bytes).unwrap();
        let cfg = GameConfig::parse_xml(&text).unwrap();
        assert_eq!(cfg.title_id_u32, Some(0x7A5F_1234));
    }
}
