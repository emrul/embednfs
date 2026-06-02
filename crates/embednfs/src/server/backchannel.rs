use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};

use embednfs_proto::xdr::*;
use embednfs_proto::*;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CallbackError {
    #[error("no callback-capable connection")]
    NoConnection,
    #[error("callback send failed")]
    SendFailed,
    #[error("callback timed out")]
    Timeout,
    #[error("callback RPC was not accepted: {0:?}")]
    RpcRejected(AcceptStat),
    #[error("callback reply was malformed: {0}")]
    BadReply(XdrError),
}

pub(crate) struct CallbackRequest {
    pub(crate) connection_id: u64,
    pub(crate) cb_program: u32,
    pub(crate) auth: OpaqueAuth,
    pub(crate) args: CbCompound4Args,
    pub(crate) timeout: Duration,
}

#[derive(Default)]
pub(crate) struct BackchannelManager {
    connections: DashMap<u64, mpsc::UnboundedSender<Bytes>>,
    waiters: DashMap<(u64, u32), oneshot::Sender<Bytes>>,
    next_xid: AtomicU32,
}

impl BackchannelManager {
    pub(crate) fn register_connection(
        &self,
        connection_id: u64,
        sender: mpsc::UnboundedSender<Bytes>,
    ) {
        let _ = self.connections.insert(connection_id, sender);
    }

    pub(crate) fn unregister_connection(&self, connection_id: u64) {
        let _ = self.connections.remove(&connection_id);
    }

    pub(crate) fn has_connection(&self, connection_id: u64) -> bool {
        self.connections.contains_key(&connection_id)
    }

    pub(crate) fn handle_reply(&self, connection_id: u64, record: Bytes) -> bool {
        let mut src = record.clone();
        let Ok(xid) = u32::decode(&mut src) else {
            return false;
        };
        if let Some((_, waiter)) = self.waiters.remove(&(connection_id, xid)) {
            waiter.send(record).is_ok()
        } else {
            false
        }
    }

    pub(crate) async fn send_callback(
        &self,
        request: CallbackRequest,
    ) -> Result<CbCompound4Res, CallbackError> {
        let sender = self
            .connections
            .get(&request.connection_id)
            .ok_or(CallbackError::NoConnection)?
            .clone();
        let mut xid = self
            .next_xid
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if xid == 0 {
            xid = self
                .next_xid
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
        }

        let mut payload = BytesMut::with_capacity(1024);
        encode_rpc_call(
            &mut payload,
            xid,
            request.cb_program,
            NFS_V4,
            1,
            &request.auth,
            &OpaqueAuth::null(),
        );
        request.args.encode(&mut payload);

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.waiters.insert((request.connection_id, xid), reply_tx);

        if sender.send(payload.freeze()).is_err() {
            let _ = self.waiters.remove(&(request.connection_id, xid));
            return Err(CallbackError::SendFailed);
        }

        let reply = match tokio::time::timeout(request.timeout, reply_rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(CallbackError::SendFailed),
            Err(_) => {
                let _ = self.waiters.remove(&(request.connection_id, xid));
                return Err(CallbackError::Timeout);
            }
        };

        decode_callback_reply(reply)
    }
}

fn decode_callback_reply(reply: Bytes) -> Result<CbCompound4Res, CallbackError> {
    let mut src = reply;
    let header = RpcAcceptedReply::decode(&mut src).map_err(CallbackError::BadReply)?;
    if header.accept_stat != AcceptStat::Success {
        return Err(CallbackError::RpcRejected(header.accept_stat));
    }
    CbCompound4Res::decode(&mut src).map_err(CallbackError::BadReply)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn test_send_callback_routes_reply_by_xid() {
        let manager = Arc::new(BackchannelManager::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.register_connection(7, tx);

        let sessionid = [0x44; 16];
        let stateid = Stateid4 {
            seqid: 1,
            other: [0x55; 12],
        };
        let request = CallbackRequest {
            connection_id: 7,
            cb_program: 0x4000_1000,
            auth: OpaqueAuth::null(),
            timeout: Duration::from_secs(1),
            args: CbCompound4Args {
                tag: "recall".into(),
                minorversion: 1,
                callback_ident: 0,
                argarray: vec![
                    NfsCbArgop4::Sequence(CbSequenceArgs4 {
                        sessionid,
                        sequenceid: 1,
                        slotid: 0,
                        highest_slotid: 0,
                        cachethis: false,
                    }),
                    NfsCbArgop4::Recall(CbRecallArgs4 {
                        stateid,
                        truncate: false,
                        fh: NfsFh4(Bytes::from_static(b"fh")),
                    }),
                ],
            },
        };

        let reply_manager = manager.clone();
        let peer = tokio::spawn(async move {
            let mut call = rx.recv().await.expect("callback request");
            let xid = u32::decode(&mut call).unwrap();
            assert_eq!(MsgType::decode(&mut call).unwrap(), MsgType::Call);
            assert_eq!(u32::decode(&mut call).unwrap(), RPC_VERSION);
            assert_eq!(u32::decode(&mut call).unwrap(), 0x4000_1000);
            assert_eq!(u32::decode(&mut call).unwrap(), NFS_V4);
            assert_eq!(u32::decode(&mut call).unwrap(), 1);
            let _cred = OpaqueAuth::decode(&mut call).unwrap();
            let _verf = OpaqueAuth::decode(&mut call).unwrap();
            assert_eq!(String::decode(&mut call).unwrap(), "recall");
            assert_eq!(u32::decode(&mut call).unwrap(), 1);
            assert_eq!(u32::decode(&mut call).unwrap(), 0);
            assert_eq!(u32::decode(&mut call).unwrap(), 2);
            assert_eq!(u32::decode(&mut call).unwrap(), OP_CB_SEQUENCE);
            let _ = decode_fixed_opaque(&mut call, 16).unwrap();
            let _ = u32::decode(&mut call).unwrap();
            let _ = u32::decode(&mut call).unwrap();
            let _ = u32::decode(&mut call).unwrap();
            let _ = bool::decode(&mut call).unwrap();
            assert_eq!(u32::decode(&mut call).unwrap(), 0);
            assert_eq!(u32::decode(&mut call).unwrap(), OP_CB_RECALL);
            let _ = Stateid4::decode(&mut call).unwrap();
            let _ = bool::decode(&mut call).unwrap();
            let _ = NfsFh4::decode(&mut call).unwrap();

            let mut reply = BytesMut::new();
            encode_rpc_reply_accepted(&mut reply, xid);
            NfsStat4::Ok.encode(&mut reply);
            "recall".to_string().encode(&mut reply);
            2u32.encode(&mut reply);
            OP_CB_SEQUENCE.encode(&mut reply);
            NfsStat4::Ok.encode(&mut reply);
            reply.extend_from_slice(&sessionid);
            1u32.encode(&mut reply);
            0u32.encode(&mut reply);
            0u32.encode(&mut reply);
            0u32.encode(&mut reply);
            OP_CB_RECALL.encode(&mut reply);
            NfsStat4::Ok.encode(&mut reply);
            assert!(reply_manager.handle_reply(7, reply.freeze()));
        });

        let res = manager.send_callback(request).await.unwrap();
        peer.await.unwrap();

        assert_eq!(res.status, NfsStat4::Ok);
        assert_eq!(res.resarray.len(), 2);
        assert!(matches!(
            &res.resarray[0],
            NfsCbResop4::Sequence(NfsStat4::Ok, Some(_))
        ));
        assert_eq!(res.resarray[1], NfsCbResop4::Recall(NfsStat4::Ok));
    }
}
