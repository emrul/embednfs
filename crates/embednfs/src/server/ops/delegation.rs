use std::hash::Hash;
use std::time::{Duration, Instant};

use embednfs_proto::*;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::fs::{FileSystem, FsError, FsResult, RequestContext};
use crate::internal::{ServerFileType, ServerObject};
use crate::session::{
    CallbackTarget, DirectoryDelegationGrant, DirectoryDelegationRecall, StateManager,
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

    pub(crate) async fn recall_directory_delegations(
        &self,
        object: &ServerObject,
    ) -> Result<(), NfsStat4> {
        recall_directory_delegations(
            &self.state,
            &self.backchannels,
            &self.delegation_config,
            object,
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
) -> Result<(), NfsStat4> {
    if !delegation_config.directory_delegations {
        return Ok(());
    }

    let recalls = state.begin_directory_recall(object).await;
    if recalls.is_empty() {
        return Ok(());
    }

    let fh = state.object_to_fh(object);
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
