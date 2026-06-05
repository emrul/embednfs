use super::*;
use embednfs::AuthPolicy;

/// Builds a well-formed AUTH_SYS credential.
fn auth_sys_cred() -> OpaqueAuth {
    OpaqueAuth {
        flavor: AuthFlavor::Sys as u32,
        body: encode_auth_sys_body("auth-policy-test", &[]).into(),
    }
}

/// The default server advertises AUTH_SYS and AUTH_NONE through SECINFO_NO_NAME,
/// preserving the historical flavor set and order.
/// Origin: backward-compatibility check for the auth-flavor policy.
/// RFC: RFC 8881 §18.45.3; RFC 5531 §8.2.
#[tokio::test]
async fn test_default_server_advertises_sys_and_none() {
    let port = start_server().await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let secinfo_op = encode_secinfo_no_name(0);
    let compound = encode_compound("default-secinfo", &[&seq_op, &rootfh_op, &secinfo_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);

    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SECINFO_NO_NAME);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(
        parse_secinfo_entries(&mut resp),
        vec![AuthFlavor::Sys as u32, AuthFlavor::None as u32]
    );
}

/// A sys-only server advertises exactly AUTH_SYS through SECINFO. Driven over
/// minor version 0 with AUTH_SYS so it needs no AUTH_NONE session setup.
/// Origin: SECINFO must reflect the configured auth policy.
/// RFC: RFC 8881 §18.29.3; RFC 5531 §8.2.
#[tokio::test]
async fn test_sys_only_server_advertises_only_sys() {
    let port = start_server_with_auth_policy(AuthPolicy::sys_only()).await;
    let mut stream = connect(port).await;

    let rootfh_op = encode_putrootfh();
    let secinfo_op = encode_secinfo("anything");
    let compound = encode_compound_minor("sysonly-secinfo", 0, &[&rootfh_op, &secinfo_op]);
    let mut resp = send_rpc_with_auth(
        &mut stream,
        1,
        1,
        &compound,
        &auth_sys_cred(),
        &OpaqueAuth::null(),
    )
    .await;

    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp); // PUTROOTFH
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_SECINFO);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    assert_eq!(
        parse_secinfo_entries(&mut resp),
        vec![AuthFlavor::Sys as u32]
    );
}

/// A sys-only server accepts the AUTH_NONE NULL procedure probe.
/// Origin: Linux NFSv4.1 mount probe observed by portal-sync Phase-5 fleet replication test.
/// RFC: RFC 5531 §11.1; RFC 8881 §16.1.
#[tokio::test]
async fn test_sys_only_accepts_auth_none_null_procedure() {
    let port = start_server_with_auth_policy(AuthPolicy::sys_only()).await;
    let mut stream = connect(port).await;

    let mut resp = send_rpc_with_auth(
        &mut stream,
        9,
        0,
        &[],
        &OpaqueAuth::null(),
        &OpaqueAuth::null(),
    )
    .await;

    let (xid, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(xid, 9);
    assert_eq!(accept_stat, 0);
    assert!(resp.is_empty());
}

/// A sys-only server rejects an AUTH_NONE operation at the RPC layer with
/// `AUTH_TOOWEAK`. The reply is MSG_DENIED, so the COMPOUND is never decoded and
/// the backend is never reached.
/// Origin: protocol-boundary enforcement of the auth policy.
/// RFC: RFC 5531 §9.2 (AUTH_TOOWEAK).
#[tokio::test]
async fn test_sys_only_rejects_auth_none_operation() {
    let port = start_server_with_auth_policy(AuthPolicy::sys_only()).await;
    let mut stream = connect(port).await;

    // A real operation (PUTROOTFH + GETATTR) under AUTH_NONE. If it were
    // processed it would produce an ACCEPTED reply with attributes; instead the
    // RPC layer denies it before any op runs.
    let rootfh_op = encode_putrootfh();
    let getattr_op = encode_getattr(&[FATTR4_TYPE]);
    let compound = encode_compound_minor("none-rejected", 0, &[&rootfh_op, &getattr_op]);
    let mut resp = send_rpc_with_auth(
        &mut stream,
        7,
        1,
        &compound,
        &OpaqueAuth::null(),
        &OpaqueAuth::null(),
    )
    .await;

    let (xid, auth_stat) = parse_rpc_auth_error(&mut resp);
    assert_eq!(xid, 7);
    assert_eq!(auth_stat, AuthStat::TooWeak as u32);
}

/// A sys-only server accepts a well-formed AUTH_SYS operation.
/// Origin: positive control for the sys-only policy.
/// RFC: RFC 5531 §8.2; RFC 8881 §18.7.3.
#[tokio::test]
async fn test_sys_only_accepts_valid_auth_sys() {
    let port = start_server_with_auth_policy(AuthPolicy::sys_only()).await;
    let mut stream = connect(port).await;

    let rootfh_op = encode_putrootfh();
    let getattr_op = encode_getattr(&[FATTR4_TYPE]);
    let compound = encode_compound_minor("sys-accepted", 0, &[&rootfh_op, &getattr_op]);
    let mut resp = send_rpc_with_auth(
        &mut stream,
        1,
        1,
        &compound,
        &auth_sys_cred(),
        &OpaqueAuth::null(),
    )
    .await;

    let (_, accept_stat) = parse_rpc_reply_fields(&mut resp);
    assert_eq!(accept_stat, 0);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp); // PUTROOTFH
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
}

/// A sys-only server still rejects a malformed AUTH_SYS credential with
/// `AUTH_BADCRED`, not `AUTH_TOOWEAK` — the flavor is allowed, the body is not.
/// Origin: malformed-credential handling must be unchanged by the policy.
/// RFC: RFC 5531 §9.2 (AUTH_BADCRED).
#[tokio::test]
async fn test_sys_only_malformed_auth_sys_returns_badcred() {
    let port = start_server_with_auth_policy(AuthPolicy::sys_only()).await;
    let mut stream = connect(port).await;

    let overlong = encode_auth_sys_body(&"x".repeat(300), &(0..17u32).collect::<Vec<_>>());
    let cred = OpaqueAuth {
        flavor: AuthFlavor::Sys as u32,
        body: overlong.into(),
    };
    let compound = encode_compound_minor("sysonly-badcred", 0, &[&encode_putrootfh()]);
    let mut resp =
        send_rpc_with_auth(&mut stream, 2, 1, &compound, &cred, &OpaqueAuth::null()).await;

    let (xid, auth_stat) = parse_rpc_auth_error(&mut resp);
    assert_eq!(xid, 2);
    assert_eq!(auth_stat, AuthStat::BadCred as u32);
}
