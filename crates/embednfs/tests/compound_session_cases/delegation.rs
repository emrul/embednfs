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

fn rpc_record_type(record: &Bytes) -> (u32, MsgType) {
    let mut peek = record.clone();
    let xid = u32::decode(&mut peek).unwrap();
    let msg_type = MsgType::decode(&mut peek).unwrap();
    (xid, msg_type)
}

async fn setup_named_session_with_callback(
    stream: &mut TcpStream,
    client_name: &[u8],
    cb_program: u32,
) -> [u8; 16] {
    let exchange_id_op = encode_exchange_id_with_name(client_name);
    let compound = encode_compound("exchange", &[&exchange_id_op]);
    let mut resp = send_rpc(stream, 1, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 1);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_EXCHANGE_ID);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (clientid, sequenceid) = skip_exchange_id_res(&mut resp);

    let create_session_op = if cb_program == 0 {
        encode_create_session(clientid, sequenceid)
    } else {
        encode_create_session_with_callback(clientid, sequenceid, cb_program)
    };
    let compound = encode_compound("create-session", &[&create_session_op]);
    let mut resp = send_rpc(stream, 2, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 1);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CREATE_SESSION);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (sessionid, _, flags) = parse_create_session_res_full(&mut resp);
    if cb_program == 0 {
        assert_eq!(flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN, 0);
    } else {
        assert_ne!(flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN, 0);
    }
    sessionid
}

async fn grant_root_directory_delegation(
    stream: &mut TcpStream,
    sessionid: &[u8; 16],
    sequenceid: u32,
    xid: u32,
    tag: &str,
) -> Stateid4 {
    let seq_op = encode_sequence(sessionid, sequenceid, 0);
    let rootfh_op = encode_putrootfh();
    let deleg_op = encode_get_dir_delegation();
    let compound = encode_compound(tag, &[&seq_op, &rootfh_op, &deleg_op]);
    let mut resp = send_rpc(stream, xid, 1, &compound).await;
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
    parse_get_dir_delegation_ok(&mut resp)
}

fn assert_compound_final_op_ok(resp: &mut Bytes, expected_results: u32, expected_final_op: u32) {
    parse_rpc_reply(resp);
    let (status, _, num_results) = parse_compound_header(resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, expected_results);
    for result_index in 0..num_results {
        let (opnum, op_status) = parse_op_header(resp);
        assert_eq!(op_status, NfsStat4::Ok as u32);
        if result_index == 0 {
            assert_eq!(opnum, OP_SEQUENCE);
            skip_sequence_res(resp);
        }
        if result_index + 1 == num_results {
            assert_eq!(opnum, expected_final_op);
        }
    }
}

async fn assert_same_client_mutation_has_no_callback(
    stream: &mut TcpStream,
    xid: u32,
    payload: &[u8],
    expected_results: u32,
    expected_final_op: u32,
) {
    let (mut resp, callbacks) = tokio::time::timeout(
        Duration::from_secs(1),
        send_rpc_handling_callbacks(stream, xid, 1, payload),
    )
    .await
    .expect("same-client mutation should not wait on its own directory delegation");
    assert_eq!(callbacks, 0);
    assert_compound_final_op_ok(&mut resp, expected_results, expected_final_op);
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

/// Same-client directory mutations do not recall that client's delegation.
/// Origin: RFC 8881 §10.9.2 same-client directory delegation rule.
/// RFC: RFC 8881 §10.9.2.
#[tokio::test]
async fn test_same_client_directory_mutations_do_not_recall_own_delegation() {
    let port = start_server_with_directory_delegations().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session_with_callback(&mut stream, 0x4000_1002).await;
    let stateid =
        grant_root_directory_delegation(&mut stream, &sessionid, 1, 3, "gdd-same-client").await;

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_create("same-client-open.txt");
    let compound = encode_compound("same-client-open-create", &[&seq_op, &rootfh_op, &open_op]);
    assert_same_client_mutation_has_no_callback(&mut stream, 4, &compound, 3, OP_OPEN).await;

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("same-client-dir");
    let compound = encode_compound("same-client-create", &[&seq_op, &rootfh_op, &create_op]);
    assert_same_client_mutation_has_no_callback(&mut stream, 5, &compound, 3, OP_CREATE).await;

    let seq_op = encode_sequence(&sessionid, 4, 0);
    let rootfh_op = encode_putrootfh();
    let remove_op = encode_remove("same-client-dir");
    let compound = encode_compound("same-client-remove", &[&seq_op, &rootfh_op, &remove_op]);
    assert_same_client_mutation_has_no_callback(&mut stream, 6, &compound, 3, OP_REMOVE).await;

    let seq_op = encode_sequence(&sessionid, 5, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("same-client-rename-from");
    let compound = encode_compound(
        "same-client-create-rename",
        &[&seq_op, &rootfh_op, &create_op],
    );
    assert_same_client_mutation_has_no_callback(&mut stream, 7, &compound, 3, OP_CREATE).await;

    let seq_op = encode_sequence(&sessionid, 6, 0);
    let rootfh_op = encode_putrootfh();
    let savefh_op = encode_savefh();
    let rename_op = encode_rename("same-client-rename-from", "same-client-rename-to");
    let compound = encode_compound(
        "same-client-rename",
        &[&seq_op, &rootfh_op, &savefh_op, &rootfh_op, &rename_op],
    );
    assert_same_client_mutation_has_no_callback(&mut stream, 8, &compound, 5, OP_RENAME).await;

    let seq_op = encode_sequence(&sessionid, 7, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("same-client-stateid-live", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut stream, 9, 1, &compound).await;
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
    let mut holder = connect(port).await;
    let holder_sessionid =
        setup_named_session_with_callback(&mut holder, b"recall-holder", 0x4000_1003).await;
    let stateid =
        grant_root_directory_delegation(&mut holder, &holder_sessionid, 1, 3, "gdd-recall").await;

    let mut mutator = connect(port).await;
    let mutator_sessionid =
        setup_named_session_with_callback(&mut mutator, b"recall-mutator", 0).await;

    let seq_op = encode_sequence(&mutator_sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("after-recall");
    let compound = encode_compound("create-recall", &[&seq_op, &rootfh_op, &create_op]);
    write_rpc_call(&mut mutator, 3, 1, &compound).await;

    let record = tokio::time::timeout(Duration::from_secs(1), read_record(&mut holder))
        .await
        .expect("other client should receive CB_RECALL");
    let (_, msg_type) = rpc_record_type(&record);
    assert_eq!(msg_type, MsgType::Call);
    reply_to_callback(&mut holder, record).await;

    let mut resp = tokio::time::timeout(Duration::from_secs(1), read_record(&mut mutator))
        .await
        .expect("mutation should complete after recall timeout revokes unreturned delegation");
    assert_compound_final_op_ok(&mut resp, 3, OP_CREATE);

    let seq_op = encode_sequence(&holder_sessionid, 2, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("revoked-stateid", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut holder, 4, 1, &compound).await;
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
    let mut holder = connect(port).await;
    let holder_sessionid =
        setup_named_session_with_callback(&mut holder, b"delegreturn-holder", 0x4000_1004).await;
    let stateid =
        grant_root_directory_delegation(&mut holder, &holder_sessionid, 1, 3, "gdd-recall-return")
            .await;

    let mut mutator = connect(port).await;
    let mutator_sessionid =
        setup_named_session_with_callback(&mut mutator, b"delegreturn-mutator", 0).await;

    let seq_op = encode_sequence(&mutator_sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("after-delegreturn");
    let compound = encode_compound("create-delegreturn", &[&seq_op, &rootfh_op, &create_op]);
    write_rpc_call(&mut mutator, 3, 1, &compound).await;

    let record = tokio::time::timeout(Duration::from_secs(1), read_record(&mut holder))
        .await
        .expect("other client should receive CB_RECALL");
    let (_, msg_type) = rpc_record_type(&record);
    assert_eq!(msg_type, MsgType::Call);
    reply_to_callback(&mut holder, record).await;

    match tokio::time::timeout(Duration::from_millis(50), read_record(&mut mutator)).await {
        Ok(record) => {
            let (early_xid, early_msg_type) = rpc_record_type(&record);
            panic!("received {early_msg_type:?} xid {early_xid} before DELEGRETURN");
        }
        Err(_) => {}
    }

    write_delegreturn_call(&mut holder, 0xD1E6_E002, &holder_sessionid, &stateid).await;

    let mut delegreturn_resp =
        tokio::time::timeout(Duration::from_secs(1), read_record(&mut holder))
            .await
            .expect("DELEGRETURN reply should arrive");
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

    let mut resp = tokio::time::timeout(Duration::from_secs(1), read_record(&mut mutator))
        .await
        .expect("DELEGRETURN should unblock recall without waiting for recall_timeout");
    assert_compound_final_op_ok(&mut resp, 3, OP_CREATE);

    let seq_op = encode_sequence(&holder_sessionid, 2, 0);
    let test_op = encode_test_stateid(&[stateid]);
    let compound = encode_compound("returned-stateid", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut holder, 5, 1, &compound).await;
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

/// Mutating delegation holder recalls other clients but not itself.
/// Origin: RFC 8881 §10.9.2 multi-client directory delegation rule.
/// RFC: RFC 8881 §10.9.2.
#[tokio::test]
async fn test_mutating_delegation_holder_recalls_other_client_only() {
    let config = DelegationConfig {
        directory_delegations: true,
        recall_timeout: Duration::from_secs(5),
        max_delegations_per_client: 1024,
        max_delegations_total: 16_384,
    };
    let port = start_server_with_delegation_config(config).await;
    let mut mutating_holder = connect(port).await;
    let mutating_sessionid =
        setup_named_session_with_callback(&mut mutating_holder, b"mutating-holder", 0x4000_1005)
            .await;
    let mutating_stateid = grant_root_directory_delegation(
        &mut mutating_holder,
        &mutating_sessionid,
        1,
        3,
        "gdd-mutating-holder",
    )
    .await;

    let mut other_holder = connect(port).await;
    let other_sessionid =
        setup_named_session_with_callback(&mut other_holder, b"other-holder", 0x4000_1006).await;
    let other_stateid = grant_root_directory_delegation(
        &mut other_holder,
        &other_sessionid,
        1,
        3,
        "gdd-other-holder",
    )
    .await;

    let seq_op = encode_sequence(&mutating_sessionid, 2, 0);
    let rootfh_op = encode_putrootfh();
    let create_op = encode_create_dir("mutating-holder-create");
    let compound = encode_compound("mutating-holder-create", &[&seq_op, &rootfh_op, &create_op]);
    write_rpc_call(&mut mutating_holder, 4, 1, &compound).await;

    match tokio::time::timeout(Duration::from_millis(50), read_record(&mut mutating_holder)).await {
        Ok(record) => {
            let (early_xid, early_msg_type) = rpc_record_type(&record);
            panic!(
                "received {early_msg_type:?} xid {early_xid} on mutating holder before other client returned"
            );
        }
        Err(_) => {}
    }

    let record = tokio::time::timeout(Duration::from_secs(1), read_record(&mut other_holder))
        .await
        .expect("other holder should receive CB_RECALL");
    let (_, msg_type) = rpc_record_type(&record);
    assert_eq!(msg_type, MsgType::Call);
    reply_to_callback(&mut other_holder, record).await;
    write_delegreturn_call(
        &mut other_holder,
        0xD1E6_E003,
        &other_sessionid,
        &other_stateid,
    )
    .await;

    let mut delegreturn_resp =
        tokio::time::timeout(Duration::from_secs(1), read_record(&mut other_holder))
            .await
            .expect("other holder DELEGRETURN reply should arrive");
    assert_compound_final_op_ok(&mut delegreturn_resp, 2, OP_DELEGRETURN);

    let record = tokio::time::timeout(Duration::from_secs(1), read_record(&mut mutating_holder))
        .await
        .expect("mutating holder request should complete after other holder returns");
    let (xid, msg_type) = rpc_record_type(&record);
    assert_eq!(xid, 4);
    assert_eq!(msg_type, MsgType::Reply);
    let mut resp = record;
    assert_compound_final_op_ok(&mut resp, 3, OP_CREATE);

    let seq_op = encode_sequence(&mutating_sessionid, 3, 0);
    let test_op = encode_test_stateid(&[mutating_stateid]);
    let compound = encode_compound("mutating-holder-stateid-live", &[&seq_op, &test_op]);
    let mut resp = send_rpc(&mut mutating_holder, 5, 1, &compound).await;
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
