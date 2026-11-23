//! An in-memory [`StateStore`], for tests in this and dependent crates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::enforcer::StateStore;

#[derive(Default)]
struct StoreState {
    documents: HashMap<String, String>,
    fail_saves: bool,
}

/// Keeps documents in memory. Clones share the same documents, so a test
/// can hand one clone to an enforcer and "restart" with another.
#[derive(Clone, Default)]
pub struct MemoryStateStore(Arc<Mutex<StoreState>>);

impl MemoryStateStore {
    fn state(&self) -> MutexGuard<'_, StoreState> {
        // A poisoned store means a test already panicked; keep reporting
        // that panic rather than a second one.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Makes every following save fail (or succeed again).
    pub fn fail_saves(&self, fail: bool) {
        self.state().fail_saves = fail;
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self, name: &str) -> Option<String> {
        self.state().documents.get(name).cloned()
    }

    fn save(&self, name: &str, content: &str) -> Result<(), String> {
        let mut state = self.state();
        if state.fail_saves {
            return Err("the fake store refuses to save".into());
        }
        state.documents.insert(name.to_owned(), content.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_documents() {
        let store = MemoryStateStore::default();
        assert_eq!(store.load("a"), None);
        store.clone().save("a", "one").unwrap();
        assert_eq!(store.load("a").as_deref(), Some("one"));
    }

    #[test]
    fn failing_store_keeps_the_previous_document() {
        let store = MemoryStateStore::default();
        store.save("a", "one").unwrap();
        store.fail_saves(true);
        assert_eq!(store.save("a", "two"), Err("the fake store refuses to save".into()));
        assert_eq!(store.load("a").as_deref(), Some("one"));
    }
}
