//! The retirement fence: a permanent refusal to re-register handles a backend
//! has withdrawn.
//!
//! ## Why dropping the state is not enough
//!
//! [`NfsServerControl::retire_handles`] drops every mapping and every piece of
//! client-visible state naming a set of objects. On its own that is a
//! *snapshot*, and a snapshot cannot express "and never again":
//!
//! ```text
//! LOOKUP resolves a handle at the backend   (backend still authorized)
//! backend authority is withdrawn
//! retire_handles snapshots the mappings     (the handle is not in them yet)
//! the LOOKUP registers its handle           (a fresh, valid mapping)
//! ```
//!
//! The retired object is reachable again through a mapping created *after* the
//! withdrawal, and nothing is left to notice. Widening the snapshot does not
//! help; there is always a later registration.
//!
//! ## The fence
//!
//! A fence is the retirement predicate, kept. Registration consults it, so a
//! handle that matches a retirement can never be given an object id again — the
//! only possible answer is `NFS4ERR_STALE`, which is exactly what a withdrawn
//! export should say.
//!
//! Installing the fence and taking the snapshot happen under the *same* write
//! lock that registration uses. That is what makes the pair atomic: a
//! registration either completes before both, and is therefore in the snapshot,
//! or takes the lock afterwards and is refused. There is no ordering in which
//! it lands in between.
//!
//! Fences are never removed. That is the point — a retired handle must not
//! regain validity — and it is why [`RetirementFences::len`] exists: an
//! embedder that retires unboundedly should scope its predicate to a generation
//! so the list grows with generations, not with objects.

use std::sync::Arc;

/// A backend's membership test for one retirement, kept so it can be applied to
/// future registrations rather than only to present ones.
type Fence<H> = Arc<dyn Fn(&H) -> bool + Send + Sync>;

/// Every retirement this server has performed, in installation order.
pub(crate) struct RetirementFences<H> {
    fences: Vec<Fence<H>>,
}

impl<H> RetirementFences<H> {
    pub(crate) fn new() -> Self {
        Self { fences: Vec::new() }
    }

    /// Record a retirement. Callers must hold the registration lock so this is
    /// atomic with the snapshot the retirement takes.
    pub(crate) fn install(&mut self, fence: Fence<H>) {
        self.fences.push(fence);
    }

    /// Whether `handle` names something retired.
    pub(crate) fn is_retired(&self, handle: &H) -> bool {
        self.fences.iter().any(|f| f(handle))
    }

    /// How many retirements are in force. Diagnostics only — see the module
    /// docs on scoping.
    pub(crate) fn len(&self) -> usize {
        self.fences.len()
    }
}

impl<H> Default for RetirementFences<H> {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn a_handle_matching_any_fence_is_retired() {
        let mut fences = RetirementFences::<u32>::new();
        assert!(!fences.is_retired(&1));

        fences.install(Arc::new(|h: &u32| *h == 1));
        assert!(fences.is_retired(&1));
        assert!(!fences.is_retired(&2));

        // A second retirement does not undo the first.
        fences.install(Arc::new(|h: &u32| *h == 2));
        assert!(fences.is_retired(&1));
        assert!(fences.is_retired(&2));
        assert!(!fences.is_retired(&3));
        assert_eq!(fences.len(), 2);
    }
}
