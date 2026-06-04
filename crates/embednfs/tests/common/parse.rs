use bytes::Bytes;

use embednfs_proto::xdr::*;
use embednfs_proto::*;

pub fn parse_lock_res(resp: &mut Bytes) -> Stateid4 {
    Stateid4::decode(resp).unwrap()
}

pub fn parse_locku_res(resp: &mut Bytes) -> Stateid4 {
    Stateid4::decode(resp).unwrap()
}

pub fn parse_rpc_reply_fields(resp: &mut Bytes) -> (u32, u32) {
    let xid = u32::decode(resp).unwrap();
    let msg_type = u32::decode(resp).unwrap();
    assert_eq!(msg_type, 1, "expected RPC reply");
    let reply_stat = u32::decode(resp).unwrap();
    assert_eq!(reply_stat, 0, "expected accepted reply");
    let _verf = OpaqueAuth::decode(resp).unwrap();
    let accept_stat = u32::decode(resp).unwrap();
    (xid, accept_stat)
}

pub fn parse_rpc_reply(resp: &mut Bytes) {
    let _ = parse_rpc_reply_fields(resp);
}

pub fn parse_rpc_auth_error(resp: &mut Bytes) -> (u32, u32) {
    let xid = u32::decode(resp).unwrap();
    let msg_type = u32::decode(resp).unwrap();
    assert_eq!(msg_type, 1, "expected RPC reply");
    let reply_stat = u32::decode(resp).unwrap();
    assert_eq!(reply_stat, 1, "expected rejected reply");
    let reject_stat = u32::decode(resp).unwrap();
    assert_eq!(reject_stat, 1, "expected AUTH_ERROR");
    let auth_stat = u32::decode(resp).unwrap();
    (xid, auth_stat)
}

pub fn parse_compound_header(resp: &mut Bytes) -> (u32, String, u32) {
    let status = u32::decode(resp).unwrap();
    let tag = String::decode(resp).unwrap();
    let num_results = u32::decode(resp).unwrap();
    (status, tag, num_results)
}

pub fn parse_op_header(resp: &mut Bytes) -> (u32, u32) {
    let opnum = u32::decode(resp).unwrap();
    let status = u32::decode(resp).unwrap();
    (opnum, status)
}

pub type ReaddirEntry = (u64, String, Fattr4);

pub fn parse_readdir_body(resp: &mut Bytes) -> (usize, [u8; 8], Vec<ReaddirEntry>, bool) {
    let body_len_before = resp.len();
    let cookieverf_data = decode_fixed_opaque(resp, 8).unwrap();
    let mut cookieverf = [0u8; 8];
    cookieverf.copy_from_slice(&cookieverf_data);

    let mut entries = Vec::new();
    while bool::decode(resp).unwrap() {
        let cookie = u64::decode(resp).unwrap();
        let name = String::decode(resp).unwrap();
        let attrs = Fattr4::decode(resp).unwrap();
        entries.push((cookie, name, attrs));
    }
    let eof = bool::decode(resp).unwrap();

    (body_len_before - resp.len(), cookieverf, entries, eof)
}

pub fn parse_stateid(resp: &mut Bytes) -> Stateid4 {
    Stateid4::decode(resp).unwrap()
}

pub fn skip_change_info(resp: &mut Bytes) {
    let _ = bool::decode(resp).unwrap();
    let _ = u64::decode(resp).unwrap();
    let _ = u64::decode(resp).unwrap();
}

pub fn parse_change_info(resp: &mut Bytes) -> (bool, u64, u64) {
    (
        bool::decode(resp).unwrap(),
        u64::decode(resp).unwrap(),
        u64::decode(resp).unwrap(),
    )
}

pub fn skip_bitmap(resp: &mut Bytes) {
    let _ = Bitmap4::decode(resp).unwrap();
}

pub fn skip_open_res(resp: &mut Bytes) -> Stateid4 {
    let stateid = parse_stateid(resp);
    skip_change_info(resp);
    let _ = u32::decode(resp).unwrap();
    skip_bitmap(resp);
    let _ = read_open_delegation(resp);
    stateid
}

/// Consumes an `open_delegation4` union and returns its type plus, for an
/// `OPEN_DELEGATE_NONE_EXT` reply, the `why_no_delegation4` reason code. The
/// full union is read so a following op in the same compound parses correctly.
fn read_open_delegation(resp: &mut Bytes) -> (u32, Option<u32>) {
    let deleg_type = u32::decode(resp).unwrap();
    let why = if deleg_type == OpenDelegationType4::NoneExt as u32 {
        let why = u32::decode(resp).unwrap();
        // The Contention and Resource reasons carry a trailing bool; the
        // generic `Other` reason does not.
        if why == WhyNoDelegation4::Contention as u32
            || why == WhyNoDelegation4::ResourceNotAvail as u32
        {
            let _ = bool::decode(resp).unwrap();
        }
        Some(why)
    } else if deleg_type == OpenDelegationType4::Read as u32 {
        let _ = parse_stateid(resp);
        let _ = bool::decode(resp).unwrap(); // recall
        skip_nfsace4(resp);
        None
    } else if deleg_type == OpenDelegationType4::Write as u32 {
        let _ = parse_stateid(resp);
        let _ = bool::decode(resp).unwrap(); // recall
        skip_nfs_space_limit4(resp);
        skip_nfsace4(resp);
        None
    } else {
        None
    };
    (deleg_type, why)
}

/// Skips an `nfsace4` (type, flag, mask, who).
fn skip_nfsace4(resp: &mut Bytes) {
    let _ = u32::decode(resp).unwrap(); // acetype
    let _ = u32::decode(resp).unwrap(); // aceflag
    let _ = u32::decode(resp).unwrap(); // acemask
    let _ = decode_opaque(resp).unwrap(); // who
}

/// Skips an `nfs_space_limit4` union (by-blocks or by-size).
fn skip_nfs_space_limit4(resp: &mut Bytes) {
    let limitby = u32::decode(resp).unwrap();
    if limitby == 1 {
        // NFS_LIMIT_SIZE: filesize (u64)
        let _ = u64::decode(resp).unwrap();
    } else {
        // NFS_LIMIT_BLOCKS: num_blocks (u32) + bytes_per_block (u32)
        let _ = u32::decode(resp).unwrap();
        let _ = u32::decode(resp).unwrap();
    }
}

/// Parses an OPEN result and returns its delegation type plus, for an
/// `OPEN_DELEGATE_NONE_EXT` reply, the `why_no_delegation4` reason code.
pub fn parse_open_res_delegation(resp: &mut Bytes) -> (u32, Option<u32>) {
    let _ = parse_stateid(resp);
    skip_change_info(resp);
    let _ = u32::decode(resp).unwrap(); // rflags
    skip_bitmap(resp);
    read_open_delegation(resp)
}

pub fn parse_open_res(resp: &mut Bytes) -> (Stateid4, (bool, u64, u64)) {
    let stateid = parse_stateid(resp);
    let cinfo = parse_change_info(resp);
    let _ = u32::decode(resp).unwrap();
    skip_bitmap(resp);
    let _ = u32::decode(resp).unwrap();
    (stateid, cinfo)
}

pub fn parse_open_downgrade_res(resp: &mut Bytes) -> Stateid4 {
    Stateid4::decode(resp).unwrap()
}

pub fn parse_getfh(resp: &mut Bytes) -> Vec<u8> {
    decode_opaque(resp).unwrap().to_vec()
}

pub fn parse_test_stateid_results(resp: &mut Bytes) -> Vec<u32> {
    let count = u32::decode(resp).unwrap() as usize;
    (0..count).map(|_| u32::decode(resp).unwrap()).collect()
}

pub fn skip_exchange_id_res(resp: &mut Bytes) -> (u64, u32) {
    let clientid = u64::decode(resp).unwrap();
    let sequenceid = u32::decode(resp).unwrap();
    let _flags = u32::decode(resp).unwrap();
    let _state_protect = u32::decode(resp).unwrap();
    let _server_minor_id = u64::decode(resp).unwrap();
    let _server_major_id = Vec::<u8>::decode(resp).unwrap();
    let _server_scope = Vec::<u8>::decode(resp).unwrap();
    let _impl_count = u32::decode(resp).unwrap();
    (clientid, sequenceid)
}

pub struct ExchangeIdFields {
    pub clientid: u64,
    pub sequenceid: u32,
    pub flags: u32,
    pub server_owner_minor_id: u64,
    pub server_owner_major_id: Vec<u8>,
    pub server_scope: Vec<u8>,
}

pub fn parse_exchange_id_res(resp: &mut Bytes) -> (u64, u32, u32) {
    let fields = parse_exchange_id_res_full(resp);
    (fields.clientid, fields.sequenceid, fields.flags)
}

pub fn parse_exchange_id_res_full(resp: &mut Bytes) -> ExchangeIdFields {
    let clientid = u64::decode(resp).unwrap();
    let sequenceid = u32::decode(resp).unwrap();
    let flags = u32::decode(resp).unwrap();
    let _state_protect = u32::decode(resp).unwrap();
    let server_owner_minor_id = u64::decode(resp).unwrap();
    let server_owner_major_id = Vec::<u8>::decode(resp).unwrap();
    let server_scope = Vec::<u8>::decode(resp).unwrap();
    let impl_count = u32::decode(resp).unwrap();
    for _ in 0..impl_count {
        let _ = NfsImplId4::decode(resp).unwrap();
    }
    ExchangeIdFields {
        clientid,
        sequenceid,
        flags,
        server_owner_minor_id,
        server_owner_major_id,
        server_scope,
    }
}

pub fn skip_sequence_res(resp: &mut Bytes) {
    let _sessionid = decode_fixed_opaque(resp, 16).unwrap();
    let _sequenceid = u32::decode(resp).unwrap();
    let _slotid = u32::decode(resp).unwrap();
    let _highest_slotid = u32::decode(resp).unwrap();
    let _target_highest_slotid = u32::decode(resp).unwrap();
    let _status_flags = u32::decode(resp).unwrap();
}

pub fn parse_setclientid_res(resp: &mut Bytes) -> (u64, [u8; 8]) {
    let clientid = u64::decode(resp).unwrap();
    let mut verifier = [0u8; 8];
    let data = decode_fixed_opaque(resp, 8).unwrap();
    verifier.copy_from_slice(&data);
    (clientid, verifier)
}

pub fn parse_create_session_res(resp: &mut Bytes) -> [u8; 16] {
    let (sessionid, _, _) = parse_create_session_res_full(resp);
    sessionid
}

pub fn parse_create_session_res_full(resp: &mut Bytes) -> ([u8; 16], u32, u32) {
    let session_data = decode_fixed_opaque(resp, 16).unwrap();
    let mut sessionid = [0u8; 16];
    sessionid.copy_from_slice(&session_data);
    let sequenceid = u32::decode(resp).unwrap();
    let flags = u32::decode(resp).unwrap();
    let _fore_attrs = ChannelAttrs4::decode(resp).unwrap();
    let _back_attrs = ChannelAttrs4::decode(resp).unwrap();
    (sessionid, sequenceid, flags)
}

pub fn parse_bind_conn_to_session_res(resp: &mut Bytes) -> ([u8; 16], u32, bool) {
    let session_data = decode_fixed_opaque(resp, 16).unwrap();
    let mut sessionid = [0u8; 16];
    sessionid.copy_from_slice(&session_data);
    let dir = u32::decode(resp).unwrap();
    let use_conn_in_rdma_mode = bool::decode(resp).unwrap();
    (sessionid, dir, use_conn_in_rdma_mode)
}

pub fn parse_write_res(resp: &mut Bytes) -> (u32, u32) {
    let count = u32::decode(resp).unwrap();
    let committed = u32::decode(resp).unwrap();
    let _ = decode_fixed_opaque(resp, 8).unwrap();
    (count, committed)
}

pub fn parse_read_res(resp: &mut Bytes) -> (bool, Bytes) {
    let eof = bool::decode(resp).unwrap();
    let data = decode_opaque(resp).unwrap();
    (eof, data)
}

pub fn parse_access_res(resp: &mut Bytes) -> (u32, u32) {
    let supported = u32::decode(resp).unwrap();
    let access = u32::decode(resp).unwrap();
    (supported, access)
}

pub fn skip_secinfo_entries(resp: &mut Bytes) -> u32 {
    let count = u32::decode(resp).unwrap();
    for _ in 0..count {
        let _ = u32::decode(resp).unwrap();
    }
    count
}
