use super::*;

const OWNER_A: &[u8] = b"open-owner-a";
const OWNER_B: &[u8] = b"open-owner-b";

/// One SEQUENCE slot on one session; each call takes the next sequence id.
struct Conn {
    stream: tokio::net::TcpStream,
    sessionid: Sessionid4,
    seq: u32,
    xid: u32,
}

impl Conn {
    async fn new(port: u16) -> Self {
        let mut stream = connect(port).await;
        let sessionid = setup_session(&mut stream).await;
        Conn {
            stream,
            sessionid,
            seq: 0,
            xid: 10,
        }
    }

    async fn call(&mut self, tag: &str, ops: &[&[u8]]) -> Bytes {
        self.seq += 1;
        self.xid += 1;
        let seq_op = encode_sequence(&self.sessionid, self.seq, 0);
        let mut all: Vec<&[u8]> = vec![&seq_op];
        all.extend_from_slice(ops);
        let compound = encode_compound(tag, &all);
        let mut resp = send_rpc(&mut self.stream, self.xid, 1, &compound).await;
        parse_rpc_reply(&mut resp);
        resp
    }

    /// OPEN `name` as `owner`; returns the stateid, or the failing status.
    async fn open(
        &mut self,
        name: &str,
        owner: &[u8],
        access: u32,
        deny: u32,
    ) -> Result<Stateid4, u32> {
        let rootfh = encode_putrootfh();
        let open = encode_open_nocreate_as(name, owner, access, deny);
        let mut resp = self.call("open", &[&rootfh, &open]).await;
        let (status, _, _) = parse_compound_header(&mut resp);
        if status != NfsStat4::Ok as u32 {
            return Err(status);
        }
        let _ = parse_op_header(&mut resp);
        skip_sequence_res(&mut resp);
        let _ = parse_op_header(&mut resp);
        let _ = parse_op_header(&mut resp);
        Ok(skip_open_res(&mut resp))
    }

    /// WRITE through `stateid` on `name`; returns the compound status.
    async fn write(&mut self, name: &str, stateid: &Stateid4, data: &[u8]) -> u32 {
        let rootfh = encode_putrootfh();
        let lookup = encode_lookup(name);
        let write = encode_write(stateid, 0, data);
        let mut resp = self.call("write", &[&rootfh, &lookup, &write]).await;
        parse_compound_header(&mut resp).0
    }

    async fn read(&mut self, name: &str, stateid: &Stateid4) -> u32 {
        let rootfh = encode_putrootfh();
        let lookup = encode_lookup(name);
        let read = encode_read_stateid(stateid, 0, 64);
        let mut resp = self.call("read", &[&rootfh, &lookup, &read]).await;
        parse_compound_header(&mut resp).0
    }
}

/// The client's "current" form of a stateid: seqid 0 names the latest
/// version (RFC 8881 §8.2.2). The Linux client sends I/O this way.
fn current(stateid: &Stateid4) -> Stateid4 {
    Stateid4 {
        seqid: 0,
        other: stateid.other,
    }
}

/// A second OPEN by the same open-owner upgrades the first: same `other`,
/// bumped seqid, union of access. This is the S17 livelock: a writer process
/// and a `tail -f` reader of the same user share one Linux open-owner. Before
/// the fix the READ open minted a fresh read-only stateid, the client sent its
/// WRITEs on it, got `NFS4ERR_OPENMODE`, re-opened, and looped forever.
/// Origin: Kasm spike S17 (`tail -f` on the KasmVNC log).
/// RFC: RFC 8881 §18.16.3, §9.1.2.
#[tokio::test]
async fn test_same_owner_write_then_read_open_keeps_write_access() {
    let fs = populated_fs(&["vnc.log"]).await;
    let port = start_server_with_fs(fs).await;
    let mut conn = Conn::new(port).await;

    let writer = conn
        .open(
            "vnc.log",
            OWNER_A,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    let reader = conn
        .open(
            "vnc.log",
            OWNER_A,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();

    assert_eq!(reader.other, writer.other, "same owner, same open state");
    assert_eq!(reader.seqid, writer.seqid + 1, "upgrade bumps the seqid");

    assert_eq!(
        conn.write("vnc.log", &current(&reader), b"line\n").await,
        NfsStat4::Ok as u32,
        "WRITE through the upgraded stateid must keep write access",
    );
    assert_eq!(
        conn.read("vnc.log", &current(&reader)).await,
        NfsStat4::Ok as u32
    );
    // The older seqid is now stale, as for any stateid that moved on.
    assert_eq!(
        conn.write("vnc.log", &writer, b"line\n").await,
        NfsStat4::OldStateid as u32
    );
}

/// The reverse order: READ first, then WRITE, by one owner.
/// RFC: RFC 8881 §18.16.3.
#[tokio::test]
async fn test_same_owner_read_then_write_open_upgrades() {
    let fs = populated_fs(&["upgrade.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut conn = Conn::new(port).await;

    let first = conn
        .open(
            "upgrade.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    assert_eq!(
        conn.write("upgrade.txt", &current(&first), b"x").await,
        NfsStat4::Openmode as u32,
        "read-only so far",
    );
    let second = conn
        .open(
            "upgrade.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    assert_eq!(second.other, first.other);
    assert_eq!(
        conn.write("upgrade.txt", &current(&second), b"x").await,
        NfsStat4::Ok as u32
    );
}

/// Different open-owners still get independent opens: the reader's stateid
/// stays read-only even while another owner holds the file open for write.
/// RFC: RFC 8881 §9.1.2, §18.32.3.
#[tokio::test]
async fn test_different_owners_keep_separate_open_state() {
    let fs = populated_fs(&["shared.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut conn = Conn::new(port).await;

    let writer = conn
        .open(
            "shared.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    let reader = conn
        .open(
            "shared.txt",
            OWNER_B,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();

    assert_ne!(writer.other, reader.other);
    assert_eq!(
        conn.write("shared.txt", &current(&writer), b"w").await,
        NfsStat4::Ok as u32
    );
    assert_eq!(
        conn.write("shared.txt", &current(&reader), b"r").await,
        NfsStat4::Openmode as u32
    );
}

/// An owner's own share reservation never conflicts with its upgrade, but
/// still binds every other owner, and the union of deny bits is enforced.
/// RFC: RFC 8881 §9.7, §18.16.3.
#[tokio::test]
async fn test_share_deny_ignores_own_open_but_binds_others() {
    let fs = populated_fs(&["deny.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut conn = Conn::new(port).await;

    let first = conn
        .open(
            "deny.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_WRITE,
        )
        .await
        .unwrap();
    let upgraded = conn
        .open(
            "deny.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .expect("an owner's own deny must not block its upgrade");
    assert_eq!(upgraded.other, first.other);

    assert_eq!(
        conn.open(
            "deny.txt",
            OWNER_B,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE
        )
        .await,
        Err(NfsStat4::ShareDenied as u32),
        "the union still carries DENY_WRITE",
    );
    assert!(
        conn.open(
            "deny.txt",
            OWNER_B,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE
        )
        .await
        .is_ok()
    );
}

/// CLOSE releases the whole combined open; a later OPEN by the same owner
/// starts a fresh open state.
/// RFC: RFC 8881 §18.2.3.
#[tokio::test]
async fn test_close_releases_combined_open() {
    let fs = populated_fs(&["close.txt"]).await;
    let port = start_server_with_fs(fs).await;
    let mut conn = Conn::new(port).await;

    let _ = conn
        .open(
            "close.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_WRITE,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    let combined = conn
        .open(
            "close.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();

    let rootfh = encode_putrootfh();
    let lookup = encode_lookup("close.txt");
    let close = encode_close(&combined);
    let mut resp = conn.call("close", &[&rootfh, &lookup, &close]).await;
    assert_eq!(parse_compound_header(&mut resp).0, NfsStat4::Ok as u32);

    assert_eq!(
        conn.write("close.txt", &current(&combined), b"x").await,
        NfsStat4::BadStateid as u32,
        "a closed open grants nothing",
    );
    let reopened = conn
        .open(
            "close.txt",
            OWNER_A,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
        )
        .await
        .unwrap();
    assert_ne!(reopened.other, combined.other);
}
