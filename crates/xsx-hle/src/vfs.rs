//! Virtual Xbox filesystem mapping `G:\` (game root), `T:\` (title scratch),
//! `D:\` (game alias), and `S:\` (local saves) onto sandboxed host directories.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Maps Xbox Series X drive paths (`G:\`, `T:\`, `D:\`, `S:\`) onto host directories.
#[derive(Debug, Clone)]
pub struct VirtualFs {
    pub game_root: PathBuf,
    pub scratch_root: PathBuf,
    pub save_root: PathBuf,
}

impl VirtualFs {
    pub fn new(game_root: PathBuf, scratch_root: PathBuf) -> Self {
        let save_root = scratch_root.join("saves");
        Self {
            game_root,
            scratch_root,
            save_root,
        }
    }

    pub fn default_temp() -> Self {
        let base = std::env::temp_dir().join("xsxemu_vfs");
        Self {
            game_root: base.join("G"),
            scratch_root: base.join("T"),
            save_root: base.join("S"),
        }
    }

    /// Ensure root directories exist on disk before file creation.
    pub fn ensure_roots(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.game_root)?;
        fs::create_dir_all(&self.scratch_root)?;
        fs::create_dir_all(&self.save_root)?;
        Ok(())
    }

    /// Resolve a Windows/Xbox path (e.g. `G:\assets\mesh.bin`, `T:\cache.dat`, `\\?\T:\a`)
    /// into a sandboxed host `PathBuf` without allowing `..` traversal outside the drive root.
    pub fn resolve_guest_path(&self, raw: &str) -> Option<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Strip extended-length prefix `\\?\`
        let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
        let normalized = stripped.replace('\\', "/");

        let (root, rel_str) = if normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
            let drive = (normalized.as_bytes()[0] as char).to_ascii_uppercase();
            let rest = normalized[2..].trim_start_matches('/');
            let root = match drive {
                'G' | 'D' => &self.game_root,
                'T' => &self.scratch_root,
                'S' => &self.save_root,
                _ => &self.scratch_root,
            };
            (root, rest)
        } else {
            (&self.game_root, normalized.trim_start_matches('/'))
        };

        let mut resolved = root.clone();
        let mut depth = 0usize;
        for comp in Path::new(rel_str).components() {
            match comp {
                Component::Normal(part) => {
                    resolved.push(part);
                    depth += 1;
                }
                Component::ParentDir => {
                    if depth > 0 {
                        resolved.pop();
                        depth -= 1;
                    }
                }
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }
        Some(resolved)
    }
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
            vfs.resolve_guest_path(r"T:\temp\save.bin"),
            Some(PathBuf::from("/host/scratch/temp/save.bin"))
        );
        assert_eq!(
            vfs.resolve_guest_path(r"\\?\D:\shaders\main.dxil"),
            Some(PathBuf::from("/host/game/shaders/main.dxil"))
        );
        // Traversal above drive root must clamp at root
        assert_eq!(
            vfs.resolve_guest_path(r"T:\..\..\..\etc\passwd"),
            Some(PathBuf::from("/host/scratch/etc/passwd"))
        );
    }
}
