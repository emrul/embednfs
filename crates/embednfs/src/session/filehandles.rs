use crate::internal::ServerObject;

use super::{FH_LEN, FH_NONCE_LEN, StateManager};

impl StateManager {
    pub(crate) fn alloc_connection_id(&self) -> u64 {
        self.next_connectionid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The opaque filehandle for `object`, minting one on first use.
    ///
    /// Layout is `[boot nonce | counter]`, big-endian counter. The nonce makes
    /// the bytes unique to this server boot, so a handle from a previous boot
    /// can never be mistaken for one of ours — see `fh_boot_nonce`.
    /// Mint (or reuse) the opaque filehandle for `object`.
    ///
    /// Retirement blocks **minting**, not naming.
    ///
    /// An existing mapping is still returned for a retired object, because a
    /// delegation recall has to name the very handle the client already holds,
    /// and refusing there would silence the recall instead of protecting
    /// anything. What must not happen is a *new* filehandle appearing after the
    /// invalidation sweep: the sweep runs once, so anything minted afterwards
    /// would outlive it and resolve. Hence `None` only on the minting path.
    pub(crate) fn object_to_fh(&self, object: &ServerObject) -> Option<embednfs_proto::NfsFh4> {
        if let Some(fh) = self.object_to_fh.get(object) {
            return Some(embednfs_proto::NfsFh4(fh.value().clone().into()));
        }
        if self.is_retired(object) {
            return None;
        }
        let fh_num = self
            .next_fh
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut fh = Vec::with_capacity(FH_LEN);
        fh.extend_from_slice(&self.fh_boot_nonce);
        fh.extend_from_slice(&fh_num.to_be_bytes());
        let _ = self.fh_to_object.insert(fh.clone(), object.clone());
        let _ = self.object_to_fh.insert(object.clone(), fh.clone());
        Some(embednfs_proto::NfsFh4(fh.into()))
    }

    /// Resolve an opaque filehandle, rejecting anything not minted by this boot.
    ///
    /// The nonce check runs **before** the map lookup, so a stale handle is
    /// rejected on its own merits rather than by happening to miss the table.
    /// That is what makes the rejection durable: without it, a counter value
    /// from a previous boot resolves as soon as this boot allocates far enough
    /// to reach it.
    pub(crate) fn fh_to_object(&self, fh: &embednfs_proto::NfsFh4) -> Option<ServerObject> {
        let bytes = fh.0.as_ref();
        if bytes.len() != FH_LEN || bytes.get(..FH_NONCE_LEN)? != self.fh_boot_nonce {
            return None;
        }
        self.fh_to_object.get(bytes).map(|r| r.value().clone())
    }

    /// This boot's filehandle nonce, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn fh_boot_nonce(&self) -> [u8; FH_NONCE_LEN] {
        self.fh_boot_nonce
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

    fn object(id: u64) -> ServerObject {
        ServerObject::Fs(id)
    }

    #[test]
    fn a_filehandle_carries_this_boot_nonce_and_round_trips() {
        let state = StateManager::new();
        let fh = state.object_to_fh(&object(7)).unwrap();
        assert_eq!(fh.0.len(), FH_LEN);
        assert_eq!(&fh.0[..FH_NONCE_LEN], &state.fh_boot_nonce());
        assert_eq!(state.fh_to_object(&fh), Some(object(7)));
    }

    #[test]
    fn the_same_object_keeps_one_filehandle() {
        let state = StateManager::new();
        assert_eq!(
            state.object_to_fh(&object(1)).unwrap(),
            state.object_to_fh(&object(1)).unwrap()
        );
        assert_ne!(
            state.object_to_fh(&object(1)).unwrap(),
            state.object_to_fh(&object(2)).unwrap()
        );
    }

    /// The reuse hazard this design exists to close.
    ///
    /// Two servers stand in for a restart: the counter restarts at 1 in both,
    /// so without a per-boot prefix the *same bytes* would name a different
    /// object in the second. The first server's handle must not resolve in the
    /// second, no matter how far its counter has advanced.
    #[test]
    fn a_previous_boots_filehandle_never_resolves_after_restart() {
        let first = StateManager::new();
        let captured = first.object_to_fh(&object(42)).unwrap();

        let second = StateManager::new();
        // Advance well past the captured counter value, which is exactly the
        // situation that made the old bare-counter handle resolve again.
        for id in 0..200u64 {
            let _ = second.object_to_fh(&object(id));
        }

        assert_eq!(
            second.fh_to_object(&captured),
            None,
            "a handle minted by another boot must never resolve",
        );
        // And the collision it would have had: same counter, different nonce.
        let same_counter = second.object_to_fh(&object(42));
        assert_ne!(
            same_counter.unwrap().0,
            captured.0,
            "two boots must not mint identical filehandle bytes",
        );
    }

    #[test]
    fn a_malformed_or_truncated_filehandle_is_rejected() {
        let state = StateManager::new();
        let good = state.object_to_fh(&object(3)).unwrap();
        for bad in [
            embednfs_proto::NfsFh4(bytes::Bytes::from_static(&[])),
            embednfs_proto::NfsFh4(bytes::Bytes::from_static(&[0u8; 8])),
            embednfs_proto::NfsFh4(good.0.slice(..FH_LEN - 1)),
            embednfs_proto::NfsFh4(bytes::Bytes::from(vec![0u8; FH_LEN + 4])),
        ] {
            assert_eq!(state.fh_to_object(&bad), None, "{bad:?} should be rejected");
        }
    }
}
