use embednfs_proto::{
    Clientid4, NfsStat4, OPEN4_SHARE_ACCESS_BOTH, OPEN4_SHARE_ACCESS_READ,
    OPEN4_SHARE_ACCESS_WANT_DELEG_MASK, OPEN4_SHARE_ACCESS_WRITE, OPEN4_SHARE_DENY_BOTH,
    OPEN4_SHARE_DENY_NONE, OPEN4_SHARE_DENY_READ, OPEN4_SHARE_DENY_WRITE, Stateid4,
};

use crate::internal::ServerObject;

use super::StateManager;
use super::model::OpenFileState;

/// Outcome of [`StateManager::close_state`].
#[derive(Debug)]
pub(crate) struct CloseOutcome {
    /// The bumped open stateid to return to the client.
    pub(crate) stateid: Stateid4,
    /// True if this close removed the object's final write-open.
    pub(crate) last_writer: bool,
}

impl StateManager {
    pub(crate) async fn has_conflicting_share_deny(
        &self,
        object: &ServerObject,
        access: u32,
        ignore_open_other: Option<[u8; 12]>,
    ) -> bool {
        self.reap_expired_clients().await;
        let inner = self.inner.read().await;
        inner.open_files.iter().any(|(other, state)| {
            state.active
                && state.object == *object
                && Some(*other) != ignore_open_other
                && (state.share_deny & access) != 0
        })
    }

    /// Create an open state for an object.
    pub(crate) async fn create_open_state(
        &self,
        object: ServerObject,
        clientid: Clientid4,
        share_access: u32,
        share_deny: u32,
    ) -> Result<Stateid4, NfsStat4> {
        self.reap_expired_clients().await;
        // Store only the access mode. Want/signal hints (the 0xFF00 selector and
        // the signal flags above it) are a protocol-layer concern that `op_open`
        // has already validated; keeping them out of `OpenFileState` ensures the
        // stored share can never carry non-mode bits that would skew the
        // share/lock conflict checks below.
        let share_access = share_access & OPEN4_SHARE_ACCESS_BOTH;
        // Defense in depth: `op_open` fully validates the share before reaching
        // here, but this is the only place an open is registered. Reject a
        // malformed mode so a future caller cannot install an open that no
        // READ/WRITE could be authorized against (zero access mode) or that
        // would corrupt share-conflict checks (out-of-range deny).
        if share_access == 0 || share_deny & !OPEN4_SHARE_DENY_BOTH != 0 {
            return Err(NfsStat4::Inval);
        }
        let mut inner = self.inner.write().await;
        for state in inner.open_files.values() {
            if !state.active {
                continue;
            }
            if state.object == object
                && ((state.share_deny & share_access) != 0
                    || (share_deny & state.share_access) != 0)
            {
                return Err(NfsStat4::ShareDenied);
            }
        }

        let seq = self
            .next_stateid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut other = [0u8; 12];
        other[..4].copy_from_slice(&seq.to_be_bytes());
        other[4..12].copy_from_slice(&clientid.to_be_bytes());

        let _ = inner.open_files.insert(
            other,
            OpenFileState {
                object,
                clientid,
                stateid_seq: 1,
                active: true,
                share_access,
                share_deny,
            },
        );

        Ok(Stateid4 { seqid: 1, other })
    }

    /// NFSv4.0 OPEN_CONFIRM (RFC 7530 §16.18): validate the stateid the
    /// client received from OPEN, bump the stored `stateid_seq`, and return
    /// the bumped stateid. v4.1 clients never call this — sessions subsume
    /// the confirm step.
    pub(crate) async fn confirm_open_state(
        &self,
        stateid: &Stateid4,
    ) -> Result<Stateid4, NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        let state = inner
            .open_files
            .get_mut(&stateid.other)
            .ok_or(NfsStat4::BadStateid)?;
        if !state.active {
            return Err(NfsStat4::BadStateid);
        }
        Self::validate_stateid_seq(state.stateid_seq, stateid.seqid)?;
        state.stateid_seq = state.stateid_seq.wrapping_add(1);
        Ok(Stateid4 {
            seqid: state.stateid_seq,
            other: stateid.other,
        })
    }

    pub(super) fn validate_stateid_seq(stored_seq: u32, provided_seq: u32) -> Result<(), NfsStat4> {
        if provided_seq == 0 || provided_seq == stored_seq {
            Ok(())
        } else if provided_seq < stored_seq {
            Err(NfsStat4::OldStateid)
        } else {
            Err(NfsStat4::BadStateid)
        }
    }

    /// Close an open state.
    ///
    /// Reports whether this close removed the object's last write-open
    /// (`last_writer`): the closed open held write access and no other
    /// active open for the same object still does. The server uses that
    /// to drive an [`crate::fs::OpenLifecycle::on_close`] notification.
    pub(crate) async fn close_state(&self, stateid: &Stateid4) -> Result<CloseOutcome, NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        let (stored_seq, active, object, was_writer) = {
            let state = inner
                .open_files
                .get(&stateid.other)
                .ok_or(NfsStat4::BadStateid)?;
            (
                state.stateid_seq,
                state.active,
                state.object.clone(),
                (state.share_access & OPEN4_SHARE_ACCESS_WRITE) != 0,
            )
        };
        Self::validate_stateid_seq(stored_seq, stateid.seqid)?;
        if !active {
            return Err(NfsStat4::BadStateid);
        }
        if inner
            .lock_files
            .values()
            .any(|lock| lock.active && lock.open_state_other == stateid.other)
        {
            return Err(NfsStat4::LocksHeld);
        }
        let new_seqid = {
            let state = inner
                .open_files
                .get_mut(&stateid.other)
                .ok_or(NfsStat4::BadStateid)?;
            state.active = false;
            state.stateid_seq = stored_seq.wrapping_add(1);
            state.stateid_seq
        };
        // After deactivating this open, is any active write-open left for
        // the same object? If not, and this close was itself a writer,
        // this was the last writer.
        let writers_remain = inner.open_files.values().any(|s| {
            s.active && s.object == object && (s.share_access & OPEN4_SHARE_ACCESS_WRITE) != 0
        });
        Ok(CloseOutcome {
            stateid: Stateid4 {
                seqid: new_seqid,
                other: stateid.other,
            },
            last_writer: was_writer && !writers_remain,
        })
    }

    /// Free a stateid.
    pub(crate) async fn free_stateid(&self, stateid: &Stateid4) -> Result<(), NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        if let Some((open_seq, open_active)) = inner
            .open_files
            .get(&stateid.other)
            .map(|open| (open.stateid_seq, open.active))
        {
            Self::validate_stateid_seq(open_seq, stateid.seqid)?;
            let locks_held = inner
                .lock_files
                .values()
                .any(|lock| lock.active && lock.open_state_other == stateid.other);
            if open_active || locks_held {
                return Err(NfsStat4::LocksHeld);
            }
            let _ = inner.open_files.remove(&stateid.other);
            return Ok(());
        }
        if let Some(lock) = inner.lock_files.get(&stateid.other) {
            Self::validate_stateid_seq(lock.stateid_seq, stateid.seqid)?;
            if lock.active {
                return Err(NfsStat4::LocksHeld);
            }
            let _ = inner.lock_files.remove(&stateid.other);
            return Ok(());
        }
        if let Some(delegation) = inner.delegations.get(&stateid.other) {
            Self::validate_stateid_seq(delegation.stateid_seq, stateid.seqid)?;
            if !matches!(
                delegation.status,
                super::model::DelegationStatus::Revoked | super::model::DelegationStatus::Returned
            ) {
                return Err(NfsStat4::LocksHeld);
            }
            let _ = Self::remove_delegation_locked(&mut inner, stateid.other);
            return Ok(());
        }
        Err(NfsStat4::BadStateid)
    }

    pub(crate) async fn test_stateids(
        &self,
        stateids: &[Stateid4],
        _current_stateid: Option<Stateid4>,
    ) -> Vec<NfsStat4> {
        self.reap_expired_clients().await;
        let inner = self.inner.read().await;
        stateids
            .iter()
            .map(|stateid| {
                if StateManager::is_special_stateid(stateid)
                    || self
                        .normalize_stateid(stateid, None, super::CurrentStateidMode::ZeroSeqid)
                        .is_err()
                {
                    return NfsStat4::BadStateid;
                }
                if let Some(state) = inner.open_files.get(&stateid.other) {
                    if !state.active {
                        return NfsStat4::BadStateid;
                    }
                    match Self::validate_stateid_seq(state.stateid_seq, stateid.seqid) {
                        Ok(()) => NfsStat4::Ok,
                        Err(status) => status,
                    }
                } else if let Some(state) = inner.lock_files.get(&stateid.other) {
                    match Self::validate_stateid_seq(state.stateid_seq, stateid.seqid) {
                        Ok(()) => NfsStat4::Ok,
                        Err(status) => status,
                    }
                } else if let Some(state) = inner.delegations.get(&stateid.other) {
                    Self::delegation_test_status(state, stateid)
                } else {
                    NfsStat4::BadStateid
                }
            })
            .collect()
    }

    pub(crate) async fn open_downgrade(
        &self,
        open_stateid: &Stateid4,
        share_access: u32,
        share_deny: u32,
    ) -> Result<Stateid4, NfsStat4> {
        self.reap_expired_clients().await;
        let access_mode = share_access & !OPEN4_SHARE_ACCESS_WANT_DELEG_MASK;
        if !matches!(
            access_mode,
            OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WRITE | OPEN4_SHARE_ACCESS_BOTH
        ) {
            return Err(NfsStat4::Inval);
        }
        if !matches!(
            share_deny,
            OPEN4_SHARE_DENY_NONE
                | OPEN4_SHARE_DENY_READ
                | OPEN4_SHARE_DENY_WRITE
                | OPEN4_SHARE_DENY_BOTH
        ) {
            return Err(NfsStat4::Inval);
        }

        let mut inner = self.inner.write().await;
        let state = inner
            .open_files
            .get_mut(&open_stateid.other)
            .ok_or(NfsStat4::BadStateid)?;
        Self::validate_stateid_seq(state.stateid_seq, open_stateid.seqid)?;
        if !state.active {
            return Err(NfsStat4::BadStateid);
        }

        let current_access = state.share_access;
        if (access_mode & !current_access) != 0 || (share_deny & !state.share_deny) != 0 {
            return Err(NfsStat4::Inval);
        }

        state.share_access = access_mode;
        state.share_deny = share_deny;
        state.stateid_seq = state.stateid_seq.wrapping_add(1);
        Ok(Stateid4 {
            seqid: state.stateid_seq,
            other: open_stateid.other,
        })
    }
}
