# embednfs

[![crates.io](https://img.shields.io/crates/v/embednfs)](https://crates.io/crates/embednfs)

An embeddable NFSv4 server library in Rust. You implement a small filesystem trait; the library handles the wire protocol, sessions, filehandles, locking, and TCP serving over both NFSv4.0 (RFC 7530) and NFSv4.1 (RFC 8881) — the same server speaks both minor versions and lets the client pick.

The primary implementation target is Apple/macOS client compatibility for a localhost FUSE-replacement use case. The macOS in-kernel `mount_nfs(8)` does not advertise minor version 1, so embednfs serves NFSv4.0 on that path; Linux is served via NFSv4.1 where the protocol picks up sessions and the xattr ops (RFC 8276) Linux relies on for extended attributes.

## Support Boundary

This project currently makes two important non-promises:

- It does **not** guarantee correct or robust behavior over a real network. The target deployment is localhost. Running it over non-localhost transport may work in some cases, but that is not a supported or validated use case.
- It targets macOS first (NFSv4.0 path) and the Linux in-kernel client (NFSv4.1 path) for xattr workflows. Other clients may work, but they are not a compatibility target.

In short: the supported target is **macOS / Linux over localhost**.

## Architecture

This is a Cargo workspace with three crates:

- **`embednfs-proto`** — XDR encoding/decoding and NFSv4.0 / NFSv4.1 protocol types
- **`embednfs`** — Embeddable server library with the filesystem traits and COMPOUND handler
- **`embednfsd`** — NFSv4 server daemon powered by embednfs

## Quick Start

```rust
use embednfs::{MemFs, NfsServer};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let fs = MemFs::new();
    let server = NfsServer::new(fs);
    server.listen("127.0.0.1:2049").await
}
```

Then mount:

```bash
# macOS — vers=4 (the only value mount_nfs(8) accepts).
mkdir -p /tmp/embednfs
mount_nfs -o vers=4,tcp,port=2049 127.0.0.1:/ /tmp/embednfs

# Linux — vers=4.1 to opt into sessions and xattr ops.
mkdir -p /mnt/embednfs
mount -t nfs4 -o vers=4.1,proto=tcp,port=2049 127.0.0.1:/ /mnt/embednfs
```

## Filesystem API

The filesystem API is handle-based and models the exported filesystem rather than the raw backing store. Weak backends such as exFAT- or S3-style adapters are expected to provide stable handles, exported attrs, and any overlay metadata they need behind the trait.

### Core Trait

```rust
use async_trait::async_trait;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CreateRequest, CreateResult, DirPage, FileSystem, FsCapabilities,
    FsLimits, FsResult, FsStats, ReadResult, RequestContext, SetAttrs, WriteResult,
};

#[async_trait]
pub trait FileSystem: Send + Sync + 'static {
    type Handle: Clone + Eq + std::hash::Hash + Send + Sync + 'static;

    fn root(&self) -> Self::Handle;
    fn capabilities(&self) -> FsCapabilities;
    fn limits(&self) -> FsLimits;

    async fn statfs(&self, ctx: &RequestContext) -> FsResult<FsStats>;
    async fn getattr(&self, ctx: &RequestContext, handle: &Self::Handle) -> FsResult<Attrs>;
    async fn access(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        requested: AccessMask,
    ) -> FsResult<AccessMask>;
    async fn lookup(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<Self::Handle>;
    async fn parent(
        &self,
        ctx: &RequestContext,
        dir: &Self::Handle,
    ) -> FsResult<Option<Self::Handle>>;
    async fn readdir(
        &self,
        ctx: &RequestContext,
        dir: &Self::Handle,
        cookie: u64,
        max_entries: u32,
        with_attrs: bool,
    ) -> FsResult<DirPage<Self::Handle>>;
    async fn read(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> FsResult<ReadResult>;
    async fn write(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        data: Bytes,
    ) -> FsResult<WriteResult>;
    async fn create(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
        req: CreateRequest,
    ) -> FsResult<CreateResult<Self::Handle>>;
    async fn remove(
        &self,
        ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<()>;
    async fn rename(
        &self,
        ctx: &RequestContext,
        from_dir: &Self::Handle,
        from_name: &str,
        to_dir: &Self::Handle,
        to_name: &str,
    ) -> FsResult<()>;
    async fn setattr(
        &self,
        ctx: &RequestContext,
        handle: &Self::Handle,
        attrs: &SetAttrs,
    ) -> FsResult<Attrs>;
}
```

Key points:

- `Handle` is opaque backend identity. It is not the NFS wire handle and not the exported `fileid`.
- `Attrs` carries the exported metadata view, including `fileid`, `change`, times, flags, and ownership.
- `RequestContext` is passed to every op so adapters can make explicit policy decisions.
- `readdir()` is paged and cookie-driven, with optional inline attrs for `READDIR` hot paths.

### Extension Traits

The server will use these when present:

- `Xattrs` for macOS named attributes / xattrs / named streams
- `Symlinks` for `CREATE symlink` and `READLINK`
- `HardLinks` for `LINK`
- `CommitSupport` for explicit `COMMIT`

If an extension trait is absent, the server returns the appropriate NFS unsupported/type errors and does not advertise the feature where that matters.

## Apple-Focused Operation Support

Implemented for normal Apple/macOS client flows:

- `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`, `DESTROY_SESSION`, `DESTROY_CLIENTID`
- `PUTROOTFH`, `PUTFH`, `GETFH`, `LOOKUP`, `LOOKUPP`, `SAVEFH`, `RESTOREFH`
- `GETATTR`, `ACCESS`, `OPEN`, `CLOSE`, `OPEN_DOWNGRADE`
- `READ`, `WRITE`, `COMMIT`, `READDIR`, `SETATTR`
- `CREATE` for directories and symlinks
- `REMOVE`, `RENAME`
- `LOCK`, `LOCKT`, `LOCKU`
- `SECINFO_NO_NAME`
- `OPENATTR`
- `NVERIFY`
- `RECLAIM_COMPLETE`, `FREE_STATEID`

Supported through extensions:

- `READLINK`
- `LINK`
- macOS named-attribute and xattr flows behind `OPENATTR`

Kept as cheap compatibility ops:

- `SECINFO`
- `PUTPUBFH`
- `VERIFY`
- `TEST_STATEID`
- `DELEGPURGE`
- `BIND_CONN_TO_SESSION`
- `DELEGRETURN`

Explicitly unsupported:

- `BACKCHANNEL_CTL`
- `GETDEVICEINFO`, `GETDEVICELIST`
- `GET_DIR_DELEGATION`
- `LAYOUTGET`, `LAYOUTRETURN`, `LAYOUTCOMMIT`
- `SET_SSV`
- `WANT_DELEGATION`

Implemented for NFSv4.0 mount paths (returned `NFS4ERR_NOTSUPP` if a v4.1 client sends them, per RFC 8881):

- `SETCLIENTID`, `SETCLIENTID_CONFIRM`
- `RENEW`
- `OPEN_CONFIRM`
- `RELEASE_LOCKOWNER`

## Testing

```bash
cargo clippy --workspace
cargo test --workspace
bash scripts/ensure-nfs4j-client.sh
cargo test -p embednfs --test nfs4j_smoke -- --ignored --nocapture
cargo test -p embednfs --test nfs4j_stress -- --ignored --nocapture
./scripts/smoke-macos-nfs41.sh
```

The integration suite exercises the full RPC path over TCP and includes raw `OPENATTR`/named-attribute flows for macOS-style clients.

`cargo test --workspace` also runs the small default foreign-client interoperability smoke lane through `nfs-rs`.

The ignored `nfs4j` smoke and stress tests use the pinned harness from `https://github.com/PeronGH/nfs4j.git` at commit `9d433b98bf56ea6d5cf791388c9d75ad32d5d0f2`. `scripts/ensure-nfs4j-client.sh` clones or reuses `/tmp/nfs4j`, checks out that exact ref, builds `basic-client`, and prints the resulting `jar-with-dependencies` path.

For a genuine localhost/macOS smoke test, `scripts/smoke-macos-nfs41.sh` starts `embednfsd`, mounts it with `mount_nfs` using NFSv4.0, and exercises basic create/write/read/rename/remove/rmdir behavior through the kernel client.

Many of the protocol conformance tests are adapted from the maintained `pynfs` tree at `git://git.linux-nfs.org/projects/cdmackay/pynfs.git`.

## License

MIT
