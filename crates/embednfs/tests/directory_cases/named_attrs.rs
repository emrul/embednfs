use super::*;
use embednfs::{CreateKind, CreateRequest, FileSystem, MemFs, RequestContext, SetAttrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// OPENATTR on a file with xattrs sets the current filehandle to the attribute directory.
/// Origin: RFC- and macOS-client-driven; not a direct pynfs one-to-one case.
/// RFC: RFC 8881 §18.17.
#[tokio::test]
async fn test_openattr_on_file_returns_attrdir() {
    let fs = fs_with_xattr("notes.txt", "user.demo", b"value").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let getattr_op = encode_getattr(&[FATTR4_TYPE]);
    let compound = encode_compound(
        "openattr",
        &[&seq_op, &rootfh_op, &lookup_op, &openattr_op, &getattr_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 5);

    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);

    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPENATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let fattr = Fattr4::decode(&mut resp).unwrap();
    let mut attr_vals = Bytes::from(fattr.attr_vals);
    let file_type = u32::decode(&mut attr_vals).unwrap();
    assert_eq!(file_type, NfsFtype4::AttrDir as u32);
}

/// OPENATTR followed by READDIR lists named attributes.
/// Origin: RFC- and macOS-client-driven; not a direct pynfs one-to-one case.
/// RFC: RFC 8881 §18.17.
#[tokio::test]
async fn test_openattr_readdir_lists_named_attrs() {
    let fs = fs_with_xattr("notes.txt", "user.demo", b"value").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_FILEID, FATTR4_TYPE]);
    let compound = encode_compound(
        "openattr-readdir",
        &[&seq_op, &rootfh_op, &lookup_op, &openattr_op, &readdir_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 5);

    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);

    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READDIR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (_, _, entries, eof) = parse_readdir_body(&mut resp);
    assert!(eof);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "user.demo");
}

/// OPENATTR followed by READDIR on a file with no named attributes returns an empty list.
/// Origin: Apple/macOS named-attribute workflow, equivalent to the empty-list intent of `pynfs` XATT10.
/// RFC: RFC 8881 §18.17.
#[tokio::test]
async fn test_openattr_readdir_empty_named_attr_dir() {
    let fs = populated_fs(&["notes.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_FILEID, FATTR4_TYPE]);
    let compound = encode_compound(
        "openattr-readdir-empty",
        &[&seq_op, &rootfh_op, &lookup_op, &openattr_op, &readdir_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 5);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READDIR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (_, _, entries, eof) = parse_readdir_body(&mut resp);
    assert!(eof);
    assert!(entries.is_empty());
}

/// Named attribute lookup and read works through the synthetic attribute directory.
/// Origin: RFC- and macOS-client-driven named-attribute workflow.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_lookup_and_read() {
    let fs = fs_with_xattr("notes.txt", "user.demo", b"value").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let lookup_xattr_op = encode_lookup("user.demo");
    let read_op = encode_read(0, 1024);
    let compound = encode_compound(
        "named-attr-read",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &lookup_xattr_op,
            &read_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 6);

    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);

    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert_eq!(data.as_ref(), b"value");
}

/// Looking up a missing named attribute returns `NFS4ERR_NOENT`.
/// Origin: Apple/macOS named-attribute workflow, equivalent to the missing-attribute intent of `pynfs` XATT2.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_lookup_missing_returns_noent() {
    let fs = populated_fs(&["notes.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let lookup_xattr_op = encode_lookup("user.missing");
    let compound = encode_compound(
        "named-attr-missing",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &lookup_xattr_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
    assert_eq!(num_results, 5);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_LOOKUP);
    assert_eq!(op_status, NfsStat4::Noent as u32);
}

/// Named attributes support open-create, write, close, read-back, and remove.
/// Origin: RFC- and macOS-client-driven named-attribute workflow.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_open_create_write_close_and_remove() {
    let fs = MemFs::new();
    let ctx = RequestContext::anonymous();
    let _file_id = fs
        .create(
            &ctx,
            &1,
            "notes.txt",
            CreateRequest {
                kind: CreateKind::File,
                attrs: SetAttrs::default(),
            },
        )
        .await
        .unwrap();
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(true);
    let open_xattr_op = encode_open_create("user.created");
    let getfh_op = encode_getfh();
    let compound = encode_compound(
        "named-attr-open-create",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &open_xattr_op,
            &getfh_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 6);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let stateid = skip_open_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let xattr_fh = parse_getfh(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putfh_op = encode_putfh(&xattr_fh);
    let write_op = encode_write(&stateid, 0, b"hello-xattr");
    let close_op = encode_close(&stateid);
    let compound = encode_compound(
        "named-attr-write-close",
        &[&seq_op, &putfh_op, &write_op, &close_op],
    );
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 4);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_WRITE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let written = u32::decode(&mut resp).unwrap();
    assert_eq!(written, 11);
    let _ = u32::decode(&mut resp).unwrap();
    let _ = decode_fixed_opaque(&mut resp, 8).unwrap();
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CLOSE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let _ = parse_stateid(&mut resp);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_FILEID, FATTR4_TYPE]);
    let compound = encode_compound(
        "named-attr-readdir-after-write",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &readdir_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (_, _, entries, _) = parse_readdir_body(&mut resp);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "user.created");

    let seq_op = encode_sequence(&sessionid, 4, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let remove_op = encode_remove("user.created");
    let compound = encode_compound(
        "named-attr-remove",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &remove_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 6, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 5);
}

/// OPENATTR + OPEN(CREATE, GUARDED) on an existing named attribute returns `NFS4ERR_EXIST`.
/// Origin: Apple/macOS named-attribute workflow, equivalent to the exclusive-create intent of `pynfs` XATT6.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_guarded_create_existing_returns_exist() {
    let fs = fs_with_xattr("notes.txt", "user.demo", b"value").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(true);
    let open_xattr_op = encode_open_create_guarded("user.demo");
    let compound = encode_compound(
        "named-attr-guarded-exist",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &open_xattr_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Exist as u32);
    assert_eq!(num_results, 5);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Exist as u32);
}

/// OPENATTR + OPEN(NOCREATE) on a missing named attribute returns `NFS4ERR_NOENT`.
/// Origin: Apple/macOS named-attribute workflow, equivalent to the replace-missing intent of `pynfs` XATT5.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_open_nocreate_missing_returns_noent() {
    let fs = populated_fs(&["notes.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(true);
    let open_xattr_op = encode_open_nocreate("user.missing");
    let compound = encode_compound(
        "named-attr-open-missing",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &open_xattr_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
    assert_eq!(num_results, 5);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Noent as u32);
}

/// Reopening an existing named attribute and writing replaces its content.
/// Origin: Apple/macOS named-attribute workflow, covering the update-existing intent of `pynfs` XATT7.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_reopen_and_replace_value() {
    let fs = fs_with_xattr("notes.txt", "user.demo", b"value1").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(true);
    let open_xattr_op = encode_open_nocreate_with_access(
        "user.demo",
        OPEN4_SHARE_ACCESS_BOTH,
        OPEN4_SHARE_DENY_NONE,
    );
    let getfh_op = encode_getfh();
    let compound = encode_compound(
        "named-attr-reopen",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &open_xattr_op,
            &getfh_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 6);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let stateid = skip_open_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let xattr_fh = parse_getfh(&mut resp);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putfh_op = encode_putfh(&xattr_fh);
    let write_op = encode_write(&stateid, 0, b"value2");
    let close_op = encode_close(&stateid);
    let compound = encode_compound(
        "named-attr-rewrite",
        &[&seq_op, &putfh_op, &write_op, &close_op],
    );
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let lookup_xattr_op = encode_lookup("user.demo");
    let read_op = encode_read(0, 1024);
    let compound = encode_compound(
        "named-attr-read-replaced",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &lookup_xattr_op,
            &read_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert_eq!(data.as_ref(), b"value2");
}

/// Removing a missing named attribute returns `NFS4ERR_NOENT`.
/// Origin: Apple/macOS named-attribute workflow, equivalent to the remove-missing intent of `pynfs` XATT8.
/// RFC: RFC 8881 §5.3.
#[tokio::test]
async fn test_named_attr_remove_missing_returns_noent() {
    let fs = populated_fs(&["notes.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let remove_op = encode_remove("user.missing");
    let compound = encode_compound(
        "named-attr-remove-missing",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file_op,
            &openattr_op,
            &remove_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
    assert_eq!(num_results, 5);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Noent as u32);
}

// ===== Named-attribute OPEN authorization (fail-closed OPEN) =====

/// Builds a single-file `MemFs` with the given parent mode and optional xattr.
async fn file_with_mode_xattr(name: &str, mode: u32, xattr: Option<(&str, &[u8])>) -> MemFs {
    let fs = MemFs::new();
    let ctx = RequestContext::anonymous();
    let id = fs
        .create(
            &ctx,
            &1,
            name,
            CreateRequest {
                kind: CreateKind::File,
                attrs: SetAttrs {
                    mode: Some(mode),
                    ..SetAttrs::default()
                },
            },
        )
        .await
        .unwrap()
        .handle;
    if let Some((key, value)) = xattr {
        fs.set_xattr(
            &ctx,
            &id,
            key,
            Bytes::copy_from_slice(value),
            XattrSetMode::CreateOnly,
        )
        .await
        .unwrap();
    }
    fs
}

/// Drives `SEQUENCE → PUTROOTFH → LOOKUP(notes.txt) → OPENATTR → <open_op>` and
/// returns the OPEN op status (or OPENATTR's status if that step itself fails).
async fn named_attr_open_status(fs: AccessPolicyFs, create_dir: bool, open_op: &[u8]) -> u32 {
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(create_dir);
    let compound = encode_compound(
        "named-attr-authz",
        &[&seq_op, &rootfh_op, &lookup_op, &openattr_op, open_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (_status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_LOOKUP);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPENATTR);
    if op_status != NfsStat4::Ok as u32 {
        return op_status;
    }
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    op_status
}

/// A write OPEN of an existing named attribute whose parent denies XATTR_WRITE
/// (`0444`) returns `NFS4ERR_ACCESS` — the macOS OPENATTR path is gated like
/// the RFC 8276 SETXATTR op.
/// Origin: fail-closed OPEN extended to synthetic named-attribute files.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.3 (XATTR_WRITE).
#[tokio::test]
async fn test_named_attr_write_open_denied_when_parent_readonly() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_nocreate_with_access(
        "user.demo",
        OPEN4_SHARE_ACCESS_WRITE,
        OPEN4_SHARE_DENY_NONE,
    );
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
}

/// A read OPEN of an existing named attribute whose parent denies XATTR_READ
/// (`0000`) returns `NFS4ERR_ACCESS`.
/// Origin: fail-closed OPEN extended to synthetic named-attribute files.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.2 (XATTR_READ).
#[tokio::test]
async fn test_named_attr_read_open_denied_when_parent_unreadable() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o000, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_nocreate_with_access(
        "user.demo",
        OPEN4_SHARE_ACCESS_READ,
        OPEN4_SHARE_DENY_NONE,
    );
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
}

/// OPEN+CREATE of a named attribute is denied before any mutation when the
/// parent denies XATTR_WRITE (`0444`).
/// Origin: fail-closed OPEN — creating an xattr requires XATTR_WRITE.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.3 (XATTR_WRITE).
#[tokio::test]
async fn test_named_attr_create_open_denied_when_parent_readonly() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, None).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_create("user.new");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
}

/// OPEN+CREATE of a named attribute succeeds when the parent grants XATTR_WRITE
/// (`0644`) — the gate does not reject opens the backend would permit.
/// Origin: fail-closed OPEN must not over-restrict the named-attribute path.
/// RFC: RFC 8881 §5.3, §18.16.3.
#[tokio::test]
async fn test_named_attr_create_open_allowed_when_parent_writable() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o644, None).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_create("user.new");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Ok as u32
    );
}

// ===== Named-attribute namespace authorization (LOOKUP/READDIR/READ/REMOVE) =====

/// Runs `SEQUENCE → PUTROOTFH → LOOKUP(notes.txt) → OPENATTR → <tail>` where the
/// final tail op is expected to be the authorization decision, and returns that
/// last op's status. Every op the server executes before it is bodyless, so the
/// compound short-circuits on the first denial.
async fn named_attr_tail_status(fs: AccessPolicyFs, tail: &[&[u8]]) -> u32 {
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let mut ops: Vec<&[u8]> = vec![&seq_op, &rootfh_op, &lookup_file, &openattr_op];
    ops.extend_from_slice(tail);
    let compound = encode_compound("named-attr-namespace", &ops);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (_status, _, num_results) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let mut last = NfsStat4::Ok as u32;
    for _ in 1..num_results {
        let (_opnum, op_status) = parse_op_header(&mut resp);
        last = op_status;
    }
    last
}

/// LOOKUP within the attribute directory is denied when the parent withholds
/// XATTR_LIST (`0000`), so a name probe cannot leak past the backend.
/// Origin: central XATTR_LIST gate for the synthetic attribute directory.
/// RFC: RFC 8881 §5.3, §18.31.3; RFC 8276 §5.2.
#[tokio::test]
async fn test_named_attr_lookup_denied_without_xattr_list() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o000, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let lookup_xattr = encode_lookup("user.demo");
    assert_eq!(
        named_attr_tail_status(fs, &[&lookup_xattr]).await,
        NfsStat4::Access as u32
    );
}

/// READDIR of the attribute directory is denied when the parent withholds
/// XATTR_LIST (`0000`).
/// Origin: central XATTR_LIST gate for the synthetic attribute directory.
/// RFC: RFC 8881 §5.3, §18.23.3; RFC 8276 §5.2.
#[tokio::test]
async fn test_named_attr_readdir_denied_without_xattr_list() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o000, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_TYPE]);
    assert_eq!(
        named_attr_tail_status(fs, &[&readdir_op]).await,
        NfsStat4::Access as u32
    );
}

/// READ of an attribute value is denied when the parent grants XATTR_LIST but
/// not XATTR_READ — proving the READ gate is independent of the LOOKUP gate,
/// closing the LOOKUP+anonymous-READ side channel that OPEN alone did not cover.
/// Origin: central XATTR_READ gate on the named-attribute data path.
/// RFC: RFC 8881 §5.3, §18.22.3; RFC 8276 §5.2.
#[tokio::test]
async fn test_named_attr_read_denied_when_parent_grants_list_only() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o644, Some(("user.demo", b"value"))).await,
        AccessPolicy::XattrListOnly,
    );
    let lookup_xattr = encode_lookup("user.demo");
    let read_op = encode_read(0, 1024);
    assert_eq!(
        named_attr_tail_status(fs, &[&lookup_xattr, &read_op]).await,
        NfsStat4::Access as u32
    );
}

/// REMOVE of a named attribute is denied when the parent withholds XATTR_WRITE
/// (`0444`), while LISTing it is still permitted.
/// Origin: central XATTR_WRITE gate for attribute removal.
/// RFC: RFC 8881 §5.3, §18.25.3; RFC 8276 §5.3.
#[tokio::test]
async fn test_named_attr_remove_denied_without_xattr_write() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let remove_op = encode_remove("user.demo");
    assert_eq!(
        named_attr_tail_status(fs, &[&remove_op]).await,
        NfsStat4::Access as u32
    );
}

/// READDIR of the attribute directory succeeds when the parent grants XATTR_LIST
/// (`0444`) — the central gate does not over-restrict permitted listing.
/// Origin: positive control for the XATTR_LIST gate.
/// RFC: RFC 8881 §5.3, §18.23.3.
#[tokio::test]
async fn test_named_attr_readdir_allowed_with_xattr_list() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_FILEID, FATTR4_TYPE]);
    assert_eq!(
        named_attr_tail_status(fs, &[&readdir_op]).await,
        NfsStat4::Ok as u32
    );
}

/// ACCESS on a named-attribute file derives its bits from the parent's xattr
/// rights: a read-only parent (`0444`) grants READ but not MODIFY/EXTEND/DELETE,
/// and never advertises LOOKUP, EXECUTE, or the XATTR_* bits.
/// Origin: ACCESS reporting consistency for synthetic attribute files.
/// RFC: RFC 8881 §5.3, §18.1.3; RFC 8276 §5.2, §5.3.
#[tokio::test]
async fn test_access_on_named_attr_file_reflects_parent_xattr_rights() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let request = ACCESS4_READ
        | ACCESS4_LOOKUP
        | ACCESS4_MODIFY
        | ACCESS4_EXTEND
        | ACCESS4_DELETE
        | ACCESS4_EXECUTE
        | ACCESS4_XAREAD
        | ACCESS4_XAWRITE
        | ACCESS4_XALIST;
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let lookup_xattr = encode_lookup("user.demo");
    let access_op = encode_access(request);
    let compound = encode_compound(
        "access-named-attr-file",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_file,
            &openattr_op,
            &lookup_xattr,
            &access_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let (supported, access) = access_after_prefix(&mut resp);
    assert_eq!(
        supported,
        ACCESS4_READ | ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE
    );
    assert_eq!(access, ACCESS4_READ);
}

/// ACCESS on a named-attribute directory derives its bits from the parent's
/// xattr rights: a read-only parent (`0444`) grants READ|LOOKUP (from
/// XATTR_LIST) but not the mutating bits, and never advertises EXECUTE or
/// XATTR_*.
/// Origin: ACCESS reporting consistency for the synthetic attribute directory.
/// RFC: RFC 8881 §5.3, §18.1.3; RFC 8276 §5.2, §5.3.
#[tokio::test]
async fn test_access_on_named_attr_dir_reflects_parent_xattr_rights() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let request = ACCESS4_READ
        | ACCESS4_LOOKUP
        | ACCESS4_MODIFY
        | ACCESS4_EXTEND
        | ACCESS4_DELETE
        | ACCESS4_EXECUTE
        | ACCESS4_XAREAD
        | ACCESS4_XAWRITE
        | ACCESS4_XALIST;
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_file = encode_lookup("notes.txt");
    let openattr_op = encode_openattr(false);
    let access_op = encode_access(request);
    let compound = encode_compound(
        "access-named-attr-dir",
        &[&seq_op, &rootfh_op, &lookup_file, &openattr_op, &access_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let (supported, access) = access_after_prefix(&mut resp);
    assert_eq!(
        supported,
        ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE
    );
    assert_eq!(access, ACCESS4_READ | ACCESS4_LOOKUP);
}

/// Walks bodyless op headers (SEQUENCE excepted) until the ACCESS result and
/// returns its `(supported, access)` pair.
fn access_after_prefix(resp: &mut Bytes) -> (u32, u32) {
    let _ = parse_op_header(resp);
    skip_sequence_res(resp);
    loop {
        let (opnum, op_status) = parse_op_header(resp);
        if opnum == OP_ACCESS {
            assert_eq!(op_status, NfsStat4::Ok as u32);
            return parse_access_res(resp);
        }
        assert_eq!(op_status, NfsStat4::Ok as u32);
    }
}

// ===== OPEN must not leak attribute existence to an unauthorized caller =====

/// OPEN4_NOCREATE of a missing attribute returns `NFS4ERR_ACCESS`, not
/// `NFS4ERR_NOENT`, when the parent withholds XATTR_READ — the existence probe
/// runs only after the up-front gate, so absence cannot leak.
/// Origin: OPEN existence-leak closure for the synthetic attribute namespace.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.2.
#[tokio::test]
async fn test_named_attr_nocreate_open_missing_denied_without_xattr_read() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o000, None).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_nocreate("user.missing");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
}

/// OPEN4_NOCREATE of a missing attribute still returns `NFS4ERR_NOENT` to a
/// caller the parent grants XATTR_READ — absence is reported only to the
/// authorized.
/// Origin: positive control for OPEN existence reporting.
/// RFC: RFC 8881 §5.3, §18.16.3.
#[tokio::test]
async fn test_named_attr_nocreate_open_missing_noent_when_readable() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, None).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_nocreate("user.missing");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Noent as u32
    );
}

/// OPEN(CREATE, GUARDED) of an existing attribute returns `NFS4ERR_ACCESS`, not
/// `NFS4ERR_EXIST`, when the parent withholds XATTR_WRITE — presence cannot leak
/// to a caller who could not have created it.
/// Origin: OPEN existence-leak closure for the synthetic attribute namespace.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.3.
#[tokio::test]
async fn test_named_attr_guarded_create_existing_denied_without_xattr_write() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_create_guarded("user.demo");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
}

/// OPEN(CREATE, GUARDED) of an existing attribute still returns `NFS4ERR_EXIST`
/// to a caller the parent grants XATTR_WRITE.
/// Origin: positive control for OPEN existence reporting.
/// RFC: RFC 8881 §5.3, §18.16.3.
#[tokio::test]
async fn test_named_attr_guarded_create_existing_exist_when_writable() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o644, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let open_op = encode_open_create_guarded("user.demo");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Exist as u32
    );
}

// ===== A denied synthetic-namespace op must not touch the backend first =====

/// A denied READDIR never reaches the backend `list_xattrs` — the XATTR_LIST
/// gate runs before `build_attr` lists the parent's attributes.
/// Origin: ordering guarantee for the central named-attribute gate.
/// RFC: RFC 8881 §5.3, §18.23.3; RFC 8276 §5.2.
#[tokio::test]
async fn test_named_attr_readdir_denial_does_not_touch_backend() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o000, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let calls = fs.calls();
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, &[FATTR4_TYPE]);
    assert_eq!(
        named_attr_tail_status(fs, &[&readdir_op]).await,
        NfsStat4::Access as u32
    );
    assert_eq!(calls.list.load(Ordering::Relaxed), 0);
}

/// A denied REMOVE never reaches the backend — neither the `build_attr`
/// listing nor `remove_xattr` runs ahead of the XATTR_WRITE gate.
/// Origin: ordering guarantee for the central named-attribute gate.
/// RFC: RFC 8881 §5.3, §18.25.3; RFC 8276 §5.3.
#[tokio::test]
async fn test_named_attr_remove_denial_does_not_touch_backend() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, Some(("user.demo", b"value"))).await,
        AccessPolicy::OwnerMode,
    );
    let calls = fs.calls();
    let remove_op = encode_remove("user.demo");
    assert_eq!(
        named_attr_tail_status(fs, &[&remove_op]).await,
        NfsStat4::Access as u32
    );
    assert_eq!(calls.list.load(Ordering::Relaxed), 0);
    assert_eq!(calls.remove.load(Ordering::Relaxed), 0);
}

/// A denied OPEN+CREATE never probes or writes the backend — the up-front gate
/// precedes both the existence probe and the create.
/// Origin: ordering guarantee for the OPEN existence-leak fix.
/// RFC: RFC 8881 §5.3, §18.16.3; RFC 8276 §5.3.
#[tokio::test]
async fn test_named_attr_open_create_denial_does_not_touch_backend() {
    let fs = AccessPolicyFs::new(
        file_with_mode_xattr("notes.txt", 0o444, None).await,
        AccessPolicy::OwnerMode,
    );
    let calls = fs.calls();
    let open_op = encode_open_create("user.new");
    assert_eq!(
        named_attr_open_status(fs, true, &open_op).await,
        NfsStat4::Access as u32
    );
    assert_eq!(calls.get.load(Ordering::Relaxed), 0);
    assert_eq!(calls.set.load(Ordering::Relaxed), 0);
    assert_eq!(calls.list.load(Ordering::Relaxed), 0);
}

/// GETATTR on a file caches its named-attribute summary.
/// Origin: implementation-specific cache behavior.
/// RFC: RFC 8881 §5.3, §18.7.3.
#[tokio::test]
async fn test_getattr_file_named_attr_summary_is_cached() {
    let inner = fs_with_xattr("cached.txt", "user.demo", b"value").await;
    let list_count = Arc::new(AtomicUsize::new(0));
    let fs = CountingNamedAttrFs {
        inner,
        list_count: list_count.clone(),
    };
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    for (xid, seq) in [(3, 1), (4, 2)] {
        let seq_op = encode_sequence(&sessionid, seq, 0);
        let rootfh_op = encode_putrootfh();
        let lookup_op = encode_lookup("cached.txt");
        let getattr_op = encode_getattr(&[FATTR4_NAMED_ATTR]);
        let compound = encode_compound(
            "getattr-file-cache",
            &[&seq_op, &rootfh_op, &lookup_op, &getattr_op],
        );
        let mut resp = send_rpc(&mut stream, xid, 1, &compound).await;
        parse_rpc_reply(&mut resp);
        let (status, _, _) = parse_compound_header(&mut resp);
        assert_eq!(status, NfsStat4::Ok as u32);
    }

    assert_eq!(list_count.load(Ordering::Relaxed), 0);
}

/// GETATTR on a named-attribute directory caches its summary metadata.
/// Origin: implementation-specific cache behavior.
/// RFC: RFC 8881 §5.3, §18.7.3.
#[tokio::test]
async fn test_getattr_named_attr_dir_summary_is_cached() {
    let inner = fs_with_xattr("cached.txt", "user.demo", b"value").await;
    let list_count = Arc::new(AtomicUsize::new(0));
    let fs = CountingNamedAttrFs {
        inner,
        list_count: list_count.clone(),
    };
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    for (xid, seq) in [(3, 1), (4, 2)] {
        let seq_op = encode_sequence(&sessionid, seq, 0);
        let rootfh_op = encode_putrootfh();
        let lookup_op = encode_lookup("cached.txt");
        let openattr_op = encode_openattr(false);
        let getattr_op = encode_getattr(&[FATTR4_TYPE, FATTR4_SIZE]);
        let compound = encode_compound(
            "getattr-attrdir-cache",
            &[&seq_op, &rootfh_op, &lookup_op, &openattr_op, &getattr_op],
        );
        let mut resp = send_rpc(&mut stream, xid, 1, &compound).await;
        parse_rpc_reply(&mut resp);
        let (status, _, _) = parse_compound_header(&mut resp);
        assert_eq!(status, NfsStat4::Ok as u32);
    }

    assert_eq!(list_count.load(Ordering::Relaxed), 1);
}
