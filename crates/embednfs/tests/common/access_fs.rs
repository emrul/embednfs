//! A permission-enforcing `FileSystem` wrapper for OPEN authorization tests.
//!
//! `MemFs` is permissionless (its `access` grants everything requested), so it
//! cannot exercise the fail-closed OPEN path. `AccessPolicyFs` wraps `MemFs` and
//! returns a restricted access mask from `access`, either derived from the
//! object's POSIX owner mode bits or fixed to a read-only export.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;

use embednfs::{
    AccessMask, Attrs, CommitSupport, CreateRequest, CreateResult, DirPage, FileSystem, FsResult,
    FsStats, HardLinks, MemFs, ReadResult, RequestContext, SetAttrs, Symlinks, WriteResult,
    WriteStability, XattrSetMode, Xattrs,
};

/// How `AccessPolicyFs` decides which access bits to grant.
#[derive(Clone, Copy)]
pub enum AccessPolicy {
    /// Grant access derived from the object's POSIX owner mode bits. Tests run
    /// as the file owner (uid 0), so only the owner triad is consulted.
    OwnerMode,
    /// Deny all write-related access regardless of mode (a read-only export).
    ReadOnly,
    /// Grant only XATTR_LIST — a backend that allows listing the named-attribute
    /// directory but neither reading attribute values nor writing them. Used to
    /// prove the per-op XATTR_READ/XATTR_WRITE gates are independent of the
    /// XATTR_LIST traversal gate (POSIX mode bits cannot separate the two).
    XattrListOnly,
}

/// Counts how many times each backend `Xattrs` method is actually reached, so a
/// test can assert that a denied op short-circuits before touching the backend.
#[derive(Default)]
pub struct XattrCallCounts {
    pub list: AtomicUsize,
    pub get: AtomicUsize,
    pub set: AtomicUsize,
    pub remove: AtomicUsize,
}

/// A `MemFs` wrapper that enforces `policy` from its `access` implementation and
/// counts the backend `Xattrs` calls that reach the inner filesystem.
pub struct AccessPolicyFs {
    pub inner: MemFs,
    pub policy: AccessPolicy,
    pub calls: Arc<XattrCallCounts>,
}

impl AccessPolicyFs {
    pub fn new(inner: MemFs, policy: AccessPolicy) -> Self {
        Self {
            inner,
            policy,
            calls: Arc::new(XattrCallCounts::default()),
        }
    }

    /// Returns a shared handle to the backend `Xattrs` call counters.
    pub fn calls(&self) -> Arc<XattrCallCounts> {
        self.calls.clone()
    }
}

#[async_trait::async_trait]
impl Xattrs<u64> for AccessPolicyFs {
    async fn list_xattrs(&self, ctx: &RequestContext, id: &u64) -> FsResult<Vec<String>> {
        let _ = self.calls.list.fetch_add(1, Ordering::Relaxed);
        self.inner.list_xattrs(ctx, id).await
    }
    async fn get_xattr(&self, ctx: &RequestContext, id: &u64, name: &str) -> FsResult<Bytes> {
        let _ = self.calls.get.fetch_add(1, Ordering::Relaxed);
        self.inner.get_xattr(ctx, id, name).await
    }
    async fn set_xattr(
        &self,
        ctx: &RequestContext,
        id: &u64,
        name: &str,
        value: Bytes,
        mode: XattrSetMode,
    ) -> FsResult<()> {
        let _ = self.calls.set.fetch_add(1, Ordering::Relaxed);
        self.inner.set_xattr(ctx, id, name, value, mode).await
    }
    async fn remove_xattr(&self, ctx: &RequestContext, id: &u64, name: &str) -> FsResult<()> {
        let _ = self.calls.remove.fetch_add(1, Ordering::Relaxed);
        self.inner.remove_xattr(ctx, id, name).await
    }
}

fn owner_mode_access(mode: u32) -> AccessMask {
    let mut granted = AccessMask::NONE;
    if mode & 0o400 != 0 {
        granted |=
            AccessMask::READ | AccessMask::LOOKUP | AccessMask::XATTR_READ | AccessMask::XATTR_LIST;
    }
    if mode & 0o200 != 0 {
        granted |=
            AccessMask::MODIFY | AccessMask::EXTEND | AccessMask::DELETE | AccessMask::XATTR_WRITE;
    }
    if mode & 0o100 != 0 {
        granted |= AccessMask::EXECUTE;
    }
    granted
}

fn read_only_access() -> AccessMask {
    AccessMask::READ
        | AccessMask::LOOKUP
        | AccessMask::EXECUTE
        | AccessMask::XATTR_READ
        | AccessMask::XATTR_LIST
}

#[async_trait::async_trait]
impl FileSystem for AccessPolicyFs {
    type Handle = u64;

    fn root(&self) -> Self::Handle {
        self.inner.root()
    }
    fn capabilities(&self) -> embednfs::FsCapabilities {
        self.inner.capabilities()
    }
    fn limits(&self) -> embednfs::FsLimits {
        self.inner.limits()
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
        let granted = match self.policy {
            AccessPolicy::OwnerMode => {
                owner_mode_access(self.inner.getattr(ctx, handle).await?.mode)
            }
            AccessPolicy::ReadOnly => read_only_access(),
            AccessPolicy::XattrListOnly => AccessMask::XATTR_LIST,
        };
        Ok(granted & requested)
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
    fn symlinks(&self) -> Option<&dyn Symlinks<Self::Handle>> {
        self.inner.symlinks()
    }
    fn hard_links(&self) -> Option<&dyn HardLinks<Self::Handle>> {
        self.inner.hard_links()
    }
    fn xattrs(&self) -> Option<&dyn Xattrs<Self::Handle>> {
        // Route through the counting wrapper (not the inner FS) so tests can
        // observe whether a denied op reached the backend.
        self.inner
            .xattrs()
            .map(|_| self as &dyn Xattrs<Self::Handle>)
    }
    fn commit_support(&self) -> Option<&dyn CommitSupport<Self::Handle>> {
        self.inner.commit_support()
    }
}
