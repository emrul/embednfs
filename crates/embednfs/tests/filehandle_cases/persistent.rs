//! Filehandles that survive a server restart.
//!
//! Each test boots two servers over one [`PersistentMemFs`]: the second has
//! fresh protocol state, as after a restart, over the same durable objects.

use std::sync::atomic::Ordering;

use embednfs::{FileSystem, NfsServer};

use super::*;

/// Run `ops` after a SEQUENCE and return the compound status.
async fn run(stream: &mut TcpStream, sessionid: &[u8; 16], seq: u32, ops: &[&[u8]]) -> u32 {
    let seq_op = encode_sequence(sessionid, seq, 0);
    let mut all: Vec<&[u8]> = vec![&seq_op];
    all.extend_from_slice(ops);
    let compound = encode_compound("persistent-fh", &all);
    let mut resp = send_rpc(stream, seq + 10, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    status
}

/// The filehandle of `name` under the root, as GETFH reports it.
async fn fh_of(stream: &mut TcpStream, sessionid: &[u8; 16], seq: u32, name: &str) -> Vec<u8> {
    let seq_op = encode_sequence(sessionid, seq, 0);
    let ops: Vec<Vec<u8>> = match name {
        "" => vec![encode_putrootfh(), encode_getfh()],
        _ => vec![encode_putrootfh(), encode_lookup(name), encode_getfh()],
    };
    let mut all: Vec<&[u8]> = vec![&seq_op];
    all.extend(ops.iter().map(Vec::as_slice));
    let compound = encode_compound("getfh", &all);
    let mut resp = send_rpc(stream, seq + 10, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32, "GETFH of {name:?}");
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    for _ in 1..ops.len() {
        let _ = parse_op_header(&mut resp);
    }
    let _ = parse_op_header(&mut resp);
    parse_getfh(&mut resp)
}

async fn session(port: u16) -> (TcpStream, [u8; 16]) {
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;
    (stream, sessionid)
}

#[tokio::test]
async fn a_filehandle_resolves_again_after_a_restart() {
    let fs = PersistentMemFs::new(populated_fs(&["a.txt"]).await);

    let first = start_server_with_fs(fs.clone()).await;
    let (mut a, a_sid) = session(first).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;
    let root = fh_of(&mut a, &a_sid, 2, "").await;
    assert_eq!(file[0], 0xA5, "a persistent backend gets tagged handles");

    let second = start_server_with_fs(fs).await;
    let (mut b, b_sid) = session(second).await;
    let getattr = encode_getattr(&[FATTR4_TYPE]);
    assert_eq!(
        run(&mut b, &b_sid, 1, &[&encode_putfh(&file), &getattr]).await,
        NfsStat4::Ok as u32,
        "the previous boot's file handle must resolve",
    );
    assert_eq!(
        fh_of(&mut b, &b_sid, 2, "").await,
        root,
        "the root keeps its handle across boots",
    );
    assert_eq!(
        fh_of(&mut b, &b_sid, 3, "a.txt").await,
        file,
        "a fresh lookup mints the same bytes the client already holds",
    );
}

#[tokio::test]
async fn a_removed_object_is_stale_after_a_restart() {
    let fs = PersistentMemFs::new(populated_fs(&["a.txt"]).await);
    let first = start_server_with_fs(fs.clone()).await;
    let (mut a, a_sid) = session(first).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;

    let ctx = embednfs::RequestContext::anonymous();
    fs.inner
        .remove(&ctx, &fs.inner.root(), "a.txt")
        .await
        .unwrap();

    let second = start_server_with_fs(fs).await;
    let (mut b, b_sid) = session(second).await;
    assert_eq!(
        run(
            &mut b,
            &b_sid,
            1,
            &[&encode_putfh(&file), &encode_getattr(&[FATTR4_TYPE])]
        )
        .await,
        NfsStat4::Stale as u32,
    );
}

#[tokio::test]
async fn a_backend_that_is_not_ready_yet_answers_delay() {
    let fs = PersistentMemFs::new(populated_fs(&["a.txt"]).await);
    let first = start_server_with_fs(fs.clone()).await;
    let (mut a, a_sid) = session(first).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;

    fs.decode_delayed.store(true, Ordering::SeqCst);
    let second = start_server_with_fs(fs.clone()).await;
    let (mut b, b_sid) = session(second).await;
    let getattr = encode_getattr(&[FATTR4_TYPE]);
    assert_eq!(
        run(&mut b, &b_sid, 1, &[&encode_putfh(&file), &getattr]).await,
        NfsStat4::Delay as u32,
        "not ready is a retry, never a stale handle",
    );
    fs.decode_delayed.store(false, Ordering::SeqCst);
    assert_eq!(
        run(&mut b, &b_sid, 2, &[&encode_putfh(&file), &getattr]).await,
        NfsStat4::Ok as u32,
    );
}

#[tokio::test]
async fn a_retired_handle_stays_stale_even_when_decodable() {
    let fs = PersistentMemFs::new(populated_fs(&["a.txt"]).await);
    let first = start_server_with_fs(fs.clone()).await;
    let (mut a, a_sid) = session(first).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;
    let id = u64::from_be_bytes(file[1..].try_into().unwrap());

    let server = NfsServer::new(fs);
    let control = server.control_handle();
    let _ = control.retire_handles(move |h: &u64| *h == id).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(tokio::spawn(async move {
        server.serve(listener).await.unwrap()
    }));

    let (mut b, b_sid) = session(port).await;
    assert_eq!(
        run(
            &mut b,
            &b_sid,
            1,
            &[&encode_putfh(&file), &encode_getattr(&[FATTR4_TYPE])]
        )
        .await,
        NfsStat4::Stale as u32,
        "the retirement fence outranks the backend's decoder",
    );
}

#[tokio::test]
async fn a_client_reclaims_its_open_after_a_restart() {
    let fs = PersistentMemFs::new(populated_fs(&["a.txt"]).await);
    let first = start_server_with_fs(fs.clone()).await;
    let (mut a, a_sid) = session(first).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;

    let second = start_server_with_fs(fs).await;
    let mut b = connect(second).await;
    let (b_sid, clientid) = setup_session_full(&mut b).await;
    let reclaim = encode_open_claim_previous(clientid, OpenDelegationType4::None as u32);
    assert_eq!(
        run(&mut b, &b_sid, 1, &[&encode_putfh(&file), &reclaim]).await,
        NfsStat4::Ok as u32,
        "CLAIM_PREVIOUS against the old handle is how a client keeps its open",
    );
    assert_eq!(
        run(&mut b, &b_sid, 2, &[&encode_reclaim_complete(false)]).await,
        NfsStat4::Ok as u32,
    );
}

#[tokio::test]
async fn a_backend_without_persistence_keeps_boot_scoped_handles() {
    let port = start_server_with_fs(populated_fs(&["a.txt"]).await).await;
    let (mut a, a_sid) = session(port).await;
    let file = fh_of(&mut a, &a_sid, 1, "a.txt").await;
    assert_eq!(file.len(), 16);
    assert_ne!(file[0], 0xA5, "a boot nonce never looks persistent");
}
