use super::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ===== OPEN + CLOSE (pynfs OPEN, CLOSE) =====

/// OPEN with `CLAIM_NULL` and `OPEN4_CREATE` creates a new file.
/// Origin: derived from `pynfs/nfs4.0/servertests/st_open.py` (CODE `MKFILE`).
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_create_new_file() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_create("new-file.txt");
    let getfh_op = encode_getfh();
    let compound = encode_compound("open-create", &[&seq_op, &rootfh_op, &open_op, &getfh_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 4);

    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let stateid = skip_open_res(&mut resp);
    assert_ne!(stateid.other, [0u8; 12]); // Valid stateid

    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let fh = parse_getfh(&mut resp);
    assert!(!fh.is_empty());
}

/// OPEN with `OPEN4_NOCREATE` on an existing file succeeds.
/// Origin: `pynfs/nfs4.0/servertests/st_open.py` (CODE `OPEN5`).
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_nocreate_existing_file() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_nocreate("existing.txt");
    let compound = encode_compound("open-nocreate", &[&seq_op, &rootfh_op, &open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
}

/// OPEN with `OPEN4_NOCREATE` on a non-existent file returns `NFS4ERR_NOENT`.
/// Origin: `pynfs/nfs4.0/servertests/st_open.py` (CODE `OPEN6`).
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_nocreate_nonexistent() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_nocreate("ghost.txt");
    let compound = encode_compound("open-noent", &[&seq_op, &rootfh_op, &open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
}

/// OPEN with `state_owner4.owner` longer than 1024 bytes returns `NFS4ERR_BADXDR`.
/// Origin: RFC 8881 `state_owner4` length bound; no direct pynfs one-to-one case.
/// RFC: RFC 8881 §3.3.10.
#[tokio::test]
async fn test_open_owner_too_long_returns_badxdr() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;
    let long_owner = vec![b'o'; 1025];

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_nocreate_with_owner("ghost.txt", &long_owner);
    let compound = encode_compound("open-owner-too-long", &[&seq_op, &rootfh_op, &open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::BadXdr as u32);
    assert_eq!(num_results, 0);
}

// ===== OPEN authorization (fail-closed OPEN) =====

/// Creates a single-file `MemFs` whose file carries the given POSIX mode.
async fn memfs_with_mode(name: &str, mode: u32) -> MemFs {
    let fs = MemFs::new();
    let ctx = RequestContext::anonymous();
    let _ = fs
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
        .unwrap();
    fs
}

/// Drives `SEQUENCE → PUTROOTFH → <open_op>` and returns the OPEN op status.
async fn open_status(port: u16, open_op: &[u8]) -> u32 {
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let compound = encode_compound("open-authz", &[&seq_op, &rootfh_op, open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (_status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    op_status
}

/// A write OPEN of an existing read-only (`0444`) file returns `NFS4ERR_ACCESS`.
/// Origin: fail-closed OPEN — OPEN must honor the permission its share_access implies.
/// RFC: RFC 8881 §18.16.3 (NFS4ERR_ACCESS), §6.2.1.3.1 (ACE4_WRITE_DATA).
#[tokio::test]
async fn test_open_write_on_readonly_file_denied() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("ro.txt", 0o444).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let open_op =
        encode_open_nocreate_with_access("ro.txt", OPEN4_SHARE_ACCESS_WRITE, OPEN4_SHARE_DENY_NONE);
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Access as u32);
}

/// A read OPEN of an existing unreadable (`0000`) file returns `NFS4ERR_ACCESS`.
/// Origin: fail-closed OPEN — OPEN must honor the permission its share_access implies.
/// RFC: RFC 8881 §18.16.3 (NFS4ERR_ACCESS), §6.2.1.3.1 (ACE4_READ_DATA).
#[tokio::test]
async fn test_open_read_on_unreadable_file_denied() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("noperm.txt", 0o000).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "noperm.txt",
        OPEN4_SHARE_ACCESS_READ,
        OPEN4_SHARE_DENY_NONE,
    );
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Access as u32);
}

/// A write OPEN of an existing writable (`0644`) file still succeeds.
/// Origin: fail-closed OPEN must not reject opens the backend would permit.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_write_on_writable_file_allowed() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("rw.txt", 0o644).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let open_op =
        encode_open_nocreate_with_access("rw.txt", OPEN4_SHARE_ACCESS_WRITE, OPEN4_SHARE_DENY_NONE);
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Ok as u32);
}

/// OPEN+CREATE of a new file succeeds even when its create mode is restrictive.
/// Origin: fail-closed OPEN exempts freshly created files (POSIX open-after-create).
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_create_restrictive_mode_allowed() {
    let fs = AccessPolicyFs::new(MemFs::new(), AccessPolicy::OwnerMode);
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_create_with_mode("fresh.txt", OPEN4_SHARE_ACCESS_BOTH, 0o444);
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Ok as u32);
}

/// OPEN+CREATE against an existing read-only file is authorized as a plain OPEN.
/// Origin: fail-closed OPEN — an OPEN4_CREATE that finds an existing file must
/// still pass the target-file access check.
/// RFC: RFC 8881 §18.16.3 (NFS4ERR_ACCESS).
#[tokio::test]
async fn test_open_create_existing_readonly_file_denied() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("exists.txt", 0o444).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    // encode_open_create requests OPEN4_SHARE_ACCESS_BOTH (write included).
    let open_op = encode_open_create("exists.txt");
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Access as u32);
}

/// A write OPEN against a read-only export is denied via `FileSystem::access`.
/// Origin: fail-closed OPEN — a read-only backend tree rejects write opens.
/// RFC: RFC 8881 §18.16.3 (NFS4ERR_ACCESS).
#[tokio::test]
async fn test_open_write_on_readonly_export_denied() {
    let fs = AccessPolicyFs::new(populated_fs(&["file.txt"]).await, AccessPolicy::ReadOnly);
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "file.txt",
        OPEN4_SHARE_ACCESS_WRITE,
        OPEN4_SHARE_DENY_NONE,
    );
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Access as u32);
}

// ===== OPEN share_access / share_deny validation =====

/// OPEN with `share_access == 0` is rejected with `NFS4ERR_INVAL`. A zero access
/// mode would otherwise pass the fail-closed gate with an empty required-
/// permission set and yield a stateid that later READ/WRITE would honor.
/// Origin: RFC 8881 share_access well-formedness; guards the OPEN gate.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_zero_share_access_returns_inval() {
    let port = start_server().await;
    let open_op = encode_open_nocreate_with_access("anything.txt", 0, OPEN4_SHARE_DENY_NONE);
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Inval as u32);
}

/// OPEN with a `share_deny` outside NONE/READ/WRITE/BOTH is rejected.
/// Origin: RFC 8881 share_deny well-formedness.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_invalid_share_deny_returns_inval() {
    let port = start_server().await;
    let open_op = encode_open_nocreate_with_access("anything.txt", OPEN4_SHARE_ACCESS_READ, 0x4);
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Inval as u32);
}

/// OPEN whose `share_access` carries bits outside the access mode and the
/// recognized want-delegation hints is rejected.
/// Origin: RFC 8881 share_access well-formedness.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_unknown_share_access_bits_return_inval() {
    let port = start_server().await;
    let open_op = encode_open_nocreate_with_access(
        "anything.txt",
        OPEN4_SHARE_ACCESS_READ | 0x10,
        OPEN4_SHARE_DENY_NONE,
    );
    assert_eq!(open_status(port, &open_op).await, NfsStat4::Inval as u32);
}

/// Drives `SEQUENCE → PUTROOTFH → <open_op>`, asserts the OPEN succeeds, and
/// returns its delegation type plus any `why_no_delegation4` reason.
async fn open_delegation(port: u16, open_op: &[u8]) -> (u32, Option<u32>) {
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let compound = encode_compound("open-deleg", &[&seq_op, &rootfh_op, open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (_status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    parse_open_res_delegation(&mut resp)
}

/// A plain OPEN with no want bits returns `OPEN_DELEGATE_NONE` (no reason).
/// Origin: response-shape control for the no-delegation path.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_without_want_bits_returns_plain_none() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::None as u32);
    assert_eq!(why, None);
}

/// A v4.1 OPEN that asks for a delegation it cannot get is accepted, but the
/// reply must use `OPEN_DELEGATE_NONE_EXT` and say why — this server supports no
/// delegations, so `WND4_NOT_SUPP_FTYPE`.
/// Origin: response-shape requirement for want-delegation OPENs.
/// RFC: RFC 8881 §18.16.3, §10.4.1.
#[tokio::test]
async fn test_open_v41_want_read_deleg_returns_none_ext_not_supp() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_READ_DELEG,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::NoneExt as u32);
    assert_eq!(why, Some(WhyNoDelegation4::NotSuppFtype as u32));
}

/// A v4.1 OPEN that asks for no delegation (`WANT_NO_DELEG`) gets
/// `OPEN_DELEGATE_NONE_EXT` with `WND4_NOT_WANTED`.
/// Origin: response-shape requirement for want-delegation OPENs.
/// RFC: RFC 8881 §18.16.3, §10.4.1.
#[tokio::test]
async fn test_open_v41_want_no_deleg_returns_none_ext_not_wanted() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_NO_DELEG,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::NoneExt as u32);
    assert_eq!(why, Some(WhyNoDelegation4::NotWanted as u32));
}

/// A v4.1 OPEN with `WANT_CANCEL` gets `OPEN_DELEGATE_NONE_EXT` with
/// `WND4_CANCELED`, not the generic not-supported reason.
/// Origin: response-shape requirement for want-delegation OPENs.
/// RFC: RFC 8881 §18.16.3, §10.4.1.
#[tokio::test]
async fn test_open_v41_want_cancel_returns_none_ext_cancelled() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_CANCEL,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::NoneExt as u32);
    assert_eq!(why, Some(WhyNoDelegation4::Cancelled as u32));
}

/// A v4.1 OPEN that only sets a "signal when available" flag (no delegation
/// level) still gets `OPEN_DELEGATE_NONE_EXT` with `WND4_NOT_SUPP_FTYPE` — the
/// server has nothing to signal because it grants no delegations.
/// Origin: locks down the signal-flag no-delegation response shape.
/// RFC: RFC 8881 §18.16.3, §10.4.1.
#[tokio::test]
async fn test_open_v41_signal_only_returns_none_ext_not_supp() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_SIGNAL_DELEG_WHEN_RESRC_AVAIL,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::NoneExt as u32);
    assert_eq!(why, Some(WhyNoDelegation4::NotSuppFtype as u32));
}

/// A v4.1 OPEN combining a delegation want with a signal flag is reported by the
/// want level: `WND4_NOT_SUPP_FTYPE` for a read-delegation want.
/// Origin: locks down the signal+want no-delegation response shape.
/// RFC: RFC 8881 §18.16.3, §10.4.1.
#[tokio::test]
async fn test_open_v41_want_with_signal_returns_none_ext_not_supp() {
    let fs = populated_fs(&["existing.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let open_op = encode_open_nocreate_with_access(
        "existing.txt",
        OPEN4_SHARE_ACCESS_READ
            | OPEN4_SHARE_ACCESS_WANT_READ_DELEG
            | OPEN4_SHARE_ACCESS_WANT_SIGNAL_DELEG_WHEN_RESRC_AVAIL,
        OPEN4_SHARE_DENY_NONE,
    );
    let (deleg_type, why) = open_delegation(port, &open_op).await;
    assert_eq!(deleg_type, OpenDelegationType4::NoneExt as u32);
    assert_eq!(why, Some(WhyNoDelegation4::NotSuppFtype as u32));
}

/// A v4.0 OPEN must not carry want-delegation bits — sessions/delegation wants
/// do not exist at minor version 0, so such a share_access is `NFS4ERR_INVAL`.
/// Origin: RFC 7530 has no want-delegation bits in share_access.
/// RFC: RFC 7530 §16.16; RFC 8881 §18.16.3.
#[tokio::test]
async fn test_open_v40_want_delegation_bits_return_inval() {
    let port = start_server().await;
    let mut stream = connect(port).await;

    let setclientid_op = encode_setclientid(&[4, 0, 0, 0, 0, 0, 0, 9], b"v40-share-validation");
    let compound = encode_compound_minor("v40-setclientid", 0, &[&setclientid_op]);
    let mut resp = send_rpc(&mut stream, 1, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    let (clientid, confirm_verifier) = parse_setclientid_res(&mut resp);

    let confirm_op = encode_setclientid_confirm(clientid, &confirm_verifier);
    let compound = encode_compound_minor("v40-confirm", 0, &[&confirm_op]);
    let mut resp = send_rpc(&mut stream, 2, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);

    let putrootfh_op = encode_putrootfh();
    let open_op = encode_open_create_with_clientid(
        "x.txt",
        clientid,
        OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_READ_DELEG,
        OPEN4_SHARE_DENY_NONE,
    );
    let compound = encode_compound_minor("v40-open", 0, &[&putrootfh_op, &open_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (_status, _, _) = parse_compound_header(&mut resp);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Inval as u32);
}

// ===== Regular-file data path enforces FileSystem::access =====

/// Drives `SEQUENCE → PUTROOTFH → LOOKUP(file) → <op>` (op using an
/// anonymous/default stateid) and returns the final op's status.
async fn anon_data_op_status(port: u16, file: &str, op: &[u8]) -> u32 {
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup(file);
    let compound = encode_compound("anon-data", &[&seq_op, &rootfh_op, &lookup_op, op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (_status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    let (opnum, _) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_LOOKUP);
    let (_opnum, op_status) = parse_op_header(&mut resp);
    op_status
}

/// READ with the anonymous stateid (no OPEN) is denied when the backend denies
/// read — the data path must enforce access, not just stateid validity.
/// Origin: data-path access enforcement for special stateids.
/// RFC: RFC 8881 §8.2.3, §18.22.3.
#[tokio::test]
async fn test_anonymous_read_denied_by_backend_access() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("secret.txt", 0o000).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let read_op = encode_read(0, 1024);
    assert_eq!(
        anon_data_op_status(port, "secret.txt", &read_op).await,
        NfsStat4::Access as u32
    );
}

/// WRITE with the anonymous stateid is denied when the backend denies write.
/// Origin: data-path access enforcement for special stateids.
/// RFC: RFC 8881 §8.2.3, §18.32.3.
#[tokio::test]
async fn test_anonymous_write_denied_by_backend_access() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("ro.txt", 0o444).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let write_op = encode_write(&Stateid4::default(), 0, b"intrusion");
    assert_eq!(
        anon_data_op_status(port, "ro.txt", &write_op).await,
        NfsStat4::Access as u32
    );
}

/// SETATTR(size) with the anonymous stateid is denied when the backend denies
/// write — a truncate is a data mutation.
/// Origin: data-path access enforcement for special stateids.
/// RFC: RFC 8881 §8.2.3, §18.30.3.
#[tokio::test]
async fn test_anonymous_truncate_denied_by_backend_access() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("ro.txt", 0o444).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let setattr_op = encode_setattr_size(&Stateid4::default(), 0);
    assert_eq!(
        anon_data_op_status(port, "ro.txt", &setattr_op).await,
        NfsStat4::Access as u32
    );
}

/// SETATTR(size) on a directory returns `NFS4ERR_ISDIR` even when the backend
/// would deny write — the ISDIR determination precedes the write-access gate.
/// Origin: error-precedence regression for the truncate access gate.
/// RFC: RFC 8881 §18.30.3.
#[tokio::test]
async fn test_truncate_on_directory_returns_isdir_not_access() {
    let fs = AccessPolicyFs::new(fs_with_subdir("adir").await, AccessPolicy::ReadOnly);
    let port = start_server_with_fs(fs).await;
    let setattr_op = encode_setattr_size(&Stateid4::default(), 0);
    assert_eq!(
        anon_data_op_status(port, "adir", &setattr_op).await,
        NfsStat4::Isdir as u32
    );
}

/// READ with the anonymous stateid still succeeds when the backend permits it —
/// the data-path gate does not over-restrict.
/// Origin: positive control for data-path access enforcement.
/// RFC: RFC 8881 §18.22.3.
#[tokio::test]
async fn test_anonymous_read_allowed_when_backend_permits() {
    let fs = AccessPolicyFs::new(
        memfs_with_mode("ok.txt", 0o644).await,
        AccessPolicy::OwnerMode,
    );
    let port = start_server_with_fs(fs).await;
    let read_op = encode_read(0, 1024);
    assert_eq!(
        anon_data_op_status(port, "ok.txt", &read_op).await,
        NfsStat4::Ok as u32
    );
}

/// CLOSE on a valid open stateid succeeds.
/// Origin: `pynfs/nfs4.0/servertests/st_close.py` (CODE `CLOSE1`).
/// RFC: RFC 8881 §18.2.3.
#[tokio::test]
async fn test_close_valid_stateid() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    // Open
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_create("close-test.txt");
    let getfh_op = encode_getfh();
    let compound = encode_compound("open", &[&seq_op, &rootfh_op, &open_op, &getfh_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = skip_open_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let fh = parse_getfh(&mut resp);

    // Close
    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putfh_op = encode_putfh(&fh);
    let close_op = encode_close(&stateid);
    let compound = encode_compound("close", &[&seq_op, &putfh_op, &close_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CLOSE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
}

/// CLOSE with a bogus stateid returns `NFS4ERR_BAD_STATEID`.
/// Origin: `pynfs/nfs4.0/servertests/st_close.py` (CODE `CLOSE4`).
/// RFC: RFC 8881 §18.2.3.
#[tokio::test]
async fn test_close_bad_stateid() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let bogus = Stateid4 {
        seqid: 999,
        other: [0xAA; 12],
    };
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let close_op = encode_close(&bogus);
    let compound = encode_compound("close-bad", &[&seq_op, &rootfh_op, &close_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CLOSE);
    assert_eq!(status, op_status);
    assert_eq!(op_status, NfsStat4::BadStateid as u32);
}

/// NFSv4.0 write OPEN_CONFIRM flow publishes one write-open and final close.
/// Origin: regression coverage for Linux NFSv4.0 fallback lifecycle callbacks.
/// RFC: RFC 7530 §16.18; RFC 8881 §18.2, §18.32.
#[tokio::test]
async fn test_v40_open_confirm_write_close_lifecycle_hooks() {
    let counts = Arc::new(OpenLifecycleCounts::default());
    let fs = OpenLifecycleFs {
        inner: MemFs::new(),
        counts: counts.clone(),
    };
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;

    let setclientid_op = encode_setclientid(&[4, 0, 0, 0, 0, 0, 0, 1], b"v40-lifecycle-client");
    let compound = encode_compound_minor("v40-setclientid", 0, &[&setclientid_op]);
    let mut resp = send_rpc(&mut stream, 1, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 1);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SETCLIENTID);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (clientid, confirm_verifier) = parse_setclientid_res(&mut resp);

    let confirm_client_op = encode_setclientid_confirm(clientid, &confirm_verifier);
    let compound = encode_compound_minor("v40-setclientid-confirm", 0, &[&confirm_client_op]);
    let mut resp = send_rpc(&mut stream, 2, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 1);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SETCLIENTID_CONFIRM);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let putrootfh_op = encode_putrootfh();
    let open_op = encode_open_create_with_clientid(
        "v40-lifecycle.txt",
        clientid,
        OPEN4_SHARE_ACCESS_BOTH,
        OPEN4_SHARE_DENY_NONE,
    );
    let getfh_op = encode_getfh();
    let compound = encode_compound_minor("v40-open", 0, &[&putrootfh_op, &open_op, &getfh_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTROOTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let open_stateid = skip_open_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let fh = parse_getfh(&mut resp);
    assert_eq!(counts.write_open_count.load(Ordering::Relaxed), 1);
    assert_eq!(counts.non_write_open_count.load(Ordering::Relaxed), 0);
    assert_eq!(counts.last_writer_close_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        counts.non_last_writer_close_count.load(Ordering::Relaxed),
        0
    );

    let putfh_op = encode_putfh(&fh);
    let open_confirm_op = encode_open_confirm_stateid(&open_stateid);
    let compound = encode_compound_minor("v40-open-confirm", 0, &[&putfh_op, &open_confirm_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_OPEN_CONFIRM);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let confirmed_stateid = parse_stateid(&mut resp);

    let putfh_op = encode_putfh(&fh);
    let write_op = encode_write(&confirmed_stateid, 0, b"confirmed-write");
    let compound = encode_compound_minor("v40-write", 0, &[&putfh_op, &write_op]);
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_WRITE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (written, committed) = parse_write_res(&mut resp);
    assert_eq!(written, b"confirmed-write".len() as u32);
    assert_eq!(committed, FILE_SYNC4);

    let putfh_op = encode_putfh(&fh);
    let close_op = encode_close(&confirmed_stateid);
    let compound = encode_compound_minor("v40-close", 0, &[&putfh_op, &close_op]);
    let mut resp = send_rpc(&mut stream, 6, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 2);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_PUTFH);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_CLOSE);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    assert_eq!(counts.write_open_count.load(Ordering::Relaxed), 1);
    assert_eq!(counts.non_write_open_count.load(Ordering::Relaxed), 0);
    assert_eq!(counts.last_writer_close_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        counts.non_last_writer_close_count.load(Ordering::Relaxed),
        0
    );
}

// ===== READ (pynfs RD) =====

/// READ from a file with data returns the correct bytes.
/// Origin: derived from `pynfs/nfs4.0/servertests/st_read.py` (CODE `RD1`).
/// RFC: RFC 8881 §18.22.3.
#[tokio::test]
async fn test_read_file_data() {
    let fs = fs_with_data("data.txt", b"hello world").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("data.txt");
    let read_op = encode_read(0, 1024);
    let compound = encode_compound("read-data", &[&seq_op, &rootfh_op, &lookup_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert_eq!(data.as_ref(), b"hello world");
}

/// READ from an empty file returns EOF with empty data.
/// Origin: RFC- and implementation-driven empty-file check.
/// RFC: RFC 8881 §18.22.3.
#[tokio::test]
async fn test_read_empty_file() {
    let fs = populated_fs(&["empty.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("empty.txt");
    let read_op = encode_read(0, 1024);
    let compound = encode_compound("read-empty", &[&seq_op, &rootfh_op, &lookup_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert!(data.is_empty());
}

/// READ with an offset beyond EOF returns EOF with empty data.
/// Origin: `pynfs/nfs4.0/servertests/st_read.py` (CODE `RD5`).
/// RFC: RFC 8881 §18.22.3.
#[tokio::test]
async fn test_read_beyond_eof() {
    let fs = fs_with_data("small.txt", b"hi").await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("small.txt");
    let read_op = encode_read(1000, 1024);
    let compound = encode_compound("read-beyond", &[&seq_op, &rootfh_op, &lookup_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert!(data.is_empty());
}

/// READ on a directory returns `NFS4ERR_ISDIR`.
/// Origin: adapted from `pynfs/nfs4.0/servertests/st_read.py` (CODE `RD7d`).
/// RFC: RFC 8881 §18.22.3.
#[tokio::test]
async fn test_read_directory_returns_error() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let read_op = encode_read(0, 1024);
    let compound = encode_compound("read-dir", &[&seq_op, &rootfh_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(status, op_status);
    assert_eq!(op_status, NfsStat4::Isdir as u32);
}

// ===== WRITE (pynfs WRT) =====

/// WRITE to a file with an open stateid succeeds and the data can be read back.
/// Origin: derived from `pynfs/nfs4.0/servertests/st_write.py` (CODE `WRT3`).
/// RFC: RFC 8881 §18.32.3.
#[tokio::test]
async fn test_write_and_read_back() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    // Open + Write
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_create("write-test.txt");
    let getfh_op = encode_getfh();
    let compound = encode_compound("open-write", &[&seq_op, &rootfh_op, &open_op, &getfh_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = skip_open_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let file_fh = parse_getfh(&mut resp);

    // Write
    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putfh_op = encode_putfh(&file_fh);
    let write_op = encode_write(&stateid, 0, b"test data 12345");
    let compound = encode_compound("write", &[&seq_op, &putfh_op, &write_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_WRITE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (count, _committed) = parse_write_res(&mut resp);
    assert_eq!(count, 15);

    // Read back
    let seq_op = encode_sequence(&sessionid, 3, 0);
    let putfh_op = encode_putfh(&file_fh);
    let read_op = encode_read(0, 1024);
    let compound = encode_compound("readback", &[&seq_op, &putfh_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert!(eof);
    assert_eq!(data.as_ref(), b"test data 12345");
}

/// WRITE beyond EOF preserves a hole before the written bytes.
/// Origin: derived from `pynfs/nfs4.0/servertests/st_write.py` (CODE `WRT1b`).
/// RFC: RFC 8881 §18.32.3.
#[tokio::test]
async fn test_write_at_offset() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    // Create & open
    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let open_op = encode_open_create("offset.txt");
    let getfh_op = encode_getfh();
    let compound = encode_compound("open", &[&seq_op, &rootfh_op, &open_op, &getfh_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let stateid = skip_open_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let file_fh = parse_getfh(&mut resp);

    // Write beyond EOF.
    let seq_op = encode_sequence(&sessionid, 2, 0);
    let putfh_op = encode_putfh(&file_fh);
    let write_op = encode_write(&stateid, 30, b"write data");
    let compound = encode_compound("write-hole", &[&seq_op, &putfh_op, &write_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let read_op = encode_read(25, 20);
    let compound = encode_compound("read-hole", &[&seq_op, &putfh_op, &read_op]);
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _eof = bool::decode(&mut resp).unwrap();
    let data = decode_opaque(&mut resp).unwrap();
    assert_eq!(data.as_ref(), b"\0\0\0\0\0write data");
}

// ===== REMOVE (pynfs RM) =====

/// REMOVE of an existing file succeeds.
/// Origin: `pynfs/nfs4.0/servertests/st_remove.py` (CODE `RM1r`).
/// RFC: RFC 8881 §18.25.3.
#[tokio::test]
async fn test_remove_existing_file() {
    let fs = populated_fs(&["doomed.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let remove_op = encode_remove("doomed.txt");
    let compound = encode_compound("remove", &[&seq_op, &rootfh_op, &remove_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    skip_change_info(&mut resp);

    // Verify it's gone
    let seq_op = encode_sequence(&sessionid, 2, 0);
    let lookup_op = encode_lookup("doomed.txt");
    let compound = encode_compound("verify-gone", &[&seq_op, &rootfh_op, &lookup_op]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
}

/// REMOVE of a non-existent name returns `NFS4ERR_NOENT`.
/// Origin: `pynfs/nfs4.0/servertests/st_remove.py` (CODE `RM6`).
/// RFC: RFC 8881 §18.25.3.
#[tokio::test]
async fn test_remove_nonexistent() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let remove_op = encode_remove("ghost.txt");
    let compound = encode_compound("rm-noent", &[&seq_op, &rootfh_op, &remove_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
}

/// REMOVE without a current filehandle returns `NFS4ERR_NOFILEHANDLE`.
/// Origin: `pynfs/nfs4.0/servertests/st_remove.py` (CODE `RM3`).
/// RFC: RFC 8881 §18.25.3.
#[tokio::test]
async fn test_remove_no_fh() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let remove_op = encode_remove("ghost.txt");
    let compound = encode_compound("rm-nofh", &[&seq_op, &remove_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Nofilehandle as u32);
    assert_eq!(num_results, 2);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Nofilehandle as u32);
}

/// REMOVE with a zero-length target returns `NFS4ERR_INVAL`.
/// Origin: `pynfs/nfs4.0/servertests/st_remove.py` (CODE `RM4`).
/// RFC: RFC 8881 §18.25.3.
#[tokio::test]
async fn test_remove_zero_length_target() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let remove_op = encode_remove("");
    let compound = encode_compound("rm-empty", &[&seq_op, &rootfh_op, &remove_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Inval as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Inval as u32);
}

/// REMOVE of `.` or `..` returns `NFS4ERR_BADNAME`.
/// Origin: adapted from `pynfs/nfs4.0/servertests/st_remove.py` (CODE `RM7`) to our stricter RFC-targeted expectation.
/// RFC: RFC 8881 §18.25.3.
#[tokio::test]
async fn test_remove_dot_names_badname() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    for (xid, seq, name) in [(3, 1, "."), (4, 2, "..")] {
        let seq_op = encode_sequence(&sessionid, seq, 0);
        let rootfh_op = encode_putrootfh();
        let remove_op = encode_remove(name);
        let compound = encode_compound("rm-dot", &[&seq_op, &rootfh_op, &remove_op]);
        let mut resp = send_rpc(&mut stream, xid, 1, &compound).await;
        parse_rpc_reply(&mut resp);

        let (status, _, num_results) = parse_compound_header(&mut resp);
        assert_eq!(status, NfsStat4::Badname as u32);
        assert_eq!(num_results, 3);
        let _ = parse_op_header(&mut resp);
        skip_sequence_res(&mut resp);
        let _ = parse_op_header(&mut resp);
        let (opnum, op_status) = parse_op_header(&mut resp);
        assert_eq!(opnum, OP_REMOVE);
        assert_eq!(op_status, NfsStat4::Badname as u32);
    }
}

/// Retrying REMOVE on the same cached slot replays the cached reply.
/// Origin: RFC 8881 replay-cache semantics; implementation-driven check.
/// RFC: RFC 8881 §2.10.6.1.3, §18.25.3.
#[tokio::test]
async fn test_remove_retry_replays_cached_reply() {
    let fs = populated_fs(&["remove-me.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence_with_cache(&sessionid, 1, 0, true);
    let rootfh_op = encode_putrootfh();
    let remove_op = encode_remove("remove-me.txt");
    let compound = encode_compound("remove-retry", &[&seq_op, &rootfh_op, &remove_op]);

    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let mut retry_resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut retry_resp);
    let (status, _, num_results) = parse_compound_header(&mut retry_resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 3);
    let _ = parse_op_header(&mut retry_resp);
    skip_sequence_res(&mut retry_resp);
    let _ = parse_op_header(&mut retry_resp);
    let (opnum, op_status) = parse_op_header(&mut retry_resp);
    assert_eq!(opnum, OP_REMOVE);
    assert_eq!(op_status, NfsStat4::Ok as u32);
}

// ===== RENAME (pynfs RNM) =====

/// RENAME of an existing file across directories succeeds.
/// Origin: `pynfs/nfs4.0/servertests/st_rename.py` (CODE `RNM1r`).
/// RFC: RFC 8881 §18.26.3.
#[tokio::test]
async fn test_rename_file() {
    let fs = MemFs::new();
    let ctx = RequestContext::anonymous();
    let dir1 = fs
        .create(
            &ctx,
            &1,
            "dir1",
            CreateRequest {
                kind: CreateKind::Directory,
                attrs: SetAttrs::default(),
            },
        )
        .await
        .unwrap()
        .handle;
    let _dir2 = fs
        .create(
            &ctx,
            &1,
            "dir2",
            CreateRequest {
                kind: CreateKind::Directory,
                attrs: SetAttrs::default(),
            },
        )
        .await
        .unwrap()
        .handle;
    let _ = fs
        .create(
            &ctx,
            &dir1,
            "old-name.txt",
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
    let lookup_dir1 = encode_lookup("dir1");
    let savefh_op = encode_savefh();
    let rootfh_op2 = encode_putrootfh();
    let lookup_dir2 = encode_lookup("dir2");
    let rename_op = encode_rename("old-name.txt", "new-name.txt");
    let compound = encode_compound(
        "rename",
        &[
            &seq_op,
            &rootfh_op,
            &lookup_dir1,
            &savefh_op,
            &rootfh_op2,
            &lookup_dir2,
            &rename_op,
        ],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp); // PUTROOTFH
    let _ = parse_op_header(&mut resp); // LOOKUP dir1
    let _ = parse_op_header(&mut resp); // SAVEFH
    let _ = parse_op_header(&mut resp); // PUTROOTFH
    let _ = parse_op_header(&mut resp); // LOOKUP dir2
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_RENAME);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    // Verify old name is gone, new name exists
    let seq_op = encode_sequence(&sessionid, 2, 0);
    let lookup_dir1 = encode_lookup("dir1");
    let lookup_old = encode_lookup("old-name.txt");
    let compound = encode_compound(
        "check-old",
        &[&seq_op, &rootfh_op, &lookup_dir1, &lookup_old],
    );
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let lookup_dir2 = encode_lookup("dir2");
    let lookup_new = encode_lookup("new-name.txt");
    let compound = encode_compound(
        "check-new",
        &[&seq_op, &rootfh_op, &lookup_dir2, &lookup_new],
    );
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
}

/// RENAME of a non-existent source returns `NFS4ERR_NOENT`.
/// Origin: `pynfs/nfs4.0/servertests/st_rename.py` (CODE `RNM5`).
/// RFC: RFC 8881 §18.26.3.
#[tokio::test]
async fn test_rename_nonexistent_source() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let savefh_op = encode_savefh();
    let rename_op = encode_rename("no-such.txt", "target.txt");
    let compound = encode_compound(
        "rename-noent",
        &[&seq_op, &rootfh_op, &savefh_op, &rename_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Noent as u32);
}

/// RENAME over a non-empty target directory leaves both names unchanged.
/// Origin: RFC 8881 §18.26.3 target replacement must fail atomically for non-empty directories.
/// RFC: RFC 8881 §18.26.3.
#[tokio::test]
async fn test_rename_over_nonempty_directory_is_atomic() {
    let fs = MemFs::new();
    let ctx = RequestContext::anonymous();
    let _ = fs
        .create(
            &ctx,
            &1,
            "source.txt",
            CreateRequest {
                kind: CreateKind::File,
                attrs: SetAttrs::default(),
            },
        )
        .await
        .unwrap();
    let target_dir = fs
        .create(
            &ctx,
            &1,
            "target",
            CreateRequest {
                kind: CreateKind::Directory,
                attrs: SetAttrs::default(),
            },
        )
        .await
        .unwrap();
    let _ = fs
        .create(
            &ctx,
            &target_dir.handle,
            "nested.txt",
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
    let savefh_op = encode_savefh();
    let rename_op = encode_rename("source.txt", "target");
    let compound = encode_compound(
        "rename-notempty",
        &[&seq_op, &rootfh_op, &savefh_op, &rename_op],
    );
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Isdir as u32);

    let seq_op = encode_sequence(&sessionid, 2, 0);
    let lookup_source = encode_lookup("source.txt");
    let compound = encode_compound("check-source", &[&seq_op, &rootfh_op, &lookup_source]);
    let mut resp = send_rpc(&mut stream, 4, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);

    let seq_op = encode_sequence(&sessionid, 3, 0);
    let lookup_target = encode_lookup("target");
    let lookup_nested = encode_lookup("nested.txt");
    let compound = encode_compound(
        "check-target",
        &[&seq_op, &rootfh_op, &lookup_target, &lookup_nested],
    );
    let mut resp = send_rpc(&mut stream, 5, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
}
