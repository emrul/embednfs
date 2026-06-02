mod common;

use crate::common::*;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CreateRequest, CreateResult, DirPage, FileSystem, FsLimits, FsResult,
    FsStats, MemFs, ReadResult, RequestContext, SetAttrs, WriteResult, WriteStability,
};
use embednfs_proto::{NfsStat4, OP_READ};

const LARGE_READ_BYTES: usize = 3 * 1024 * 1024;

struct LargeReadLimitFs {
    inner: MemFs,
}

#[async_trait::async_trait]
impl FileSystem for LargeReadLimitFs {
    type Handle = u64;

    fn root(&self) -> Self::Handle {
        self.inner.root()
    }

    fn capabilities(&self) -> embednfs::FsCapabilities {
        self.inner.capabilities()
    }

    fn limits(&self) -> FsLimits {
        FsLimits {
            max_read: LARGE_READ_BYTES as u32,
            ..self.inner.limits()
        }
    }

    async fn statfs(&self, ctx: &RequestContext, handle: &Self::Handle) -> FsResult<FsStats> {
        self.inner.statfs(ctx, handle).await
    }

    async fn getattr(&self, ctx: &RequestContext, handle: &Self::Handle) -> FsResult<Attrs> {
        self.inner.getattr(ctx, handle).await
    }

    async fn access(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        requested: AccessMask,
    ) -> FsResult<AccessMask> {
        self.inner.access(ctx, handle, requested).await
    }

    async fn lookup(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<Self::Handle> {
        self.inner.lookup(ctx, parent, name).await
    }

    async fn parent(
        &self,
        ctx: &RequestContext,
        dir: &Self::Handle,
    ) -> FsResult<Option<Self::Handle>> {
        self.inner.parent(ctx, dir).await
    }

    async fn readdir(
        &self,
        ctx: &RequestContext,
        dir: &Self::Handle,
        cookie: u64,
        max_entries: u32,
        with_attrs: bool,
    ) -> FsResult<DirPage<Self::Handle>> {
        self.inner
            .readdir(ctx, dir, cookie, max_entries, with_attrs)
            .await
    }

    async fn read(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> FsResult<ReadResult> {
        self.inner.read(ctx, handle, offset, count).await
    }

    async fn write(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        data: Bytes,
        requested: WriteStability,
    ) -> FsResult<WriteResult> {
        self.inner.write(ctx, handle, offset, data, requested).await
    }

    async fn create(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
        req: CreateRequest,
    ) -> FsResult<CreateResult<Self::Handle>> {
        self.inner.create(ctx, parent, name, req).await
    }

    async fn remove(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<()> {
        self.inner.remove(ctx, parent, name).await
    }

    async fn rename(
        &self,
        ctx: &RequestContext,
        from_dir: &Self::Handle,
        from_name: &str,
        to_dir: &Self::Handle,
        to_name: &str,
    ) -> FsResult<()> {
        self.inner
            .rename(ctx, from_dir, from_name, to_dir, to_name)
            .await
    }

    async fn setattr(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        attrs: &SetAttrs,
    ) -> FsResult<Attrs> {
        self.inner.setattr(ctx, handle, attrs).await
    }
}

/// Large READ replies are emitted across multiple RFC 5531 record fragments and still decode as one RPC reply.
/// Origin: transport interoperability smoke for outbound RPC-over-TCP fragmentation, after confirming `nfs4j` can reassemble multi-fragment replies.
/// RFC: RFC 5531 §11; RFC 8881 §18.22.3.
#[tokio::test]
async fn test_large_read_reply_uses_multiple_rpc_fragments() {
    let payload = vec![0x5a; LARGE_READ_BYTES];
    let fs = LargeReadLimitFs {
        inner: fs_with_data("big.bin", &payload).await,
    };
    let port = start_server_with_fs(fs).await;
    let mut stream = connect(port).await;
    let sessionid = setup_session(&mut stream).await;

    let seq_op = encode_sequence(&sessionid, 1, 0);
    let rootfh_op = encode_putrootfh();
    let lookup_op = encode_lookup("big.bin");
    let read_op = encode_read(0, LARGE_READ_BYTES as u32);
    let compound = encode_compound(
        "fragmented-read",
        &[&seq_op, &rootfh_op, &lookup_op, &read_op],
    );

    let (mut resp, fragment_count) = send_rpc_record(&mut stream, 3, 1, &compound).await;
    assert!(
        fragment_count > 1,
        "expected fragmented reply, got {fragment_count} fragment(s)"
    );

    parse_rpc_reply(&mut resp);
    let (status, _, num_results) = parse_compound_header(&mut resp);
    assert_eq!(status, NfsStat4::Ok as u32);
    assert_eq!(num_results, 4);

    let _ = parse_op_header(&mut resp);
    skip_sequence_res(&mut resp);
    let _ = parse_op_header(&mut resp);
    let _ = parse_op_header(&mut resp);
    let (opnum, op_status) = parse_op_header(&mut resp);
    assert_eq!(opnum, OP_READ);
    assert_eq!(op_status, NfsStat4::Ok as u32);

    let (eof, data) = parse_read_res(&mut resp);
    assert!(eof);
    assert_eq!(data.len(), LARGE_READ_BYTES);
    assert!(data.iter().all(|byte| *byte == 0x5a));
}
