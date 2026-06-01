use std::sync::atomic::Ordering;

use embednfs_proto::{Clientid4, NfsStat4, Sessionid4, Stateid4};

use crate::internal::ServerObject;

use super::StateManager;
use super::model::{DelegationKind, DelegationState, DelegationStatus, StateInner};

impl StateManager {
    /// Create or reuse a read-only directory delegation stateid.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "directory delegation grants are wired in a later implementation phase"
        )
    )]
    pub(crate) async fn create_directory_delegation(
        &self,
        object: ServerObject,
        clientid: Clientid4,
        sessionid: Option<Sessionid4>,
    ) -> Result<Stateid4, NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;

        if let Some(other) =
            Self::find_live_directory_delegation(&inner, &object, clientid).copied()
        {
            let state = inner.delegations.get(&other).ok_or(NfsStat4::BadStateid)?;
            return Ok(Stateid4 {
                seqid: state.stateid_seq,
                other,
            });
        }

        let seq = self.next_stateid.fetch_add(1, Ordering::Relaxed);
        let mut other = [0u8; 12];
        other[..4].copy_from_slice(&seq.to_be_bytes());
        other[4..12].copy_from_slice(&clientid.to_be_bytes());

        let _ = inner.delegations.insert(
            other,
            DelegationState {
                object: object.clone(),
                clientid,
                sessionid,
                stateid_seq: 1,
                kind: DelegationKind::DirectoryRead,
                status: DelegationStatus::Granted,
                granted_at: self.config.now(),
                last_recall_at: None,
            },
        );
        let _ = inner
            .dir_delegations
            .entry(object)
            .or_default()
            .insert(other);
        let _ = inner
            .client_delegations
            .entry(clientid)
            .or_default()
            .insert(other);

        Ok(Stateid4 { seqid: 1, other })
    }

    /// Return a delegation stateid and remove it from all indexes.
    pub(crate) async fn return_delegation_state(
        &self,
        stateid: &Stateid4,
        clientid: Option<Clientid4>,
    ) -> Result<(), NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        {
            let state = inner
                .delegations
                .get_mut(&stateid.other)
                .ok_or(NfsStat4::BadStateid)?;
            Self::validate_stateid_seq(state.stateid_seq, stateid.seqid)?;
            if let Some(clientid) = clientid
                && state.clientid != clientid
            {
                return Err(NfsStat4::BadStateid);
            }
            if matches!(state.status, DelegationStatus::Revoked) {
                return Err(NfsStat4::DelegRevoked);
            }
            state.status = DelegationStatus::Returned;
        }

        let _ = Self::remove_delegation_locked(&mut inner, stateid.other);
        Ok(())
    }

    /// Purge all delegation stateids owned by a client.
    pub(crate) async fn purge_client_delegations(&self, clientid: Clientid4) {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        Self::remove_client_delegations_locked(&mut inner, clientid);
    }

    /// Mark a delegation revoked so the client can acknowledge it with
    /// `FREE_STATEID`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "recall timeout revocation is wired in a later implementation phase"
        )
    )]
    pub(crate) async fn revoke_delegation_state(&self, stateid: &Stateid4) -> Result<(), NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        let state = inner
            .delegations
            .get_mut(&stateid.other)
            .ok_or(NfsStat4::BadStateid)?;
        Self::validate_stateid_seq(state.stateid_seq, stateid.seqid)?;
        state.status = DelegationStatus::Revoked;
        Ok(())
    }

    pub(super) fn remove_client_delegations_locked(inner: &mut StateInner, clientid: Clientid4) {
        let others = inner
            .client_delegations
            .get(&clientid)
            .cloned()
            .unwrap_or_default();
        for other in others {
            let _ = Self::remove_delegation_locked(inner, other);
        }
    }

    pub(super) fn remove_delegation_locked(
        inner: &mut StateInner,
        other: [u8; 12],
    ) -> Option<DelegationState> {
        let state = inner.delegations.remove(&other)?;

        let remove_dir_index = if let Some(entries) = inner.dir_delegations.get_mut(&state.object) {
            let _ = entries.remove(&other);
            entries.is_empty()
        } else {
            false
        };
        if remove_dir_index {
            let _ = inner.dir_delegations.remove(&state.object);
        }

        let remove_client_index =
            if let Some(entries) = inner.client_delegations.get_mut(&state.clientid) {
                let _ = entries.remove(&other);
                entries.is_empty()
            } else {
                false
            };
        if remove_client_index {
            let _ = inner.client_delegations.remove(&state.clientid);
        }

        Some(state)
    }

    fn find_live_directory_delegation<'a>(
        inner: &'a StateInner,
        object: &ServerObject,
        clientid: Clientid4,
    ) -> Option<&'a [u8; 12]> {
        inner
            .client_delegations
            .get(&clientid)?
            .iter()
            .find(|other| {
                inner.delegations.get(*other).is_some_and(|state| {
                    state.object == *object
                        && state.kind == DelegationKind::DirectoryRead
                        && matches!(
                            state.status,
                            DelegationStatus::Granted | DelegationStatus::RecallInProgress
                        )
                })
            })
    }

    pub(super) fn delegation_test_status(state: &DelegationState, stateid: &Stateid4) -> NfsStat4 {
        match Self::validate_stateid_seq(state.stateid_seq, stateid.seqid) {
            Ok(()) if matches!(state.status, DelegationStatus::Revoked) => NfsStat4::DelegRevoked,
            Ok(()) => NfsStat4::Ok,
            Err(status) => status,
        }
    }

    pub(super) fn has_live_client_delegations(inner: &StateInner, clientid: Clientid4) -> bool {
        inner
            .client_delegations
            .get(&clientid)
            .is_some_and(|others| {
                others.iter().any(|other| {
                    inner.delegations.get(other).is_some_and(|state| {
                        !matches!(
                            state.status,
                            DelegationStatus::Returned | DelegationStatus::Revoked
                        )
                    })
                })
            })
    }
}
