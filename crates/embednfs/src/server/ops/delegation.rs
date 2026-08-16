use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use embednfs_proto::*;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::fs::{FileSystem, FsError, FsResult, RequestContext};
use crate::internal::{ObjectId, ServerFileType, ServerObject};
use crate::session::{
    CallbackTarget, DirectoryDelegationGrant, DirectoryDelegationRecall, InvalidationCounts,
    StateManager,
};

use super::super::backchannel::{BackchannelManager, CallbackError, CallbackRequest};
use super::super::{DelegationConfig, NfsServer, NfsServerControl};

impl<F: FileSystem> NfsServer<F> {
    pub(crate) async fn op_get_dir_delegation(
        &self,
        request_ctx: &RequestContext,
        current_fh: &Option<NfsFh4>,
        minorversion: u32,
        sequence_clientid: Option<Clientid4>,
        sequence_sessionid: Option<Sessionid4>,
    ) -> NfsResop4 {
        if minorversion == 0 || !self.delegation_config.directory_delegations {
            return NfsResop4::GetDirDelegation(NfsStat4::Notsupp, None);
        }
        let Some(clientid) = sequence_clientid else {
            return NfsResop4::GetDirDelegation(NfsStat4::OpNotInSession, None);
        };
        info!("metric=get_dir_delegation_seen clientid={clientid}");

        let (_, object) = match self.resolve_object(current_fh).await {
            Ok(resolved) => resolved,
            Err(status) => return NfsResop4::GetDirDelegation(status, None),
        };
        match &object {
            ServerObject::Fs(_) => {}
            ServerObject::NamedAttrDir(_) => {
                return NfsResop4::GetDirDelegation(NfsStat4::Notsupp, None);
            }
            ServerObject::NamedAttrFile { .. } => {
                return NfsResop4::GetDirDelegation(NfsStat4::Notdir, None);
            }
        }
        let attr = match self.build_attr(request_ctx, &object).await {
            Ok(attr) => attr,
            Err(e) => return NfsResop4::GetDirDelegation(e.to_nfsstat4(), None),
        };
        if !matches!(
            attr.file_type,
            ServerFileType::Directory | ServerFileType::NamedAttrDir
        ) {
            return NfsResop4::GetDirDelegation(NfsStat4::Notdir, None);
        }

        if !self.has_callback_path(clientid).await {
            return NfsResop4::GetDirDelegation(NfsStat4::DirDelegUnavail, None);
        }

        match self
            .state
            .grant_directory_delegation(
                object,
                clientid,
                sequence_sessionid,
                self.delegation_config.max_delegations_per_client,
                self.delegation_config.max_delegations_total,
            )
            .await
        {
            Ok(DirectoryDelegationGrant::Granted(stateid)) => {
                info!(
                    "metric=get_dir_delegation_ok clientid={} stateid_seqid={}",
                    clientid, stateid.seqid
                );
                NfsResop4::GetDirDelegation(
                    NfsStat4::Ok,
                    Some(GetDirDelegationRes4::Ok(GetDirDelegationResOk4 {
                        cookieverf: attr.change_id.to_be_bytes(),
                        stateid,
                        notification: Bitmap4::new(),
                        child_attributes: Bitmap4::new(),
                        dir_attributes: Bitmap4::new(),
                    })),
                )
            }
            Ok(DirectoryDelegationGrant::AlreadyHeld) => NfsResop4::GetDirDelegation(
                NfsStat4::Ok,
                Some(GetDirDelegationRes4::Unavail {
                    will_signal_deleg_avail: false,
                }),
            ),
            Ok(DirectoryDelegationGrant::Unavailable) => {
                NfsResop4::GetDirDelegation(NfsStat4::DirDelegUnavail, None)
            }
            Err(status) => NfsResop4::GetDirDelegation(status, None),
        }
    }

    pub(crate) async fn recall_directory_delegations_excluding(
        &self,
        object: &ServerObject,
        excluded_clientid: Option<Clientid4>,
    ) -> Result<(), NfsStat4> {
        recall_directory_delegations(
            &self.state,
            &self.backchannels,
            &self.delegation_config,
            object,
            excluded_clientid,
        )
        .await
    }

    /// Recall directory delegations for an exported backend directory handle.
    ///
    /// Unknown handles are treated as a no-op because no NFS client can hold
    /// a delegation for an object that has not been exposed by this server.
    pub async fn recall_directory(&self, handle: &F::Handle) -> FsResult<()> {
        let Some(object_id) = self.handle_to_object.read().await.get(handle).copied() else {
            return Ok(());
        };
        recall_directory_delegations(
            &self.state,
            &self.backchannels,
            &self.delegation_config,
            &ServerObject::Fs(object_id),
            None,
        )
        .await
        .map_err(recall_status_to_fs_error)
    }

    async fn has_callback_path(&self, clientid: Clientid4) -> bool {
        has_callback_path(&self.state, &self.backchannels, clientid).await
    }
}

impl<H> NfsServerControl<H>
where
    H: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Recalls every **write** delegation the server has granted for objects
    /// matching `scope`, and reports how many it recalled.
    ///
    /// For a backend narrowing an export from writable to read-only. Such a
    /// backend has to know that no client still believes it may write, and it
    /// must not have to reason about what this server does or does not grant to
    /// find out — that is a server policy, and a backend guessing at it is a
    /// backend that will be wrong the day the policy changes.
    ///
    /// The answer is a **query over live delegation state**, not a statement
    /// about what this server happens to grant. Today it is always zero,
    /// because the only kind this server grants is a directory delegation and
    /// that conveys no authority over file bytes — but nothing here assumes
    /// that. `DelegationKind::conveys_write` decides, and it is an exhaustive
    /// match: a new kind will not compile until someone classifies it and, for
    /// a `true`, adds the recall path to go with it.
    ///
    /// So `Ok(0)` means "no client holds write authority under this scope",
    /// verified — not "this server does not do that", assumed.
    ///
    /// Deadlines are the **caller's**. This operation does not impose one:
    /// narrowing an export is the caller's transition, bounded by the caller's
    /// budget, and a server-side timeout here would silently substitute a
    /// different one.
    pub async fn recall_write_delegations<P>(&self, scope: P) -> FsResult<usize>
    where
        P: Fn(&H) -> bool + Send + Sync + 'static,
    {
        let scoped: HashSet<ServerObject> = {
            let map = self.handle_to_object.read().await;
            map.iter()
                .filter(|(handle, _)| scope(handle))
                .map(|(_, object)| ServerObject::Fs(*object))
                .collect()
        };
        if scoped.is_empty() {
            return Ok(0);
        }
        let outstanding = self.state.write_delegations_over(&scoped).await;
        if outstanding.is_empty() {
            return Ok(0);
        }
        // Unreachable while no kind conveys write authority. Writing the recall
        // for a delegation that cannot exist would be speculative, so the
        // contract is enforced at the type level instead: whoever adds such a
        // kind has to come through `conveys_write`, and finds this waiting.
        tracing::error!(
            outstanding = outstanding.len(),
            "write delegations are outstanding but no recall path exists for them"
        );
        Err(FsError::Unsupported)
    }

    /// Recalls directory delegations for an exported backend directory handle.
    ///
    /// Unknown handles are treated as a no-op because no NFS client can hold
    /// a delegation for an object that has not been exposed by this server.
    pub async fn recall_directory(&self, handle: &H) -> FsResult<()> {
        let Some(object_id) = self.handle_to_object.read().await.get(handle).copied() else {
            return Ok(());
        };
        recall_directory_delegations(
            &self.state,
            &self.backchannels,
            &self.delegation_config,
            &ServerObject::Fs(object_id),
            None,
        )
        .await
        .map_err(recall_status_to_fs_error)
    }
}

async fn has_callback_path(
    state: &StateManager,
    backchannels: &BackchannelManager,
    clientid: Clientid4,
) -> bool {
    state
        .callback_connection_ids(clientid)
        .await
        .into_iter()
        .any(|connection_id| backchannels.has_connection(connection_id))
}

async fn next_callback_target(
    state: &StateManager,
    backchannels: &BackchannelManager,
    clientid: Clientid4,
) -> Option<CallbackTarget> {
    for connection_id in state.callback_connection_ids(clientid).await {
        if backchannels.has_connection(connection_id)
            && let Some(target) = state.next_callback_target_on(clientid, connection_id).await
        {
            return Some(target);
        }
    }
    None
}

async fn recall_directory_delegations(
    state: &StateManager,
    backchannels: &BackchannelManager,
    delegation_config: &DelegationConfig,
    object: &ServerObject,
    excluded_clientid: Option<Clientid4>,
) -> Result<(), NfsStat4> {
    if !delegation_config.directory_delegations {
        return Ok(());
    }

    let recalls = state
        .begin_directory_recall_excluding(object, excluded_clientid)
        .await;
    if recalls.is_empty() {
        return Ok(());
    }

    // No filehandle was ever issued for this object, so no client can hold a
    // delegation naming it.
    let Some(fh) = state.object_to_fh(object) else {
        return Ok(());
    };
    for recall in &recalls {
        if recall.send_callback
            && let Err(status) =
                send_directory_recall(state, backchannels, delegation_config, recall, &fh).await
        {
            debug!(
                "directory delegation recall callback failed for client {}: {status:?}",
                recall.clientid
            );
            if let Err(revoke_status) = state.revoke_recallable_delegation(&recall.stateid).await {
                debug!("delegation revoke after callback failure failed: {revoke_status:?}");
            } else {
                info!(
                    "metric=revocation_count reason=callback_failure clientid={}",
                    recall.clientid
                );
            }
        }
    }

    wait_for_recalled_delegations(state, delegation_config, &recalls).await
}

async fn send_directory_recall(
    state: &StateManager,
    backchannels: &BackchannelManager,
    delegation_config: &DelegationConfig,
    recall: &DirectoryDelegationRecall,
    fh: &NfsFh4,
) -> Result<(), NfsStat4> {
    let target = next_callback_target(state, backchannels, recall.clientid)
        .await
        .ok_or(NfsStat4::CbPathDown)?;
    info!(
        "metric=cb_recall_sent clientid={} connection_id={}",
        recall.clientid, target.connection_id
    );
    let response = backchannels
        .send_callback(CallbackRequest {
            connection_id: target.connection_id,
            cb_program: target.cb_program,
            auth: target.auth,
            timeout: delegation_config.recall_timeout,
            args: CbCompound4Args {
                tag: "recall".into(),
                minorversion: 1,
                callback_ident: 0,
                argarray: vec![
                    NfsCbArgop4::Sequence(CbSequenceArgs4 {
                        sessionid: target.sessionid,
                        sequenceid: target.sequenceid,
                        slotid: 0,
                        highest_slotid: target.highest_slotid,
                        cachethis: false,
                    }),
                    NfsCbArgop4::Recall(CbRecallArgs4 {
                        stateid: recall.stateid,
                        truncate: false,
                        fh: fh.clone(),
                    }),
                ],
            },
        })
        .await
        .map_err(callback_error_status)?;

    validate_recall_response(&response)?;
    info!("metric=cb_recall_ok clientid={}", recall.clientid);
    Ok(())
}

async fn wait_for_recalled_delegations(
    state: &StateManager,
    delegation_config: &DelegationConfig,
    recalls: &[DirectoryDelegationRecall],
) -> Result<(), NfsStat4> {
    let deadline = Instant::now() + delegation_config.recall_timeout;
    let started = Instant::now();
    let mut outstanding: Vec<Stateid4> = recalls.iter().map(|recall| recall.stateid).collect();

    loop {
        let mut remaining = Vec::with_capacity(outstanding.len());
        for stateid in outstanding {
            if !state.delegation_recall_complete(&stateid).await {
                remaining.push(stateid);
            }
        }
        if remaining.is_empty() {
            info!(
                "metric=recall_wait_ms value={}",
                started.elapsed().as_millis()
            );
            return Ok(());
        }

        if Instant::now() >= deadline {
            info!("metric=recall_timeout count={}", remaining.len());
            for stateid in &remaining {
                if let Err(status) = state.revoke_recallable_delegation(stateid).await {
                    debug!("delegation revoke after recall timeout failed: {status:?}");
                } else {
                    info!("metric=revocation_count reason=timeout");
                }
            }
            info!(
                "metric=recall_wait_ms value={}",
                started.elapsed().as_millis()
            );
            return Ok(());
        }

        outstanding = remaining;
        sleep(Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

fn callback_error_status(error: CallbackError) -> NfsStat4 {
    match error {
        CallbackError::Timeout => NfsStat4::Delay,
        CallbackError::NoConnection
        | CallbackError::SendFailed
        | CallbackError::RpcRejected(_)
        | CallbackError::BadReply(_) => NfsStat4::CbPathDown,
    }
}

fn validate_recall_response(response: &CbCompound4Res) -> Result<(), NfsStat4> {
    if response.status != NfsStat4::Ok {
        return Err(response.status);
    }

    let mut saw_recall = false;
    for op in &response.resarray {
        match op {
            NfsCbResop4::Sequence(status, _) if *status != NfsStat4::Ok => {
                return Err(*status);
            }
            NfsCbResop4::Sequence(_, _) => {}
            NfsCbResop4::Recall(status) if *status == NfsStat4::Ok => {
                saw_recall = true;
            }
            NfsCbResop4::Recall(status) => return Err(*status),
        }
    }

    if saw_recall {
        Ok(())
    } else {
        Err(NfsStat4::Serverfault)
    }
}

fn recall_status_to_fs_error(status: NfsStat4) -> FsError {
    match status {
        NfsStat4::Access => FsError::AccessDenied,
        NfsStat4::Perm => FsError::PermissionDenied,
        NfsStat4::Badhandle | NfsStat4::Stale => FsError::Stale,
        NfsStat4::Notsupp => FsError::Unsupported,
        NfsStat4::Delay | NfsStat4::CbPathDown => FsError::Io,
        _ => FsError::ServerFault,
    }
}

impl<H> NfsServerControl<H>
where
    H: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Retire every backend object whose handle satisfies `retired`, dropping
    /// all server state that names it.
    ///
    /// For a backend that can withdraw part of its export at runtime. Refusing
    /// operations at the backend is necessary but not sufficient: without this,
    /// the server keeps filehandle mappings that still resolve, OPEN and lock
    /// state, and delegations that leave a client believing it owns cached
    /// state for objects the backend has retired.
    ///
    /// Directory delegations are recalled first, best-effort and bounded by the
    /// server's existing recall policy, so a cooperating client is told before
    /// its state disappears. A recall that cannot be delivered does not block
    /// retirement — the state is dropped regardless, and the client discovers it
    /// on next use. Withdrawal must not depend on a client answering.
    ///
    /// The predicate is the backend's own membership test; the server does not
    /// interpret handles beyond equality, so no backend policy leaks in here.
    ///
    /// Retirement is **permanent**: the predicate is kept as a fence, and any
    /// later attempt to register a handle matching it is refused with
    /// `NFS4ERR_STALE`. Dropping the current state alone would not be enough —
    /// a lookup that resolved a handle before the withdrawal can register it
    /// afterwards, giving the retired object a fresh, valid mapping. The fence
    /// is installed and the snapshot taken under one lock, so a registration
    /// either precedes both or is refused by both.
    ///
    /// Because fences are never removed, scope the predicate to a generation
    /// rather than to individual objects if you retire repeatedly.
    ///
    /// Idempotent: retiring an already-retired set reports zero.
    pub async fn retire_handles<P>(&self, retired: P) -> InvalidationCounts
    where
        P: Fn(&H) -> bool + Send + Sync + 'static,
    {
        let retired = Arc::new(retired);
        let matched: Vec<(H, ObjectId)> = {
            // Fence, snapshot, **and unmap**, all under the one lock that
            // registration takes.
            //
            // Removing the mappings here rather than at the end is what closes
            // the fast path: `register_handle` returns an existing mapping
            // without consulting the fence, so leaving matching mappings in
            // place across the recall — which can take as long as the recall
            // deadline — would let a registration keep succeeding long after
            // the fence was installed.
            let mut handles = self.handle_to_object.write().await;
            self.retired.write().await.install(retired.clone());

            let matched: Vec<(H, ObjectId)> = handles
                .iter()
                .filter(|(handle, _)| retired(handle))
                .map(|(handle, id)| (handle.clone(), *id))
                .collect();

            let mut object_to_handle = self.object_to_handle.write().await;
            for (handle, object_id) in &matched {
                let _ = handles.remove(handle);
                let _ = object_to_handle.remove(object_id);
            }
            matched
        };
        if matched.is_empty() {
            return InvalidationCounts::default();
        }

        // Mark the objects retired before the recall, not just before the
        // sweep: the recall awaits, and an ObjectId resolved earlier must not
        // be able to mint a filehandle or open state while it does.
        let ids: HashSet<ObjectId> = matched.iter().map(|(_, id)| *id).collect();
        self.state.mark_retired(&ids);

        // Tell cooperating clients before the state vanishes.
        for (_, object_id) in &matched {
            if let Err(status) = recall_directory_delegations(
                &self.state,
                &self.backchannels,
                &self.delegation_config,
                &ServerObject::Fs(*object_id),
                None,
            )
            .await
            {
                tracing::debug!(
                    object_id = *object_id,
                    ?status,
                    "retire_handles: delegation recall failed; retiring anyway",
                );
            }
        }

        let counts = self.state.invalidate_objects(&ids).await;

        // Read the count before the macro: awaiting inside the macro arguments
        // holds a non-Send value across the await and makes the whole future
        // non-Send.
        let fences = self.retired.read().await.len();
        tracing::info!(
            objects = matched.len(),
            fences,
            filehandles = counts.filehandles,
            opens = counts.opens,
            locks = counts.locks,
            delegations = counts.delegations,
            "retired backend objects and dropped their server state",
        );
        counts
    }
}
