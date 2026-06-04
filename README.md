# embednfs

[![crates.io](https://img.shields.io/crates/v/embednfs)](https://crates.io/crates/embednfs)

An embeddable NFSv4 server library in Rust. You implement a small filesystem trait; the library handles the wire protocol, sessions, filehandles, locking, and TCP serving over both NFSv4.0 (RFC 7530) and NFSv4.1 (RFC 8881) — the same server speaks both minor versions and lets the client pick.

The primary implementation target is Apple/macOS client compatibility for a localhost FUSE-replacement use case. The macOS in-kernel `mount_nfs(8)` does not advertise minor version 1, so embednfs serves NFSv4.0 on that path; Linux is served via NFSv4.1 for sessions and opt-in read-only directory delegations, with the RFC 8276 xattr ops exposed on the NFSv4.2 path.

## Support Boundary

This project currently makes two important non-promises:

- It does **not** guarantee correct or robust behavior over a real network. The target deployment is localhost. Running it over non-localhost transport may work in some cases, but that is not a supported or validated use case.
- It targets macOS first (NFSv4.0 path) and the Linux in-kernel client (NFSv4.1 path) for xattr and directory-delegation workflows. Other clients may work, but they are not a compatibility target.

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

# Linux — vers=4.1 to opt into sessions and delegation-capable clients.
mkdir -p /mnt/embednfs
mount -t nfs4 -o vers=4.1,proto=tcp,port=2049 127.0.0.1:/ /mnt/embednfs

# Linux xattrs — use the NFSv4.2 path for RFC 8276 xattr ops.
mount -t nfs4 -o vers=4.2,proto=tcp,port=2049 127.0.0.1:/ /mnt/embednfs
```

## NFSv4.1 Directory Delegations

embednfs supports opt-in read-only NFSv4.1 directory delegations. A Linux
kernel client that requests `GET_DIR_DELEGATION` can cache directory and
negative-lookup state while the server recalls the delegation before external
namespace mutations.

Client requirement: Linux kernel 6.19 or higher, mounted with `vers=4.1`. Older
Linux kernels may mount and pass normal NFSv4.1 workflows, but can skip
`GET_DIR_DELEGATION`; use the smoke harness with `REQUIRE_DELEGATIONS=1` when
claiming real delegation interop.

Library usage:

```rust
let server = NfsServer::builder(fs)
    .directory_delegations(true)
    .build();
```

Daemon usage:

```bash
EMBEDNFS_DIRECTORY_DELEGATIONS=1 \
EMBEDNFS_RECALL_TIMEOUT_MS=1000 \
cargo run -p embednfsd --release
```

Directory delegations are disabled by default. Embedders that mutate the backing
namespace outside the NFS request path should keep an `NfsServerControl` handle
and call `recall_directory(...)` before applying the external create, unlink, or
rename. The Linux product gate measures visibility after a parent-directory
namespace refresh; path-only `test -e` loops can still observe Linux dentry-cache
state after external unlink or rename. See
[`docs/linux-client-compatibility.md`](docs/linux-client-compatibility.md) for
validated kernel behavior and harness controls.

## NFSv4.1 Server Identity

NFSv4.1 clients use the `EXCHANGE_ID` `server_owner` and `server_scope` fields
to decide whether multiple connections can be treated as the same trunkable
server. The library default is unique per `NfsServer` instance to avoid
accidental trunking between independent localhost servers. Configure a stable
shared identity when one logical server is intentionally exposed through
multiple listeners:

```rust
let identity = embednfs::NfsServerIdentity::new("my-embednfs-server", 0, "my-embednfs-scope");
let server = NfsServer::builder(fs)
    .server_identity(identity)
    .build();
```

`embednfsd` derives a stable default identity from its canonical root and listen
address, so restarts of the same daemon keep the same identity while independent
ports/exports differ. Override it with `EMBEDNFS_SERVER_OWNER_MAJOR_ID`,
`EMBEDNFS_SERVER_OWNER_MINOR_ID`, and `EMBEDNFS_SERVER_SCOPE`.

## Authentication Flavors

By default the server accepts and advertises AUTH_SYS and AUTH_NONE. Restrict
the accepted RPC authentication flavors with an `AuthPolicy`; a request whose
credential flavor is not allowed is rejected at the RPC layer with
`AUTH_TOOWEAK`, before any filesystem call, and `SECINFO` / `SECINFO_NO_NAME`
advertise exactly the configured flavors:

```rust
use embednfs::{AuthPolicy, NfsServer};

// Require AUTH_SYS on every request; reject AUTH_NONE at the protocol boundary.
let server = NfsServer::builder(fs)
    .auth_policy(AuthPolicy::sys_only())
    .build();
```

`AuthPolicy::new([AuthFlavor::Sys, AuthFlavor::None])` sets an explicit list (the
order is the `SECINFO` preference order). Only AUTH_SYS and AUTH_NONE are
meaningfully authenticated; a malformed AUTH_SYS credential still fails with
`AUTH_BADCRED`. The `RequestContext` passed to the filesystem still distinguishes
`AuthContext::Sys` from `AuthContext::None`, so a backend can additionally treat
a missing AUTH_SYS identity as unprivileged.

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

    async fn statfs(&self, ctx: &RequestContext, handle: &Self::Handle) -> FsResult<FsStats>;
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
- `Attrs` carries the exported metadata view. The stable NFS identity is the `(fsid, fileid)` pair; `fileid` only needs to be unique within its `fsid`.
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

Opt-in Linux NFSv4.1 directory-delegation support:

- `GET_DIR_DELEGATION`
- callback `CB_SEQUENCE` and `CB_RECALL`
- state-aware `DELEGRETURN`, `TEST_STATEID`, and `FREE_STATEID` for directory delegations

Kept as cheap compatibility ops:

- `SECINFO`
- `PUTPUBFH`
- `VERIFY`
- `DELEGPURGE`
- `BIND_CONN_TO_SESSION`

Explicitly unsupported:

- `BACKCHANNEL_CTL`
- `GETDEVICEINFO`, `GETDEVICELIST`
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
