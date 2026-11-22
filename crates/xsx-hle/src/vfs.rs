//! Virtual Xbox filesystem mapping `GAME:\` / `G:\` / `D:\` (game package), `T:\` (scratch),
//! and `S:\` (local saves) onto sandboxed host directories with a non-destructive
//! **Copy-on-Write Overlay** (`overlay_root`) and delete tombstones (`.xsxemu-deleted`).
//!
//! Real Xbox Series X retail packages are never modified on disk:
//! - Reads prefer the overlay copy, hide tombstoned paths, and fall back to `game_root`.
//! - Writes and deletes on `GAME:\` / `G:\` / `D:\` go strictly to `overlay_root` (copying up
//!   an existing package file before write-open so `OPEN_EXISTING` preserves its bytes).
//! - Any path climbing above the drive root with `..` is clamped or refused.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

const DELETED_DIR: &str = ".xsxemu-deleted";
const REMOVED_DIR_MARKER: &str = ".xsxemu-dir";

/// Which virtual Xbox drive a guest path targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XboxDrive {
    Game,    // GAME:\, G:\, D:\
    Scratch, // T:\
    Save,    // S:\
}

#[derive(Debug, Clone)]
pub struct ParsedGuestPath {
    pub drive: XboxDrive,
    pub components: Vec<String>,
    pub escaped_root: bool,
}

/// Entry returned by merged overlay + package directory enumeration.
#[derive(Debug, Clone)]
pub struct VfsDirEntry {
    pub name: String,
    pub host_path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// Maps Xbox Series X drive paths (`GAME:\`, `G:\`, `D:\`, `T:\`, `S:\`) onto sandboxed
/// host directories with a Copy-on-Write write overlay for the game package.
#[derive(Debug, Clone)]
pub struct VirtualFs {
    pub game_root: PathBuf,
    pub overlay_root: PathBuf,
    pub scratch_root: PathBuf,
    pub save_root: PathBuf,
}

impl VirtualFs {
    pub fn new(game_root: PathBuf, scratch_root: PathBuf) -> Self {
        let overlay_root = scratch_root.join("overlay");
        let save_root = scratch_root.join("saves");
        Self {
            game_root,
            overlay_root,
            scratch_root,
            save_root,
        }
    }

    pub fn with_overlay(
        game_root: PathBuf,
        overlay_root: PathBuf,
        scratch_root: PathBuf,
    ) -> Self {
        let save_root = scratch_root.join("saves");
        Self {
            game_root,
            overlay_root,
            scratch_root,
            save_root,
        }
    }

    pub fn default_temp() -> Self {
        let base = std::env::temp_dir().join("xsxemu_vfs");
        Self {
            game_root: base.join("G"),
            overlay_root: base.join("overlay"),
            scratch_root: base.join("T"),
            save_root: base.join("S"),
        }
    }

    /// Ensure root directories exist on disk before file creation.
    pub fn ensure_roots(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.game_root)?;
        fs::create_dir_all(&self.overlay_root)?;
        fs::create_dir_all(&self.scratch_root)?;
        fs::create_dir_all(&self.save_root)?;
        Ok(())
    }

    /// Parse an Xbox guest path (`GAME:\...`, `G:\...`, `D:\...`, `T:\...`, `S:\...`, `\\?\...`).
    pub fn parse_guest_path(&self, raw: &str) -> Option<ParsedGuestPath> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
        let normalized = stripped.replace('\\', "/");

        let (drive, rel_str) = if normalized.len() >= 5
            && normalized[..5].eq_ignore_ascii_case("game:")
        {
            (XboxDrive::Game, normalized[5..].trim_start_matches('/'))
        } else if normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
            let d = (normalized.as_bytes()[0] as char).to_ascii_uppercase();
            let rest = normalized[2..].trim_start_matches('/');
            let drive = match d {
                'G' | 'D' => XboxDrive::Game,
                'T' => XboxDrive::Scratch,
                'S' => XboxDrive::Save,
                _ => XboxDrive::Scratch,
            };
            (drive, rest)
        } else {
            (XboxDrive::Game, normalized.trim_start_matches('/'))
        };

        let mut components: Vec<String> = Vec::new();
        let mut escaped_root = false;
        for comp in Path::new(rel_str).components() {
            match comp {
                Component::Normal(part) => {
                    let s = part.to_string_lossy();
                    if s.contains(':') {
                        return None;
                    }
                    components.push(s.into_owned());
                }
                Component::ParentDir => {
                    if components.pop().is_none() {
                        escaped_root = true;
                    }
                }
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }

        Some(ParsedGuestPath {
            drive,
            components,
            escaped_root,
        })
    }

    fn join_parts(root: &Path, parts: &[String]) -> PathBuf {
        let mut out = root.to_path_buf();
        for p in parts {
            out.push(p);
        }
        out
    }

    fn tombstone_path(&self, parts: &[String]) -> PathBuf {
        let mut p = self.overlay_root.join(DELETED_DIR);
        for c in parts {
            p.push(c);
        }
        p
    }

    fn is_tombstoned(&self, parts: &[String]) -> bool {
        if parts.is_empty() {
            return false;
        }
        // Check exact path and any ancestor directory marker
        for depth in 1..=parts.len() {
            let m = self.tombstone_path(&parts[..depth]);
            if m.is_file() || m.join(REMOVED_DIR_MARKER).is_file() {
                return true;
            }
        }
        false
    }

    /// Direct host path mapping (backward-compatible with Phase 1–5 callers).
    /// For `GAME:\` / `G:\` / `D:\`, returns the overlay copy if present, else `game_root`.
    pub fn resolve_guest_path(&self, raw: &str) -> Option<PathBuf> {
        let parsed = self.parse_guest_path(raw)?;
        match parsed.drive {
            XboxDrive::Game => {
                let ov = Self::join_parts(&self.overlay_root, &parsed.components);
                if ov.exists() {
                    Some(ov)
                } else {
                    Some(Self::join_parts(&self.game_root, &parsed.components))
                }
            }
            XboxDrive::Scratch => Some(Self::join_parts(&self.scratch_root, &parsed.components)),
            XboxDrive::Save => Some(Self::join_parts(&self.save_root, &parsed.components)),
        }
    }

    /// Resolve a guest path for read-only access (respects CoW overlay precedence & delete tombstones).
    pub fn resolve_for_read(&self, raw: &str) -> Option<PathBuf> {
        let parsed = self.parse_guest_path(raw)?;
        match parsed.drive {
            XboxDrive::Game => {
                let ov = Self::join_parts(&self.overlay_root, &parsed.components);
                if ov.exists() {
                    return Some(ov);
                }
                if self.is_tombstoned(&parsed.components) {
                    return None;
                }
                let pkg = Self::join_parts(&self.game_root, &parsed.components);
                if pkg.exists() {
                    Some(pkg)
                } else {
                    None
                }
            }
            XboxDrive::Scratch => {
                let p = Self::join_parts(&self.scratch_root, &parsed.components);
                p.exists().then_some(p)
            }
            XboxDrive::Save => {
                let p = Self::join_parts(&self.save_root, &parsed.components);
                p.exists().then_some(p)
            }
        }
    }

    /// Resolve a guest path for write access. On `GAME:\` / `G:\` / `D:\`, always redirects to
    /// `overlay_root` (copying up from `game_root` if `copy_up` is true and the file exists in
    /// the package without a tombstone) so the original package is never mutated.
    pub fn resolve_for_write(&self, raw: &str, copy_up: bool) -> Option<PathBuf> {
        let parsed = self.parse_guest_path(raw)?;
        match parsed.drive {
            XboxDrive::Game => {
                let ov = Self::join_parts(&self.overlay_root, &parsed.components);
                if let Some(parent) = ov.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if copy_up && !ov.exists() && !self.is_tombstoned(&parsed.components) {
                    let pkg = Self::join_parts(&self.game_root, &parsed.components);
                    if pkg.is_file() {
                        let _ = fs::copy(&pkg, &ov);
                    }
                }
                Some(ov)
            }
            XboxDrive::Scratch => {
                let p = Self::join_parts(&self.scratch_root, &parsed.components);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                Some(p)
            }
            XboxDrive::Save => {
                let p = Self::join_parts(&self.save_root, &parsed.components);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                Some(p)
            }
        }
    }

    /// Delete a guest file. Package files under `GAME:\` are hidden via a tombstone in
    /// `overlay_root/.xsxemu-deleted` without ever touching `game_root`.
    pub fn delete_guest_file(&self, raw: &str) -> bool {
        let Some(parsed) = self.parse_guest_path(raw) else {
            return false;
        };
        if parsed.components.is_empty() {
            return false;
        }
        match parsed.drive {
            XboxDrive::Game => {
                let ov = Self::join_parts(&self.overlay_root, &parsed.components);
                let pkg = Self::join_parts(&self.game_root, &parsed.components);
                let mut existed = false;
                if ov.is_file() {
                    let _ = fs::remove_file(&ov);
                    existed = true;
                }
                if pkg.is_file() && !self.is_tombstoned(&parsed.components) {
                    let mark = self.tombstone_path(&parsed.components);
                    if let Some(parent) = mark.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(&mark, b"");
                    existed = true;
                }
                existed
            }
            XboxDrive::Scratch => {
                let p = Self::join_parts(&self.scratch_root, &parsed.components);
                fs::remove_file(p).is_ok()
            }
            XboxDrive::Save => {
                let p = Self::join_parts(&self.save_root, &parsed.components);
                fs::remove_file(p).is_ok()
            }
        }
    }

    /// Create a directory in the overlay / scratch / save root.
    pub fn create_guest_dir(&self, raw: &str) -> bool {
        let Some(p) = self.resolve_for_write(raw, false) else {
            return false;
        };
        fs::create_dir_all(p).is_ok()
    }

    /// Remove an empty directory (for `GAME:\`, writes a `.xsxemu-dir` tombstone if present in package).
    pub fn remove_guest_dir(&self, raw: &str) -> bool {
        let Some(parsed) = self.parse_guest_path(raw) else {
            return false;
        };
        if parsed.components.is_empty() {
            return false;
        }
        match parsed.drive {
            XboxDrive::Game => {
                let ov = Self::join_parts(&self.overlay_root, &parsed.components);
                let pkg = Self::join_parts(&self.game_root, &parsed.components);
                let mut existed = false;
                if ov.is_dir() {
                    if fs::remove_dir(&ov).is_err() {
                        return false;
                    }
                    existed = true;
                }
                if pkg.is_dir() && !self.is_tombstoned(&parsed.components) {
                    let mark_dir = self.tombstone_path(&parsed.components);
                    let _ = fs::create_dir_all(&mark_dir);
                    let _ = fs::write(mark_dir.join(REMOVED_DIR_MARKER), b"");
                    existed = true;
                }
                existed
            }
            XboxDrive::Scratch => {
                fs::remove_dir(Self::join_parts(&self.scratch_root, &parsed.components)).is_ok()
            }
            XboxDrive::Save => {
                fs::remove_dir(Self::join_parts(&self.save_root, &parsed.components)).is_ok()
            }
        }
    }

    /// Enumerate directory entries matching a simple wildcard pattern (`*`, `?`) merging
    /// overlay + package (overlay wins on collision, tombstoned entries & `.xsxemu-deleted` hidden).
    pub fn find_guest_files(&self, raw_pattern: &str) -> Vec<VfsDirEntry> {
        let Some(parsed) = self.parse_guest_path(raw_pattern) else {
            return Vec::new();
        };
        let (dir_parts, pat) = match parsed.components.split_last() {
            Some((last, parent)) => (parent, last.as_str()),
            None => (&[][..], "*"),
        };

        let mut merged: BTreeMap<String, VfsDirEntry> = BTreeMap::new();
        let mut scan_dir = |host_dir: &Path, is_package: bool| {
            let Ok(rd) = fs::read_dir(host_dir) else {
                return;
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if dir_parts.is_empty() && name.eq_ignore_ascii_case(DELETED_DIR) {
                    continue;
                }
                if !wildcard_match(pat, &name) {
                    continue;
                }
                let key = name.to_ascii_lowercase();
                if merged.contains_key(&key) {
                    continue; // overlay already won
                }
                if is_package {
                    let mut child_parts = dir_parts.to_vec();
                    child_parts.push(name.clone());
                    if self.is_tombstoned(&child_parts) {
                        continue;
                    }
                }
                let meta = entry.metadata().ok();
                merged.insert(
                    key,
                    VfsDirEntry {
                        name,
                        host_path: entry.path(),
                        is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    },
                );
            }
        };

        match parsed.drive {
            XboxDrive::Game => {
                scan_dir(&Self::join_parts(&self.overlay_root, dir_parts), false);
                scan_dir(&Self::join_parts(&self.game_root, dir_parts), true);
            }
            XboxDrive::Scratch => {
                scan_dir(&Self::join_parts(&self.scratch_root, dir_parts), false);
            }
            XboxDrive::Save => {
                scan_dir(&Self::join_parts(&self.save_root, dir_parts), false);
            }
        }

        merged.into_values().collect()
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let t: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_xbox_drives_and_blocks_traversal() {
        let vfs = VirtualFs::new(PathBuf::from("/host/game"), PathBuf::from("/host/scratch"));
        assert_eq!(
            vfs.resolve_guest_path(r"G:\content\level0.pak"),
            Some(PathBuf::from("/host/game/content/level0.pak"))
        );
        assert_eq!(
            vfs.resolve_guest_path(r"GAME:\data\x.sps"),
            Some(PathBuf::from("/host/game/data/x.sps"))
        );
        assert_eq!(
            vfs.resolve_guest_path(r"T:\temp\save.bin"),
            Some(PathBuf::from("/host/scratch/temp/save.bin"))
        );
        assert_eq!(
            vfs.resolve_guest_path(r"\\?\D:\shaders\main.dxil"),
            Some(PathBuf::from("/host/game/shaders/main.dxil"))
        );
        assert_eq!(
            vfs.resolve_guest_path(r"T:\..\..\..\etc\passwd"),
            Some(PathBuf::from("/host/scratch/etc/passwd"))
        );
    }

    #[test]
    fn copy_on_write_overlay_never_modifies_package_and_handles_tombstones() {
        let base = std::env::temp_dir().join(format!("xsxemu_vfs_cow_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let vfs = VirtualFs::with_overlay(
            base.join("pkg"),
            base.join("overlay"),
            base.join("scratch"),
        );
        vfs.ensure_roots().unwrap();

        // Put a read-only package file in pkg/data/cfg.ini
        fs::create_dir_all(vfs.game_root.join("data")).unwrap();
        fs::write(vfs.game_root.join("data/cfg.ini"), b"orig=1").unwrap();

        // Read sees package file
        let r = vfs.resolve_for_read(r"GAME:\data\cfg.ini").unwrap();
        assert_eq!(fs::read(&r).unwrap(), b"orig=1");

        // Write copies up to overlay and leaves package untouched
        let w = vfs.resolve_for_write(r"GAME:\data\cfg.ini", true).unwrap();
        assert_eq!(fs::read(&w).unwrap(), b"orig=1");
        fs::write(&w, b"patched=2").unwrap();
        assert_eq!(fs::read(vfs.game_root.join("data/cfg.ini")).unwrap(), b"orig=1");
        assert_eq!(
            fs::read(vfs.resolve_for_read(r"GAME:\data\cfg.ini").unwrap()).unwrap(),
            b"patched=2"
        );

        // Delete hides file via tombstone while package remains intact
        assert!(vfs.delete_guest_file(r"GAME:\data\cfg.ini"));
        assert!(vfs.resolve_for_read(r"GAME:\data\cfg.ini").is_none());
        assert!(vfs.game_root.join("data/cfg.ini").is_file());
        assert!(vfs.find_guest_files(r"GAME:\data\*").is_empty());

        let _ = fs::remove_dir_all(&base);
    }
}
