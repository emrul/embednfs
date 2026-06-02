use tokio::net::TcpStream;

use embednfs_proto::{
    CREATE_SESSION4_FLAG_CONN_BACK_CHAN, NfsStat4, OP_CREATE_SESSION, OP_EXCHANGE_ID,
};

use super::encode::{
    encode_compound, encode_create_session, encode_create_session_with_callback, encode_exchange_id,
};
use super::parse::{
    parse_compound_header, parse_create_session_res, parse_create_session_res_full,
    parse_op_header, parse_rpc_reply_fields, skip_exchange_id_res,
};
use super::transport::send_rpc;

pub async fn setup_session(stream: &mut TcpStream) -> [u8; 16] {
    setup_session_with_cb_program(stream, 0).await
}

pub async fn setup_session_with_callback(stream: &mut TcpStream, cb_program: u32) -> [u8; 16] {
    setup_session_with_cb_program(stream, cb_program).await
}

async fn setup_session_with_cb_program(stream: &mut TcpStream, cb_program: u32) -> [u8; 16] {
    let exchange_id_op = encode_exchange_id();
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

pub async fn setup_session_full(stream: &mut TcpStream) -> ([u8; 16], u64) {
    let exchange_id_op = encode_exchange_id();
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

    let create_session_op = encode_create_session(clientid, sequenceid);
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

    let sessionid = parse_create_session_res(&mut resp);
    (sessionid, clientid)
}
