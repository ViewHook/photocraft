//! A bounded folder of converted FrameForge fonts that outlives the session: the desktop app's
//! `Services::font_cache_load/store` (the panel decides what goes in and checks what comes back,
//! see `photocraft_ui_egui::frameforge_ui`). One file per entry, named by its key; reading an entry
//! marks it as just used, and storing one removes the least recently used entries until the
//! folder fits its budget again. Every failure is quiet: a missing or damaged cache only costs a
//! download.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The desktop app's budget for converted fonts.
pub const BUDGET: u64 = 64 << 20;

/// See the [module docs](self).
#[derive(Clone, Debug)]
pub struct FontStore {
    dir: PathBuf,
    budget: u64,
}

impl FontStore {
    /// A store in `dir` (created when the first entry is stored) holding at most `budget` bytes.
    pub fn new(dir: PathBuf, budget: u64) -> FontStore {
        FontStore { dir, budget }
    }

    /// The entry stored under `key`, if there is one no larger than the budget; it becomes the
    /// most recently used. `None` for a key that isn't a plain file name (see [`Self::store`]).
    pub fn load(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path(key)?;
        let file = std::fs::File::open(&path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(self.budget.saturating_add(1)).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > self.budget {
            return None;
        }
        touch(&path);
        Some(bytes)
    }

    /// Store `bytes` under `key` (crash-safe: a temporary file renamed over the old entry), then
    /// remove the least recently used other entries while the folder is over budget. Keys are 1
    /// to 128 ASCII letters and digits, so an entry can't name a path outside the folder.
    pub fn store(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(key).ok_or_else(|| format!("not a font cache key: {key:?}"))?;
        if bytes.len() as u64 > self.budget {
            return Err(format!("{} bytes is over the font cache's budget", bytes.len()));
        }
        photocraft_format::atomic_write(&path, bytes).map_err(|e| e.to_string())?;
        self.evict(key);
        Ok(())
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        let plain = (1..=128).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric());
        plain.then(|| self.dir.join(key))
    }

    /// Remove entries other than `keep`, least recently used first, until the folder fits.
    fn evict(&self, keep: &str) {
        let Ok(list) = std::fs::read_dir(&self.dir) else { return };
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for entry in list.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !meta.is_file() || self.path(name).is_none() {
                continue;
            }
            total = total.saturating_add(meta.len());
            if name != keep {
                entries.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len(), entry.path()));
            }
        }
        entries.sort();
        for (_, len, path) in entries {
            if total <= self.budget {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// Mark an entry as just used (its modification time; best effort).
fn touch(path: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("photocraft-font-store-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Pretend `key` was last used `ago` seconds ago.
    fn age(store: &FontStore, key: &str, ago: u64) {
        let file = std::fs::OpenOptions::new().append(true).open(store.dir.join(key)).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(ago)).unwrap();
    }

    fn keys(store: &FontStore) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&store.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        out.sort();
        out
    }

    #[test]
    fn font_store_keeps_entries_by_key_and_refuses_other_names() {
        let dir = temp("keys");
        let store = FontStore::new(dir.clone(), 1000);
        assert_eq!(store.load("abc"), None, "nothing stored, and no folder yet");
        store.store("abc", b"first").unwrap();
        assert_eq!(store.load("abc").as_deref(), Some(b"first".as_slice()));
        store.store("abc", b"second").unwrap();
        assert_eq!(store.load("abc").as_deref(), Some(b"second".as_slice()), "a new entry replaces the old");
        for bad in ["", "../abc", "a/b", ".abc", "abc.tmp", "ab c", &"a".repeat(129)] {
            assert!(store.store(bad, b"x").is_err(), "{bad:?}");
            assert_eq!(store.load(bad), None, "{bad:?}");
        }
        assert_eq!(keys(&store), ["abc"]);
        assert!(!dir.with_file_name("abc").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn font_store_drops_the_least_recently_used_past_its_budget() {
        let dir = temp("budget");
        let store = FontStore::new(dir.clone(), 3000);
        for (key, ago) in [("a", 300), ("b", 200), ("c", 100)] {
            store.store(key, &[0u8; 1000]).unwrap();
            age(&store, key, ago);
        }
        assert_eq!(keys(&store), ["a", "b", "c"], "exactly the budget is fine");
        // Using `a` makes `b` the oldest.
        assert!(store.load("a").is_some());
        store.store("d", &[1u8; 1000]).unwrap();
        assert_eq!(keys(&store), ["a", "c", "d"]);
        // A big entry pushes out as many as it takes, but never itself.
        store.store("e", &[2u8; 2500]).unwrap();
        assert_eq!(keys(&store), ["e"]);
        // Over the whole budget: refused, and the folder is left alone.
        assert!(store.store("f", &[3u8; 3001]).is_err());
        assert_eq!(keys(&store), ["e"]);
        // Files that aren't entries are neither counted nor removed.
        std::fs::write(dir.join("notes.txt"), [0u8; 5000]).unwrap();
        store.store("g", &[4u8; 400]).unwrap();
        assert_eq!(keys(&store), ["e", "g", "notes.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn font_store_reads_no_more_than_its_budget() {
        let dir = temp("bounded");
        let store = FontStore::new(dir.clone(), 100);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big"), [0u8; 101]).unwrap();
        assert_eq!(store.load("big"), None, "larger than the budget: not read");
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        assert_eq!(store.load("folder"), None);
        // A folder that can't be created fails quietly.
        let blocked = FontStore::new(dir.join("big").join("fonts"), 100);
        assert!(blocked.store("abc", b"x").is_err());
        assert_eq!(blocked.load("abc"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
