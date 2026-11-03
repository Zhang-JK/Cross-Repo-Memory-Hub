//! Old id → new id, so references keep working after a memory moves, is
//! ported from 1.0 to a different place, or becomes evidence.
//!
//! ```text
//! <data_root>/redirects.json   { "<old id>": "<new memory or evidence id>", ... }
//! ```

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use crate::feedback::{load_json, save_json};

/// Longest chain of redirects followed.
const MAX_HOPS: usize = 8;

pub struct Redirects {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Redirects {
    pub(crate) fn open(data_root: &Path) -> Self {
        Self {
            path: data_root.join("redirects.json"),
            lock: Mutex::new(()),
        }
    }

    /// Final target of `from`, following chains; `None` if not redirected.
    pub fn resolve(&self, from: &str) -> Option<String> {
        let all = self.all();
        let mut current = all.get(from)?.clone();
        for _ in 0..MAX_HOPS {
            match all.get(&current) {
                Some(next) if next != from => current = next.clone(),
                _ => break,
            }
        }
        Some(current)
    }

    pub fn set(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let _guard = self.lock();
        let mut all = self.all();
        all.remove(to);
        all.insert(from.to_string(), to.to_string());
        if let Err(e) = save_json(&self.path, &all) {
            tracing::warn!("cannot save redirects to {}: {e}", self.path.display());
        }
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        load_json(&self.path)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chains_resolve_and_cycles_stop() {
        let dir = tempfile::tempdir().unwrap();
        let r = Redirects::open(dir.path());
        r.set("a", "b");
        r.set("b", "c");
        assert_eq!(r.resolve("a").as_deref(), Some("c"));
        assert_eq!(r.resolve("x"), None);
        // "a" is a live target again, so its own redirect goes away.
        r.set("c", "a");
        assert_eq!(r.resolve("c").as_deref(), Some("a"));
        assert_eq!(r.resolve("a"), None);
    }
}
