use std::sync::atomic::Ordering;

use embednfs_proto::{
    Clientid4, NfsStat4, SEQ4_STATUS_RECALLABLE_STATE_REVOKED, Sessionid4, Stateid4,
};

use crate::internal::ServerObject;

use super::StateManager;
use super::model::{
    DelegationKind, DelegationState, DelegationStatus, DirectoryDelegationGrant,
    DirectoryDelegationRecall, StateInner,
};

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

    /// Grant a read-only directory delegation when the client and server
    /// limits allow it.
    pub(crate) async fn grant_directory_delegation(
        &self,
        object: ServerObject,
        clientid: Clientid4,
        sessionid: Option<Sessionid4>,
        max_per_client: usize,
        max_total: usize,
    ) -> Result<DirectoryDelegationGrant, NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;

        if Self::find_live_directory_delegation(&inner, &object, clientid).is_some() {
            return Ok(DirectoryDelegationGrant::AlreadyHeld);
        }
        if Self::has_recall_in_progress(&inner, &object) {
            return Ok(DirectoryDelegationGrant::Unavailable);
        }

        let client_count = inner
            .client_delegations
            .get(&clientid)
            .map(|others| {
                others
                    .iter()
                    .filter(|other| Self::is_live_delegation(&inner, other))
                    .count()
            })
            .unwrap_or_default();
        if client_count >= max_per_client {
            return Ok(DirectoryDelegationGrant::Unavailable);
        }

        let total = inner
            .delegations
            .values()
            .filter(|state| Self::is_live_delegation_status(state.status))
            .count();
        if total >= max_total {
            return Ok(DirectoryDelegationGrant::Unavailable);
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

        Ok(DirectoryDelegationGrant::Granted(Stateid4 {
            seqid: 1,
            other,
        }))
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

    /// Mark granted delegations for a directory as being recalled.
    pub(crate) async fn begin_directory_recall(
        &self,
        object: &ServerObject,
    ) -> Vec<DirectoryDelegationRecall> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        let now = self.config.now();
        let others = inner
            .dir_delegations
            .get(object)
            .cloned()
            .unwrap_or_default();
        let mut recalls = Vec::with_capacity(others.len());

        for other in others {
            let Some(state) = inner.delegations.get_mut(&other) else {
                continue;
            };
            if state.kind != DelegationKind::DirectoryRead {
                continue;
            }
            let send_callback = match state.status {
                DelegationStatus::Granted => {
                    state.status = DelegationStatus::RecallInProgress;
                    state.last_recall_at = Some(now);
                    true
                }
                DelegationStatus::RecallInProgress => false,
                DelegationStatus::Returned | DelegationStatus::Revoked => continue,
            };
            recalls.push(DirectoryDelegationRecall {
                stateid: Stateid4 {
                    seqid: state.stateid_seq,
                    other,
                },
                clientid: state.clientid,
                send_callback,
            });
        }

        recalls
    }

    /// Return whether a recalled delegation is no longer outstanding.
    pub(crate) async fn delegation_recall_complete(&self, stateid: &Stateid4) -> bool {
        let inner = self.inner.read().await;
        !inner.delegations.get(&stateid.other).is_some_and(|state| {
            matches!(
                state.status,
                DelegationStatus::Granted | DelegationStatus::RecallInProgress
            )
        })
    }

    /// Revoke a recallable delegation and set the client's SEQUENCE status bit.
    pub(crate) async fn revoke_recallable_delegation(
        &self,
        stateid: &Stateid4,
    ) -> Result<(), NfsStat4> {
        self.reap_expired_clients().await;
        let mut inner = self.inner.write().await;
        let clientid = {
            let state = inner
                .delegations
                .get_mut(&stateid.other)
                .ok_or(NfsStat4::BadStateid)?;
            Self::validate_stateid_seq(state.stateid_seq, stateid.seqid)?;
            state.status = DelegationStatus::Revoked;
            state.clientid
        };
        if let Some(client) = inner.clients.get_mut(&clientid) {
            client.status_flags |= SEQ4_STATUS_RECALLABLE_STATE_REVOKED;
        }
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

    fn has_recall_in_progress(inner: &StateInner, object: &ServerObject) -> bool {
        inner.dir_delegations.get(object).is_some_and(|others| {
            others.iter().any(|other| {
                inner
                    .delegations
                    .get(other)
                    .is_some_and(|state| state.status == DelegationStatus::RecallInProgress)
            })
        })
    }

    fn is_live_delegation(inner: &StateInner, other: &[u8; 12]) -> bool {
        inner
            .delegations
            .get(other)
            .is_some_and(|state| Self::is_live_delegation_status(state.status))
    }

    fn is_live_delegation_status(status: DelegationStatus) -> bool {
        matches!(
            status,
            DelegationStatus::Granted | DelegationStatus::RecallInProgress
        )
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
