mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CreateRequest, CreateResult, DirEntry, DirPage, FileSystem, FsError, FsId,
    FsResult, FsStats, ObjectType, ReadResult, RequestContext, SetAttrs, WriteResult,
    WriteStability,
};
use embednfs_proto::xdr::*;
use embednfs_proto::*;

use common::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Object {
    Root,
    TreeA,
    TreeB,
}

#[derive(Debug, Clone)]
struct ObjectScopedFs {
    inline_readdir_attrs: bool,
    statfs_calls: Arc<Mutex<Vec<Object>>>,
}

impl ObjectScopedFs {
    fn new(inline_readdir_attrs: bool) -> Self {
        Self {
            inline_readdir_attrs,
            statfs_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn statfs_calls(&self) -> Vec<Object> {
        self.statfs_calls.lock().unwrap().clone()
    }

    fn attrs(handle: Object) -> Attrs {
        let (fileid, fsid) = match handle {
            Object::Root => (
                1,
                FsId {
                    major: 10,
                    minor: 1,
                },
            ),
            Object::TreeA => (
                2,
                FsId {
                    major: 20,
                    minor: 1,
                },
            ),
            Object::TreeB => (
                3,
                FsId {
                    major: 30,
                    minor: 1,
                },
            ),
        };
        let mut attrs = Attrs::new(ObjectType::Directory, fileid);
        attrs.fsid = fsid;
        attrs
    }

    fn stats(handle: Object) -> FsStats {
        let base = match handle {
            Object::Root => 1_000,
            Object::TreeA => 2_000,
            Object::TreeB => 3_000,
        };
        FsStats {
            total_bytes: base,
            free_bytes: base / 2,
            avail_bytes: base / 2,
            total_files: base + 10,
            free_files: base + 5,
            avail_files: base + 5,
        }
    }
}

#[async_trait]
impl FileSystem for ObjectScopedFs {
    type Handle = Object;

    fn root(&self) -> Self::Handle {
        Object::Root
    }

    async fn statfs(&self, _ctx: &RequestContext, handle: &Self::Handle) -> FsResult<FsStats> {
        self.statfs_calls.lock().unwrap().push(*handle);
        Ok(Self::stats(*handle))
    }

    async fn getattr(&self, _ctx: &RequestContext, handle: &Self::Handle) -> FsResult<Attrs> {
        Ok(Self::attrs(*handle))
    }

    async fn access(
        &self,
        _ctx: &RequestContext,
        _handle: &Self::Handle,
        requested: AccessMask,
    ) -> FsResult<AccessMask> {
        Ok(requested)
    }

    async fn lookup(
        &self,
        _ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<Self::Handle> {
        match (parent, name) {
            (Object::Root, "tree-a") => Ok(Object::TreeA),
            (Object::Root, "tree-b") => Ok(Object::TreeB),
            _ => Err(FsError::NotFound),
        }
    }

    async fn parent(
        &self,
        _ctx: &RequestContext,
        dir: &Self::Handle,
    ) -> FsResult<Option<Self::Handle>> {
        match dir {
            Object::Root => Ok(None),
            Object::TreeA | Object::TreeB => Ok(Some(Object::Root)),
        }
    }

    async fn readdir(
        &self,
        _ctx: &RequestContext,
        dir: &Self::Handle,
        cookie: u64,
        _max_entries: u32,
        with_attrs: bool,
    ) -> FsResult<DirPage<Self::Handle>> {
        if *dir != Object::Root {
            return Ok(DirPage {
                entries: Vec::new(),
                eof: true,
            });
        }
        let children = [("tree-a", Object::TreeA, 3), ("tree-b", Object::TreeB, 4)];
        let entries = children
            .into_iter()
            .filter(|(_, _, entry_cookie)| *entry_cookie > cookie)
            .map(|(name, handle, entry_cookie)| DirEntry {
                name: name.to_string(),
                handle,
                cookie: entry_cookie,
                attrs: (with_attrs && self.inline_readdir_attrs).then(|| Self::attrs(handle)),
            })
            .collect();
        Ok(DirPage { entries, eof: true })
    }

    async fn read(
        &self,
        _ctx: &RequestContext,
        _handle: &Self::Handle,
        _offset: u64,
        _count: u32,
    ) -> FsResult<ReadResult> {
        Err(FsError::Unsupported)
    }

    async fn write(
        &self,
        _ctx: &RequestContext,
        _handle: &Self::Handle,
        _offset: u64,
        _data: Bytes,
        _requested: WriteStability,
    ) -> FsResult<WriteResult> {
        Err(FsError::Unsupported)
    }

    async fn create(
        &self,
        _ctx: &RequestContext,
        _parent: &Self::Handle,
        _name: &str,
        _req: CreateRequest,
    ) -> FsResult<CreateResult<Self::Handle>> {
        Err(FsError::Unsupported)
    }

    async fn remove(
        &self,
        _ctx: &RequestContext,
        _parent: &Self::Handle,
        _name: &str,
    ) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    async fn rename(
        &self,
        _ctx: &RequestContext,
        _from_dir: &Self::Handle,
        _from_name: &str,
        _to_dir: &Self::Handle,
        _to_name: &str,
    ) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    async fn setattr(
        &self,
        _ctx: &RequestContext,
        handle: &Self::Handle,
        _attrs: &SetAttrs,
    ) -> FsResult<Attrs> {
        Ok(Self::attrs(*handle))
    }
}

async fn getattr_fattr(fs: ObjectScopedFs, ops: &[Vec<u8>], tag: &str) -> Fattr4 {
    getattr_fattr_bits(fs, ops, tag, &[FATTR4_FSID, FATTR4_SPACE_TOTAL]).await
}

async fn getattr_fattr_bits(
    fs: ObjectScopedFs,
    ops: &[Vec<u8>],
    tag: &str,
    bits: &[u32],
) -> Fattr4 {
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let getattr_op = encode_getattr(bits);
    let mut compound_ops = vec![seq_op.as_slice()];
    compound_ops.extend(ops.iter().map(Vec::as_slice));
    compound_ops.push(getattr_op.as_slice());
    let compound = encode_compound(tag, &compound_ops);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    for _ in ops {
        let (_, op_status) = parse_op_header(&mut resp);
        assert_eq!(op_status, NfsStat4::Ok as u32);
    }
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_GETATTR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    Fattr4::decode(&mut resp).unwrap()
}

fn decode_fsid_and_space_total(fattr: &Fattr4) -> (FsId, u64) {
    assert!(fattr.attrmask.is_set(FATTR4_FSID));
    assert!(fattr.attrmask.is_set(FATTR4_SPACE_TOTAL));
    let mut vals = Bytes::from(fattr.attr_vals.clone());
    let fsid = Fsid4::decode(&mut vals).unwrap();
    let space_total = u64::decode(&mut vals).unwrap();
    (
        FsId {
            major: fsid.major,
            minor: fsid.minor,
        },
        space_total,
    )
}

async fn readdir_entries(fs: ObjectScopedFs, bits: &[u32]) -> Vec<ReaddirEntry> {
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let readdir_op = encode_readdir_custom(0, [0u8; 8], 4096, 8192, bits);
    let compound = encode_compound("object-readdir", &[&seq_op, &rootfh_op, &readdir_op]);
    let mut resp = send_rpc(&mut stream, 3, 1, &compound).await;
    parse_rpc_reply(&mut resp);
    let (status, _, _) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READDIR);
    assert_eq!(op_status, NfsStat4::Ok as u32);
    let (_, _, entries, eof) = parse_readdir_body(&mut resp);
    assert!(eof);
    entries
}

/// GETATTR reports the FSID and stats for the object being encoded.
/// Origin: design/object-scoped-fsid.md object-scoped GETATTR regression.
/// RFC: RFC 8881 §5.8.1.9, §18.7.3.
#[tokio::test]
async fn test_getattr_uses_object_scoped_fsid_and_stats() {
    let fs = ObjectScopedFs::new(true);
    let root = getattr_fattr(fs.clone(), &[encode_putrootfh()], "root-fsid").await;
    let tree_a = getattr_fattr(
        fs.clone(),
        &[encode_putrootfh(), encode_lookup("tree-a")],
        "tree-a-fsid",
    )
    .await;
    let tree_b = getattr_fattr(
        fs.clone(),
        &[encode_putrootfh(), encode_lookup("tree-b")],
        "tree-b-fsid",
    )
    .await;

    assert_eq!(
        decode_fsid_and_space_total(&root),
        (
            FsId {
                major: 10,
                minor: 1
            },
            1_000
        )
    );
    assert_eq!(
        decode_fsid_and_space_total(&tree_a),
        (
            FsId {
                major: 20,
                minor: 1
            },
            2_000
        )
    );
    assert_eq!(
        decode_fsid_and_space_total(&tree_b),
        (
            FsId {
                major: 30,
                minor: 1
            },
            3_000
        )
    );
    assert_eq!(
        fs.statfs_calls(),
        vec![Object::Root, Object::TreeA, Object::TreeB]
    );
}

/// READDIR encodes each entry using that entry's FSID with inline backend attrs.
/// Origin: design/object-scoped-fsid.md inline READDIR attrs regression.
/// RFC: RFC 8881 §5.8.1.9, §18.23.3.
#[tokio::test]
async fn test_readdir_uses_entry_fsid_with_inline_attrs() {
    let fs = ObjectScopedFs::new(true);
    let entries = readdir_entries(fs.clone(), &[FATTR4_FSID]).await;
    let fsids: Vec<_> = entries
        .iter()
        .map(|(_, name, fattr)| {
            let mut vals = Bytes::from(fattr.attr_vals.clone());
            let fsid = Fsid4::decode(&mut vals).unwrap();
            (name.as_str(), fsid.major, fsid.minor)
        })
        .collect();

    assert_eq!(fsids, vec![("tree-a", 20, 1), ("tree-b", 30, 1)]);
    assert!(fs.statfs_calls().is_empty());
}

/// READDIR encodes each entry using that entry's FSID when attrs are fetched by fallback.
/// Origin: design/object-scoped-fsid.md fallback READDIR attrs regression.
/// RFC: RFC 8881 §5.8.1.9, §18.23.3.
#[tokio::test]
async fn test_readdir_uses_entry_fsid_with_fallback_attrs() {
    let fs = ObjectScopedFs::new(false);
    let entries = readdir_entries(fs.clone(), &[FATTR4_FSID]).await;
    let fsids: Vec<_> = entries
        .iter()
        .map(|(_, name, fattr)| {
            let mut vals = Bytes::from(fattr.attr_vals.clone());
            let fsid = Fsid4::decode(&mut vals).unwrap();
            (name.as_str(), fsid.major, fsid.minor)
        })
        .collect();

    assert_eq!(fsids, vec![("tree-a", 20, 1), ("tree-b", 30, 1)]);
    assert!(fs.statfs_calls().is_empty());
}

/// Stats attributes call statfs only for objects whose stats are encoded.
/// Origin: design/object-scoped-fsid.md stats-call regression.
/// RFC: RFC 8881 §5.8.1, §18.7.3, §18.23.3.
#[tokio::test]
async fn test_stats_are_fetched_only_for_requested_objects() {
    let fs = ObjectScopedFs::new(true);
    let entries = readdir_entries(fs.clone(), &[FATTR4_SPACE_TOTAL]).await;
    let totals: Vec<_> = entries
        .iter()
        .map(|(_, name, fattr)| {
            let mut vals = Bytes::from(fattr.attr_vals.clone());
            (name.as_str(), u64::decode(&mut vals).unwrap())
        })
        .collect();

    assert_eq!(totals, vec![("tree-a", 2_000), ("tree-b", 3_000)]);
    assert_eq!(fs.statfs_calls(), vec![Object::TreeA, Object::TreeB]);

    let no_stats_fs = ObjectScopedFs::new(true);
    let _ = readdir_entries(no_stats_fs.clone(), &[FATTR4_FSID]).await;
    assert!(no_stats_fs.statfs_calls().is_empty());

    let getattr_no_stats_fs = ObjectScopedFs::new(true);
    let _ = getattr_fattr_bits(
        getattr_no_stats_fs.clone(),
        &[encode_putrootfh()],
        "getattr-no-stats",
        &[FATTR4_FSID],
    )
    .await;
    assert!(getattr_no_stats_fs.statfs_calls().is_empty());
}
