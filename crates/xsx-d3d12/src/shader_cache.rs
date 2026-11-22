//! Content-addressed in-memory + optional on-disk Xbox -> PC shader translation cache.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shader::{translate_xbox_shader_container, ShaderError, TranslatedShader};

/// Caches Xbox -> PC `DXBC`/`DXIL` translated shader containers by FNV-1a 64-bit hash.
pub struct ShaderCache {
    disk_dir: Option<PathBuf>,
    mem: HashMap<u64, TranslatedShader>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ShaderCache {
    pub fn new(disk_dir: Option<PathBuf>) -> Self {
        if let Some(dir) = &disk_dir {
            let _ = fs::create_dir_all(dir);
        }
        Self {
            disk_dir,
            mem: HashMap::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Translate an Xbox shader container or return the cached PC `DXBC` bytes immediately.
    pub fn get_or_translate(&mut self, raw_xbox_dxbc: &[u8]) -> Result<TranslatedShader, ShaderError> {
        let key = fnv1a_64(raw_xbox_dxbc);
        if let Some(cached) = self.mem.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(cached.clone());
        }

        // Check on-disk cache if configured
        if let Some(dir) = &self.disk_dir {
            let path = dir.join(format!("{key:016x}.dxbc"));
            if let Ok(pc_bytes) = fs::read(&path) {
                if let Ok(re_tr) = translate_xbox_shader_container(&pc_bytes) {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    self.mem.insert(key, re_tr.clone());
                    return Ok(re_tr);
                }
            }
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        let translated = translate_xbox_shader_container(raw_xbox_dxbc)?;
        if let Some(dir) = &self.disk_dir {
            let path = dir.join(format!("{key:016x}.dxbc"));
            let _ = fs::write(path, &translated.pc_dxbc_bytes);
        }
        self.mem.insert(key, translated.clone());
        Ok(translated)
    }
}

pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader::ShaderContainer;

    #[test]
    fn caches_in_memory_and_on_disk() {
        let dir = std::env::temp_dir().join(format!("xsxemu_scache_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let raw = ShaderContainer::build(&[
            (*b"XBOX", &[1, 2, 3, 4]),
            (*b"DXIL", b"BC\xC0\xDEtest"),
        ]);

        let mut cache = ShaderCache::new(Some(dir.clone()));
        let t1 = cache.get_or_translate(&raw).unwrap();
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        let t2 = cache.get_or_translate(&raw).unwrap();
        assert_eq!(cache.hits(), 1);
        assert_eq!(t1.pc_dxbc_bytes, t2.pc_dxbc_bytes);

        // Fresh cache instance loads from disk
        let mut cache2 = ShaderCache::new(Some(dir.clone()));
        let t3 = cache2.get_or_translate(&raw).unwrap();
        assert_eq!(cache2.hits(), 1);
        assert_eq!(cache2.misses(), 0);
        assert_eq!(t1.pc_dxbc_bytes, t3.pc_dxbc_bytes);

        let _ = fs::remove_dir_all(&dir);
    }
}
