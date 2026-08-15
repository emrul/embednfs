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
    /// Whether `object` belongs to something the backend has retired.
    ///
    /// Consulted by every path that creates server state, so retirement is a
    /// standing refusal rather than a one-time sweep.
    pub(crate) fn is_retired(&self, object: &ServerObject) -> bool {
        let id = match object {
            ServerObject::Fs(id) | ServerObject::NamedAttrDir(id) => *id,
            ServerObject::NamedAttrFile { parent, .. } => *parent,
        };
        self.retired_objects.contains(&id)
    }

    /// Fire the test probe, if one is installed. Compiled out otherwise.
    ///
    /// Placed at each point where a retirement could interleave with state
    /// creation, so a test can complete the retirement there rather than racing
    /// to hit a window a few instructions wide.
    #[inline]
    pub(crate) fn retire_probe(&self) {
        #[cfg(test)]
        {
            let probe = self.retire_probe.lock().expect("probe mutex poisoned");
            if let Some(f) = probe.as_ref() {
                f();
            }
        }
    }

    /// Install the probe. Tests only.
    #[cfg(test)]
    pub(crate) fn set_retire_probe(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.retire_probe.lock().expect("probe mutex poisoned") = Some(Box::new(f));
    }

    /// A handle on the retired set, so a probe can mark objects retired from
    /// inside the window it is simulating.
    #[cfg(test)]
    pub(crate) fn retired_set(&self) -> std::sync::Arc<dashmap::DashSet<ObjectId>> {
        std::sync::Arc::clone(&self.retired_objects)
    }

    /// Record `ids` as retired, permanently, before anything is dropped.
    ///
    /// Separate from the sweep and ordered before it: the sweep answers for
    /// state that exists *now*, and this answers for state that would be
    /// created afterwards. Doing it second would leave exactly the window it
    /// is meant to close.
    pub(crate) fn mark_retired(&self, ids: &HashSet<ObjectId>) {
        for id in ids {
            let _ = self.retired_objects.insert(*id);
        }
    }

    /// Drop every mapping and piece of client-visible state for `ids`.
    ///
    /// Marks them retired first, so no creation path can replace what this
    /// drops. Idempotent: invalidating an already-invalidated set is a no-op
    /// that reports zero, so a caller retrying after a partial failure is safe.
    pub(crate) async fn invalidate_objects(&self, ids: &HashSet<ObjectId>) -> InvalidationCounts {
        let mut counts = InvalidationCounts::default();
        if ids.is_empty() {
            return counts;
        }
        self.mark_retired(ids);

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
    use embednfs_proto::{NfsStat4, OPEN4_SHARE_ACCESS_READ};

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
        let retired = state.object_to_fh(&ServerObject::Fs(1)).unwrap();
        let retired_attr = state.object_to_fh(&ServerObject::NamedAttrDir(1)).unwrap();
        let kept = state.object_to_fh(&ServerObject::Fs(2)).unwrap();

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
        let _ = state.object_to_fh(&ServerObject::Fs(1)).unwrap();

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

    /// A retired object cannot get a filehandle at all — not its old one, and
    /// not a fresh one.
    ///
    /// The invalidation sweep runs once. If minting stayed open afterwards, an
    /// `ObjectId` resolved before the retirement could mint a new, resolvable
    /// filehandle after it, and the sweep would have no chance to remove it.
    #[tokio::test]
    async fn a_retired_object_can_never_mint_a_filehandle_again() {
        let state = StateManager::new();
        let before = state.object_to_fh(&ServerObject::Fs(1)).unwrap();
        let _ = state.invalidate_objects(&ids(&[1])).await;

        assert_eq!(state.fh_to_object(&before), None, "the old handle is dead");
        assert_eq!(
            state.object_to_fh(&ServerObject::Fs(1)),
            None,
            "and no new handle may be minted for it",
        );
        // Its named attributes are covered too.
        assert_eq!(state.object_to_fh(&ServerObject::NamedAttrDir(1)), None);
        assert_eq!(
            state.object_to_fh(&ServerObject::NamedAttrFile {
                parent: 1,
                name: "user.x".into(),
            }),
            None,
        );
        // An unrelated object is unaffected.
        assert!(state.object_to_fh(&ServerObject::Fs(2)).is_some());
    }

    /// Retirement blocks minting, not naming: an *existing* mapping is still
    /// returned, because a delegation recall has to name the handle the client
    /// already holds. Refusing there would silence the recall rather than
    /// protect anything.
    #[tokio::test]
    async fn a_retired_object_can_still_be_named_while_its_mapping_lives() {
        let state = StateManager::new();
        let fh = state.object_to_fh(&ServerObject::Fs(1)).unwrap();

        state.mark_retired(&ids(&[1]));
        assert_eq!(
            state.object_to_fh(&ServerObject::Fs(1)),
            Some(fh),
            "the recall must still be able to name the object",
        );

        // Once the sweep has run, the mapping is gone and minting is refused.
        let _ = state.invalidate_objects(&ids(&[1])).await;
        assert_eq!(state.object_to_fh(&ServerObject::Fs(1)), None);
    }

    // ── Check-then-act windows, driven deterministically ────────────────────
    //
    // Each test installs a probe that completes a retirement *at* the window,
    // rather than racing to land in one a few instructions wide. A timing test
    // does not reliably catch these — this crate's history is explicit that two
    // earlier lost-wakeup injections survived a 2000-iteration loop.

    /// A retirement landing between the check and the insert must not leave a
    /// filehandle mapping behind.
    ///
    /// The sweep has already run by then, so a mapping published afterwards
    /// would outlive it and keep resolving.
    #[tokio::test]
    async fn a_retirement_inside_the_mint_window_leaves_no_mapping() {
        let state = StateManager::new();
        let retired = state.retired_set();
        state.set_retire_probe(move || {
            // The retirement marks (and, in the real path, then sweeps) while
            // the mint is between its check and its inserts.
            let _ = retired.insert(1);
        });

        let object = ServerObject::Fs(1);
        assert_eq!(
            state.object_to_fh(&object),
            None,
            "a mint racing a retirement must not hand back a filehandle",
        );
        assert!(
            !state.object_to_fh.contains_key(&object),
            "and must not leave a mapping the sweep has already passed",
        );
        assert_eq!(state.fh_to_object.len(), 0, "no reverse mapping either");
    }

    /// The same window for OPEN state: a retirement completing between entry
    /// and the write guard must still refuse the open.
    #[tokio::test]
    async fn a_retirement_inside_the_open_window_refuses_the_open() {
        let state = StateManager::new();
        let retired = state.retired_set();
        state.set_retire_probe(move || {
            let _ = retired.insert(1);
        });

        let result = state
            .create_open_state(ServerObject::Fs(1), 1, OPEN4_SHARE_ACCESS_READ, 0)
            .await;
        assert_eq!(
            result.err(),
            Some(NfsStat4::Stale),
            "an open must not be created for an object retired mid-call",
        );
        assert!(
            state.inner.read().await.open_files.is_empty(),
            "no open state may survive the retirement",
        );
    }

    /// And for a directory delegation.
    #[tokio::test]
    async fn a_retirement_inside_the_delegation_window_refuses_the_grant() {
        let state = StateManager::new();
        let retired = state.retired_set();
        state.set_retire_probe(move || {
            let _ = retired.insert(1);
        });

        let result = state
            .create_directory_delegation(ServerObject::Fs(1), 1, None)
            .await;
        assert_eq!(result.err(), Some(NfsStat4::Stale));
        assert!(
            state.inner.read().await.delegations.is_empty(),
            "no delegation may survive the retirement",
        );
    }

    /// An unretired object is unaffected by the probe being installed at all —
    /// so the tests above are testing the retirement, not the plumbing.
    #[tokio::test]
    async fn the_probe_does_not_disturb_an_unretired_object() {
        let state = StateManager::new();
        let retired = state.retired_set();
        state.set_retire_probe(move || {
            let _ = retired.insert(1);
        });

        // Object 2 is never retired.
        assert!(state.object_to_fh(&ServerObject::Fs(2)).is_some());
        assert!(
            state
                .create_open_state(ServerObject::Fs(2), 1, OPEN4_SHARE_ACCESS_READ, 0)
                .await
                .is_ok(),
        );
    }
}
