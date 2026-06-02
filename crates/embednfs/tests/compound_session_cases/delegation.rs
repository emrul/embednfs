use super::*;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use embednfs::{DelegationConfig, FileSystem, MemFs, NfsServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn read_record(stream: &mut TcpStream) -> Bytes {
    let mut resp = BytesMut::new();
    loop {
        let mut header = [0u8; 4];
        let _ = stream.read_exact(&mut header).await.unwrap();
        let header_val = u32::from_be_bytes(header);
        let last_fragment = (header_val & 0x8000_0000) != 0;
        let resp_len = (header_val & 0x7fff_ffff) as usize;
        let offset = resp.len();
        resp.resize(offset + resp_len, 0);
        let _ = stream.read_exact(&mut resp[offset..]).await.unwrap();
        if last_fragment {
            break;
        }
    }
    resp.freeze()
}

async fn write_record(stream: &mut TcpStream, msg: Bytes) {
    let len = msg.len() as u32 | 0x8000_0000;
    stream.write_all(&len.to_be_bytes()).await.unwrap();
    stream.write_all(&msg).await.unwrap();
    stream.flush().await.unwrap();
}

async fn write_rpc_call(stream: &mut TcpStream, xid: u32, proc_num: u32, payload: &[u8]) {
    let mut msg = BytesMut::with_capacity(256);
    encode_rpc_call(
        &mut msg,
        xid,
        NFS_PROGRAM,
        NFS_V4,
        proc_num,
        &OpaqueAuth::null(),
        &OpaqueAuth::null(),
    );
    msg.put_slice(payload);
    write_record(stream, msg.freeze()).await;
}

async fn send_rpc_handling_callbacks(
    stream: &mut TcpStream,
    xid: u32,
    proc_num: u32,
    payload: &[u8],
) -> (Bytes, usize) {
    write_rpc_call(stream, xid, proc_num, payload).await;

    let mut callback_count = 0usize;
    loop {
        let record = read_record(stream).await;
        let mut peek = record.clone();
        let record_xid = u32::decode(&mut peek).unwrap();
        let msg_type = MsgType::decode(&mut peek).unwrap();
        match msg_type {
            MsgType::Reply if record_xid == xid => return (record, callback_count),
            MsgType::Reply => panic!("unexpected RPC reply xid {record_xid}"),
            MsgType::Call => {
                callback_count += 1;
                reply_to_callback(stream, record).await;
            }
        }
    }
}

async fn send_rpc_handling_callbacks_and_delegreturn(
    stream: &mut TcpStream,
    xid: u32,
    proc_num: u32,
    payload: &[u8],
    sessionid: &[u8; 16],
    stateid: &Stateid4,
) -> (Bytes, Bytes, usize) {
    const DELEGRETURN_XID: u32 = 0xD1E6_E000;

    write_rpc_call(stream, xid, proc_num, payload).await;

    let mut callback_count = 0usize;
    let mut sent_delegreturn = false;
    let mut original_reply = None;
    let mut delegreturn_reply = None;

    loop {
        let record = read_record(stream).await;
        let mut peek = record.clone();
        let record_xid = u32::decode(&mut peek).unwrap();
        let msg_type = MsgType::decode(&mut peek).unwrap();
        match msg_type {
            MsgType::Reply if record_xid == xid => original_reply = Some(record),
            MsgType::Reply if record_xid == DELEGRETURN_XID => delegreturn_reply = Some(record),
            MsgType::Reply => panic!("unexpected RPC reply xid {record_xid}"),
            MsgType::Call => {
                callback_count += 1;
                reply_to_callback(stream, record).await;
                if !sent_delegreturn {
                    match tokio::time::timeout(Duration::from_millis(50), read_record(stream)).await
                    {
                        Ok(record) => {
                            let mut peek = record.clone();
                            let early_xid = u32::decode(&mut peek).unwrap();
                            let early_msg_type = MsgType::decode(&mut peek).unwrap();
                            panic!(
                                "received {early_msg_type:?} xid {early_xid} before DELEGRETURN"
                            );
                        }
                        Err(_) => {}
                    }
                    write_delegreturn_call(stream, DELEGRETURN_XID, sessionid, stateid).await;
                    sent_delegreturn = true;
                }
            }
        }

        if original_reply.is_some() && delegreturn_reply.is_some() {
            return (
                original_reply.take().unwrap(),
                delegreturn_reply.take().unwrap(),
                callback_count,
            );
        }
    }
}

async fn write_delegreturn_call(
    stream: &mut TcpStream,
    xid: u32,
    sessionid: &[u8; 16],
    stateid: &Stateid4,
) {
    let seq_op = encode_sequence(sessionid, 1, 1);
    let delegreturn_op = encode_delegreturn(stateid);
    let compound = encode_compound("delegreturn-recall", &[&seq_op, &delegreturn_op]);
    write_rpc_call(stream, xid, 1, &compound).await;
}

async fn reply_to_callback(stream: &mut TcpStream, record: Bytes) {
    let mut src = record;
    let call = RpcCallHeader::decode(&mut src).unwrap();
    assert_eq!(call.vers, NFS_V4);
    assert_eq!(call.proc_num, 1);

    let tag = String::decode(&mut src).unwrap();
    let minorversion = u32::decode(&mut src).unwrap();
    assert_eq!(minorversion, 1);
    let _callback_ident = u32::decode(&mut src).unwrap();
    let op_count = u32::decode(&mut src).unwrap();
    let mut sequence = None;
    let mut saw_recall = false;

    for _ in 0..op_count {
        match u32::decode(&mut src).unwrap() {
            OP_CB_SEQUENCE => {
                let sessionid = decode_fixed_opaque(&mut src, 16).unwrap();
                let mut session = [0u8; 16];
                session.copy_from_slice(&sessionid);
                let sequenceid = u32::decode(&mut src).unwrap();
                let slotid = u32::decode(&mut src).unwrap();
                let highest_slotid = u32::decode(&mut src).unwrap();
                let _cachethis = bool::decode(&mut src).unwrap();
                let referring_call_count = u32::decode(&mut src).unwrap();
                assert_eq!(referring_call_count, 0);
                sequence = Some((session, sequenceid, slotid, highest_slotid));
            }
            OP_CB_RECALL => {
                let _stateid = Stateid4::decode(&mut src).unwrap();
                let _truncate = bool::decode(&mut src).unwrap();
                let _fh = NfsFh4::decode(&mut src).unwrap();
                saw_recall = true;
            }
            op => panic!("unexpected callback op {op}"),
        }
    }
    assert!(saw_recall);
    let (sessionid, sequenceid, slotid, highest_slotid) = sequence.unwrap();

    let mut reply = BytesMut::new();
    encode_rpc_reply_accepted(&mut reply, call.xid);
    NfsStat4::Ok.encode(&mut reply);
    tag.encode(&mut reply);
    2u32.encode(&mut reply);
    OP_CB_SEQUENCE.encode(&mut reply);
    NfsStat4::Ok.encode(&mut reply);
    reply.extend_from_slice(&sessionid);
    sequenceid.encode(&mut reply);
    slotid.encode(&mut reply);
    highest_slotid.encode(&mut reply);
    highest_slotid.encode(&mut reply);
    OP_CB_RECALL.encode(&mut reply);
    NfsStat4::Ok.encode(&mut reply);
    write_record(stream, reply.freeze()).await;
}

fn parse_get_dir_delegation_ok(resp: &mut bytes::Bytes) -> Stateid4 {
    let gdd_status = u32::decode(resp).unwrap();
    assert_eq!(gdd_status, 0);
    let _cookieverf = decode_fixed_opaque(resp, 8).unwrap();
    let stateid = Stateid4::decode(resp).unwrap();
    let _notification = Bitmap4::decode(resp).unwrap();
    let _child_attrs = Bitmap4::decode(resp).unwrap();
    let _dir_attrs = Bitmap4::decode(resp).unwrap();
    stateid
}

fn parse_get_dir_delegation_unavail(resp: &mut bytes::Bytes) -> bool {
    let gdd_status = u32::decode(resp).unwrap();
    assert_eq!(gdd_status, 1);
    bool::decode(resp).unwrap()
}

fn parse_sequence_status_flags(resp: &mut Bytes) -> u32 {
    let _sessionid = decode_fixed_opaque(resp, 16).unwrap();
    let _sequenceid = u32::decode(resp).unwrap();
    let _slotid = u32::decode(resp).unwrap();
    let _highest_slotid = u32::decode(resp).unwrap();
    let _target_highest_slotid = u32::decode(resp).unwrap();
    u32::decode(resp).unwrap()
}

/// GET_DIR_DELEGATION remains unsupported when delegation support is disabled.
/// Origin: design/delegations.md compatibility contract; RFC 8881 §18.39.3.
/// RFC: RFC 8881 §18.39.3.
#[tokio::test]
async fn test_get_dir_delegation_disabled_returns_notsupp() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-disabled", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Notsupp as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GET_DIR_DELEGATION);
    assert_eq!(op_status, NfsStat4::Notsupp as u32);
}

/// GET_DIR_DELEGATION refuses grants when the session has no usable backchannel.
/// Origin: design/delegations.md grant policy; RFC 8881 §§2.10.6, 18.39.3.
/// RFC: RFC 8881 §§2.10.6, 18.39.3.
#[tokio::test]
async fn test_get_dir_delegation_requires_backchannel() {
    let port = start_server_with_directory_delegations().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-no-bc", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::DirDelegUnavail as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GET_DIR_DELEGATION);
    assert_eq!(op_status, NfsStat4::DirDelegUnavail as u32);
}

/// GET_DIR_DELEGATION grants a directory delegation on a callback-capable session.
/// Origin: design/delegations.md phase 3; RFC 8881 §§2.10.6, 18.39.3.
/// RFC: RFC 8881 §§2.10.6, 18.39.3.
#[tokio::test]
async fn test_get_dir_delegation_grants_stateid() {
    let port = start_server_with_directory_delegations().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1000).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-grant", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GET_DIR_DELEGATION);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let stateid = parse_get_dir_delegation_ok(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("gdd-test-stateid", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_TEST_STATEID);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(
        parse_test_stateid_results(&mut resp),
        vec![NfsStat4::Ok as u32]
    );
}

/// Repeated GET_DIR_DELEGATION for an already-held directory returns GDD4_UNAVAIL.
/// Origin: RFC 8881 §18.39.3 duplicate request rule.
/// RFC: RFC 8881 §18.39.3.
#[tokio::test]
async fn test_get_dir_delegation_duplicate_returns_unavail() {
    let port = start_server_with_directory_delegations().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1001).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-first", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-dupe", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GET_DIR_DELEGATION);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert!(!parse_get_dir_delegation_unavail(&mut resp));
}

/// Namespace mutation sends CB_RECALL before completing with an outstanding delegation.
/// Origin: design/delegations.md phase 4 recall policy; RFC 8881 §§10.2, 20.2.
/// RFC: RFC 8881 §§10.2, 20.2.
#[tokio::test]
async fn test_create_recalls_directory_delegation_before_mutation() {
    let config = DelegationConfig {
        directory_delegations: true,
        recall_timeout: Duration::from_millis(25),
        max_delegations_per_client: 1024,
        max_delegations_total: 16_384,
    };
    let port = start_server_with_delegation_config(config).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1002).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-recall", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = parse_get_dir_delegation_ok(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("after-recall");
    let compound = encode_compound("create-recall", &[&seq_op, &rootfh_op, &create_op]);
    let (mut resp, callbacks) = send_rpc_handling_callbacks(&mut stream, 4, 1, &compound).await;
    assert_eq!(callbacks, 1);
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CREATE);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("revoked-stateid", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let status_flags = parse_sequence_status_flags(&mut resp);
    assert_ne!(status_flags & SEQ4_STATUS_RECALLABLE_STATE_REVOKED, 0);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_TEST_STATEID);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(
        parse_test_stateid_results(&mut resp),
        vec![NfsStat4::DelegRevoked as u32]
    );
}

/// DELEGRETURN during recall lets a namespace mutation complete without timeout revocation.
/// Origin: design/delegations.md recall policy positive DELEGRETURN path.
/// RFC: RFC 8881 §§10.2, 18.37, 20.2.
#[tokio::test]
async fn test_create_waits_for_timely_delegreturn_before_mutation() {
    let config = DelegationConfig {
        directory_delegations: true,
        recall_timeout: Duration::from_secs(5),
        max_delegations_per_client: 1024,
        max_delegations_total: 16_384,
    };
    let port = start_server_with_delegation_config(config).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1003).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-recall-return", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = parse_get_dir_delegation_ok(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("after-delegreturn");
    let compound = encode_compound("create-delegreturn", &[&seq_op, &rootfh_op, &create_op]);
    let (mut resp, mut delegreturn_resp, callbacks) = tokio::time::timeout(
        Duration::from_secs(1),
        send_rpc_handling_callbacks_and_delegreturn(
            &mut stream,
            4,
            1,
            &compound,
            &sessionid,
            &stateid,
        ),
    )
    .await
    .expect("DELEGRETURN should unblock recall without waiting for recall_timeout");
    assert_eq!(callbacks, 1);

    parse_rpc_reply(&mut delegreturn_resp);
    let (status, _, num_results) = parse_compound_header(&mut delegreturn_resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let (opnum, op_status) = parse_op_header(&mut delegreturn_resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_sequence_res(&mut delegreturn_resp);
    let (opnum, op_status) = parse_op_header(&mut delegreturn_resp);
    assert_eq!(opnum, OP_DELEGRETURN);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CREATE);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("returned-stateid", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_TEST_STATEID);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(
        parse_test_stateid_results(&mut resp),
        vec![NfsStat4::BadStateid as u32]
    );
}

/// A cloneable control handle can recall a directory after the server starts.
/// Origin: design/delegations.md backend hook requirement.
/// RFC: RFC 8881 §§10.2, 18.37, 20.2.
#[tokio::test]
async fn test_control_handle_recalls_directory_delegation() {
    let config = DelegationConfig {
        directory_delegations: true,
        recall_timeout: Duration::from_secs(5),
        max_delegations_per_client: 1024,
        max_delegations_total: 16_384,
    };
    let fs = MemFs::new();
    let root = fs.root();
    let server = NfsServer::builder(fs).delegation_config(config).build();
    let control = server.control_handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    std::mem::drop(tokio::spawn(async move {
        server.serve(listener).await.unwrap();
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1004).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound("gdd-control", &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = parse_get_dir_delegation_ok(&mut resp);

    let recall_task = tokio::spawn(async move { control.recall_directory(&root).await });
    let record = read_record(&mut stream).await;
    let mut peek = record.clone();
    let _callback_xid = u32::decode(&mut peek).unwrap();
    assert_eq!(MsgType::decode(&mut peek).unwrap(), MsgType::Call);
    reply_to_callback(&mut stream, record).await;
    write_delegreturn_call(&mut stream, 0xD1E6_E001, &sessionid, &stateid).await;

    let mut delegreturn_resp = read_record(&mut stream).await;
    parse_rpc_reply(&mut delegreturn_resp);
    let (status, _, num_results) = parse_compound_header(&mut delegreturn_resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let _ = parse_op_header(&mut delegreturn_resp);
    skip_sequence_res(&mut delegreturn_resp);
    let (opnum, op_status) = parse_op_header(&mut delegreturn_resp);
    assert_eq!(opnum, OP_DELEGRETURN);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    tokio::time::timeout(Duration::from_secs(1), recall_task)
        .await
        .expect("control recall should finish after DELEGRETURN")
        .unwrap()
        .unwrap();
}
