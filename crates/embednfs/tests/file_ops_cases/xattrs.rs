use super::*;

/// NFSv4.2 xattr operations set, list, get, and remove user metadata.
/// Origin: RFC 8276 §8.4.
/// RFC: RFC 8276 §8.4.1, §8.4.2, §8.4.3, §8.4.4.
#[tokio::test]
async fn test_v42_xattr_round_trip_on_current_filehandle() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let putrootfh_op = encode_putrootfh();
    let setxattr_op = encode_setxattr(0, "user.embednfs_probe", b"value");
    let compound = encode_compound_minor("setxattr", 2, &[&seq_op, &putrootfh_op, &setxattr_op]);
    let mut resp = send_rpc(&mut stream, 10, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SETXATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_change_info(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putrootfh_op = encode_putrootfh();
    let listxattrs_op = encode_listxattrs(0, 4096);
    let getxattr_op = encode_getxattr("user.embednfs_probe");
    let compound = encode_compound_minor(
        "getxattr",
        2,
        &[&seq_op, &putrootfh_op, &listxattrs_op, &getxattr_op],
    );
    let mut resp = send_rpc(&mut stream, 11, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 4);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_LISTXATTRS);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let _cookie = u64::decode(&mut resp).unwrap();
    let names = decode_list::<String>(&mut resp).unwrap();
    let eof = bool::decode(&mut resp).unwrap();
    assert!(eof);
    assert_eq!(names, vec!["user.embednfs_probe".to_string()]);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETXATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(decode_opaque(&mut resp).unwrap().as_ref(), b"value");

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let putrootfh_op = encode_putrootfh();
    let removexattr_op = encode_removexattr("user.embednfs_probe");
    let compound =
        encode_compound_minor("removexattr", 2, &[&seq_op, &putrootfh_op, &removexattr_op]);
    let mut resp = send_rpc(&mut stream, 12, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVEXATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_change_info(&mut resp);

    let seq_op = encode_sequence(&sessionid, 4, 0);
    let putrootfh_op = encode_putrootfh();
    let getxattr_op = encode_getxattr("user.embednfs_probe");
    let compound =
        encode_compound_minor("missing-xattr", 2, &[&seq_op, &putrootfh_op, &getxattr_op]);
    let mut resp = send_rpc(&mut stream, 13, 1, &compound).await;
    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::NoXattr as u32);
    assert_eq!(num_results, 3);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SEQUENCE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETXATTR);
    assert_eq!(op_status, NfsStat4::NoXattr as u32);
}
