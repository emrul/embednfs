//! Retiring every piece of server state that belongs to a set of backend
//! objects.
//!
//! A backend can retire an exported subtree while the server is running — for
//! example when the authority that permitted the export is withdrawn. Making
//! the backend refuse operations is not sufficient on its own: the server would
//! still hold filehandle mappings, open and lock state, and delegations naming
//! those objects, so a client could keep presenting a filehandle that resolves,
//! and a delegation could keep a client believing it owns cached state.
//!
//! This drops all of it in one pass, so that after it returns the only possible
//! answer for those objects is a fresh lookup — which the backend is free to
//! refuse.

use std::collections::HashSet;

use crate::internal::{ObjectId, ServerObject};

use super::StateManager;

/// What an invalidation pass removed. Returned so a caller can log or assert on
/// it rather than trusting the operation silently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InvalidationCounts {
    /// Filehandle mappings dropped (both directions).
    pub filehandles: usize,
    /// OPEN states dropped.
    pub opens: usize,
    /// Lock states dropped.
    pub locks: usize,
    /// Delegations dropped, of any kind.
    pub delegations: usize,
    /// Synthetic metadata entries dropped.
    pub metadata: usize,
}

/// Whether `object` belongs to one of `ids`.
///
/// Named-attribute objects hang off a parent object, so retiring a file must
/// also retire its attribute directory and entries — otherwise a client holding
/// a named-attr filehandle would keep resolving after its parent was retired.
fn belongs_to(object: &ServerObject, ids: &HashSet<ObjectId>) -> bool {
    match object {
        ServerObject::Fs(id) | ServerObject::NamedAttrDir(id) => ids.contains(id),
        ServerObject::NamedAttrFile { parent, .. } => ids.contains(parent),
    }
}

impl StateManager {
    /// Drop every mapping and piece of client-visible state for `ids`.
    ///
    /// Idempotent: invalidating an already-invalidated set is a no-op that
    /// reports zero, so a caller retrying after a partial failure is safe.
    pub(crate) async fn invalidate_objects(&self, ids: &HashSet<ObjectId>) -> InvalidationCounts {
        let mut counts = InvalidationCounts::default();
        if ids.is_empty() {
            return counts;
        }

        // Filehandles first: this is the entry point every client operation
        // goes through, so removing it closes the door before the state behind
        // it is dismantled.
        let stale: Vec<(Vec<u8>, ServerObject)> = self
            .fh_to_object
            .iter()
            .filter(|e| belongs_to(e.value(), ids))
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        for (fh, object) in stale {
            let _ = self.fh_to_object.remove(&fh);
            let _ = self.object_to_fh.remove(&object);
            counts.filehandles += 1;
        }

        let mut inner = self.inner.write().await;

        let dropped_delegations: Vec<[u8; 12]> = inner
            .delegations
            .iter()
            .filter(|(_, d)| belongs_to(&d.object, ids))
            .map(|(k, _)| *k)
            .collect();
        for stateid in &dropped_delegations {
            let _ = inner.delegations.remove(stateid);
            counts.delegations += 1;
        }
        // A delegation is referenced from three places; leaving either index
        // populated would resurrect a stateid the client no longer owns.
        for set in inner.client_delegations.values_mut() {
            for stateid in &dropped_delegations {
                let _ = set.remove(stateid);
            }
        }
        inner
            .dir_delegations
            .retain(|object, _| !belongs_to(object, ids));

        let dropped_opens: Vec<[u8; 12]> = inner
            .open_files
            .iter()
            .filter(|(_, o)| belongs_to(&o.object, ids))
            .map(|(k, _)| *k)
            .collect();
        for stateid in dropped_opens {
            let _ = inner.open_files.remove(&stateid);
            counts.opens += 1;
        }

        let dropped_locks: Vec<[u8; 12]> = inner
            .lock_files
            .iter()
            .filter(|(_, l)| belongs_to(&l.object, ids))
            .map(|(k, _)| *k)
            .collect();
        for stateid in dropped_locks {
            let _ = inner.lock_files.remove(&stateid);
            counts.locks += 1;
        }

        let before = inner.metadata.len();
        inner.metadata.retain(|object, _| !belongs_to(object, ids));
        counts.metadata = before - inner.metadata.len();

        counts
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test scaffolding: a failed unwrap/panic is the test failing"
    )]
    use super::*;

    fn ids(v: &[ObjectId]) -> HashSet<ObjectId> {
        v.iter().copied().collect()
    }

    #[test]
    fn membership_covers_named_attributes_of_a_retired_object() {
        let set = ids(&[7]);
        assert!(belongs_to(&ServerObject::Fs(7), &set));
        assert!(belongs_to(&ServerObject::NamedAttrDir(7), &set));
        assert!(belongs_to(
            &ServerObject::NamedAttrFile {
                parent: 7,
                name: "user.x".into(),
            },
            &set,
        ));
        // A named-attr file of a *different* parent must survive.
        assert!(!belongs_to(
            &ServerObject::NamedAttrFile {
                parent: 8,
                name: "user.x".into(),
            },
            &set,
        ));
        assert!(!belongs_to(&ServerObject::Fs(8), &set));
    }

    #[tokio::test]
    async fn retiring_drops_filehandles_and_leaves_others_intact() {
        let state = StateManager::new();
        let retired = state.object_to_fh(&ServerObject::Fs(1));
        let retired_attr = state.object_to_fh(&ServerObject::NamedAttrDir(1));
        let kept = state.object_to_fh(&ServerObject::Fs(2));

        let counts = state.invalidate_objects(&ids(&[1])).await;
        assert_eq!(counts.filehandles, 2, "object and its named-attr dir");

        assert_eq!(state.fh_to_object(&retired), None);
        assert_eq!(state.fh_to_object(&retired_attr), None);
        assert_eq!(
            state.fh_to_object(&kept),
            Some(ServerObject::Fs(2)),
            "an unrelated object must not be disturbed",
        );
    }

    #[tokio::test]
    async fn retiring_is_idempotent_and_empty_is_a_no_op() {
        let state = StateManager::new();
        let _ = state.object_to_fh(&ServerObject::Fs(1));

        assert_eq!(
            state.invalidate_objects(&ids(&[])).await,
            InvalidationCounts::default()
        );
        let first = state.invalidate_objects(&ids(&[1])).await;
        assert_eq!(first.filehandles, 1);
        let second = state.invalidate_objects(&ids(&[1])).await;
        assert_eq!(
            second,
            InvalidationCounts::default(),
            "retrying after a partial failure must be safe",
        );
    }

    /// A retired object must not be able to reappear under its old filehandle
    /// even if the backend hands out the same object again — the new mapping is
    /// a fresh filehandle, so old bytes stay dead.
    #[tokio::test]
    async fn a_retired_object_gets_a_new_filehandle_if_it_returns() {
        let state = StateManager::new();
        let before = state.object_to_fh(&ServerObject::Fs(1));
        let _ = state.invalidate_objects(&ids(&[1])).await;
        let after = state.object_to_fh(&ServerObject::Fs(1));

        assert_ne!(before.0, after.0, "the old filehandle must not be reissued");
        assert_eq!(state.fh_to_object(&before), None);
        assert_eq!(state.fh_to_object(&after), Some(ServerObject::Fs(1)));
    }
}
