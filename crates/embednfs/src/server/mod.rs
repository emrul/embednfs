use bytes::{Bytes, BytesMut};
/// NFSv4.1 server - COMPOUND procedure handling.
///
/// This is the core of the NFS server. It receives COMPOUND requests,
/// dispatches each operation, and builds the COMPOUND response.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info};

use embednfs_proto::xdr::*;
use embednfs_proto::*;

use crate::fs::*;
use crate::identity::NfsServerIdentity;
use crate::internal::{ObjectId, ServerObject};
use crate::session::StateManager;

const RPC_LAST_FRAGMENT: u32 = 0x8000_0000;
const RPC_FRAG_LEN_MASK: u32 = 0x7fff_ffff;
const MAX_FRAGMENT_SIZE: usize = 2 * 1024 * 1024;
const CONN_BUF_SIZE: usize = 65_536;

type NfsResult<T> = FsResult<T>;

mod backchannel;
mod compound;
mod file_attrs;
mod objects;
mod ops;
mod transport;

/// Maps numeric ids to NFS owner/group strings.
pub trait IdMapper: Send + Sync + 'static {
    /// Maps a numeric uid to an NFS owner string.
    fn owner(&self, uid: u32) -> String;

    /// Maps a numeric gid to an NFS owner-group string.
    fn group(&self, gid: u32) -> String;
}

/// Default id mapper that renders numeric ids directly.
pub struct NumericIdMapper;

impl IdMapper for NumericIdMapper {
    fn owner(&self, uid: u32) -> String {
        uid.to_string()
    }

    fn group(&self, gid: u32) -> String {
        gid.to_string()
    }
}

/// Which RPC authentication flavors the server accepts and advertises.
///
/// A request whose credential flavor is not in this set is rejected at the RPC
/// layer with `AUTH_TOOWEAK`, before any filesystem/backend call runs, and the
/// same set is what `SECINFO` and `SECINFO_NO_NAME` report. The default accepts
/// AUTH_SYS and AUTH_NONE, matching the historical behavior.
///
/// Only AUTH_SYS and AUTH_NONE are meaningfully authenticated by this server.
/// Other flavors may be listed, in which case they are advertised and accepted
/// at the RPC layer but resolve to [`AuthContext::Unknown`]; the filesystem
/// implementation is then responsible for how it treats them.
#[derive(Debug, Clone)]
pub struct AuthPolicy {
    flavors: Vec<AuthFlavor>,
}

impl AuthPolicy {
    /// Accepts and advertises exactly `flavors`, preserving their order for the
    /// `SECINFO` reply (most-preferred first).
    pub fn new(flavors: impl IntoIterator<Item = AuthFlavor>) -> Self {
        Self {
            flavors: flavors.into_iter().collect(),
        }
    }

    /// Accepts only AUTH_SYS — a request must carry AUTH_SYS credentials, and
    /// AUTH_NONE (or anything else) is rejected with `AUTH_TOOWEAK`.
    pub fn sys_only() -> Self {
        Self::new([AuthFlavor::Sys])
    }

    /// Accepts AUTH_SYS and AUTH_NONE — the default, backward-compatible policy.
    pub fn sys_and_none() -> Self {
        Self::new([AuthFlavor::Sys, AuthFlavor::None])
    }

    /// The accepted flavors in `SECINFO` order (most-preferred first).
    pub fn flavors(&self) -> &[AuthFlavor] {
        &self.flavors
    }

    /// Returns whether a raw RPC auth-flavor number is accepted.
    pub fn allows(&self, flavor: u32) -> bool {
        self.flavors.iter().any(|f| *f as u32 == flavor)
    }
}

impl Default for AuthPolicy {
    fn default() -> Self {
        Self::sys_and_none()
    }
}

/// Builder for [`NfsServer`].
pub struct NfsServerBuilder<F: FileSystem> {
    fs: F,
    id_mapper: Arc<dyn IdMapper>,
    delegation_config: DelegationConfig,
    server_identity: NfsServerIdentity,
    auth_policy: AuthPolicy,
}

impl<F: FileSystem> NfsServerBuilder<F> {
    /// Replaces the uid/gid string mapper used for `owner` attributes.
    pub fn id_mapper<M: IdMapper>(mut self, mapper: M) -> Self {
        self.id_mapper = Arc::new(mapper);
        self
    }

    /// Enables or disables NFSv4.1 directory delegations.
    pub fn directory_delegations(mut self, enabled: bool) -> Self {
        self.delegation_config.directory_delegations = enabled;
        self
    }

    /// Replaces the delegation configuration.
    pub fn delegation_config(mut self, config: DelegationConfig) -> Self {
        self.delegation_config = config;
        self
    }

    /// Replaces the NFSv4.1 server identity returned by `EXCHANGE_ID`.
    pub fn server_identity(mut self, identity: NfsServerIdentity) -> Self {
        self.server_identity = identity;
        self
    }

    /// Replaces the NFSv4.1 `server_owner` value returned by `EXCHANGE_ID`.
    pub fn server_owner(mut self, minor_id: u64, major_id: impl Into<Bytes>) -> Self {
        self.server_identity = self
            .server_identity
            .with_owner_minor_id(minor_id)
            .with_owner_major_id(major_id);
        self
    }

    /// Replaces the NFSv4.1 `server_scope` value returned by `EXCHANGE_ID`.
    pub fn server_scope(mut self, scope: impl Into<Bytes>) -> Self {
        self.server_identity = self.server_identity.with_scope(scope);
        self
    }

    /// Replaces the accepted/advertised RPC authentication flavors.
    ///
    /// Example: restrict the server to AUTH_SYS only.
    /// ```no_run
    /// # use embednfs::{AuthPolicy, MemFs, NfsServer};
    /// let server = NfsServer::builder(MemFs::new())
    ///     .auth_policy(AuthPolicy::sys_only())
    ///     .build();
    /// ```
    pub fn auth_policy(mut self, policy: AuthPolicy) -> Self {
        self.auth_policy = policy;
        self
    }

    /// Builds the server instance.
    pub fn build(self) -> NfsServer<F> {
        NfsServer {
            fs: Arc::new(self.fs),
            state: Arc::new(StateManager::with_server_identity(self.server_identity)),
            handle_to_object: Arc::new(RwLock::new(HashMap::new())),
            object_to_handle: Arc::new(RwLock::new(HashMap::new())),
            next_object_id: AtomicU64::new(1),
            id_mapper: self.id_mapper,
            delegation_config: self.delegation_config,
            auth_policy: self.auth_policy,
            backchannels: Arc::new(backchannel::BackchannelManager::default()),
        }
    }
}

/// Configuration for NFSv4 delegation behavior.
#[derive(Debug, Clone)]
pub struct DelegationConfig {
    /// Enables read-only directory delegations on NFSv4.1+ sessions.
    pub directory_delegations: bool,
    /// Maximum time to wait for a recalled delegation to be returned.
    pub recall_timeout: Duration,
    /// Maximum number of delegations one client may hold.
    pub max_delegations_per_client: usize,
    /// Maximum number of delegations the server may hold in total.
    pub max_delegations_total: usize,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            directory_delegations: false,
            recall_timeout: Duration::from_secs(5),
            max_delegations_per_client: 1024,
            max_delegations_total: 16_384,
        }
    }
}

/// The NFS server.
pub struct NfsServer<F: FileSystem> {
    fs: Arc<F>,
    state: Arc<StateManager>,
    handle_to_object: Arc<RwLock<HashMap<F::Handle, ObjectId>>>,
    object_to_handle: Arc<RwLock<HashMap<ObjectId, F::Handle>>>,
    next_object_id: AtomicU64,
    id_mapper: Arc<dyn IdMapper>,
    delegation_config: DelegationConfig,
    auth_policy: AuthPolicy,
    backchannels: Arc<backchannel::BackchannelManager>,
}

/// Cloneable control handle for a running [`NfsServer`].
///
/// Keep this handle before calling [`NfsServer::serve`] or [`NfsServer::listen`]
/// when an embedder needs to trigger server-side actions, such as recalling
/// directory delegations before applying namespace changes outside the NFS
/// request path.
pub struct NfsServerControl<H>
where
    H: Clone + Eq + Hash + Send + Sync + 'static,
{
    state: Arc<StateManager>,
    handle_to_object: Arc<RwLock<HashMap<H, ObjectId>>>,
    delegation_config: DelegationConfig,
    backchannels: Arc<backchannel::BackchannelManager>,
}

impl<H> Clone for NfsServerControl<H>
where
    H: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            handle_to_object: self.handle_to_object.clone(),
            delegation_config: self.delegation_config.clone(),
            backchannels: self.backchannels.clone(),
        }
    }
}

impl<F: FileSystem> NfsServer<F> {
    /// Creates a builder for a new server.
    pub fn builder(fs: F) -> NfsServerBuilder<F> {
        NfsServerBuilder {
            fs,
            id_mapper: Arc::new(NumericIdMapper),
            delegation_config: DelegationConfig::default(),
            server_identity: NfsServerIdentity::default(),
            auth_policy: AuthPolicy::default(),
        }
    }

    /// Create a new NFS server with the given filesystem.
    pub fn new(fs: F) -> Self {
        Self::builder(fs).build()
    }

    /// Returns a cloneable handle for controlling this server after it starts.
    pub fn control_handle(&self) -> NfsServerControl<F::Handle> {
        NfsServerControl {
            state: self.state.clone(),
            handle_to_object: self.handle_to_object.clone(),
            delegation_config: self.delegation_config.clone(),
            backchannels: self.backchannels.clone(),
        }
    }

    /// Start listening on the given address.
    pub async fn listen(self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.serve(listener).await
    }

    /// Serve on an already-bound TCP listener. Returns the local address.
    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        let local_addr = listener.local_addr()?;
        info!("NFSv4.1 server listening on {local_addr}");

        let server = Arc::new(self);

        loop {
            let (stream, peer) = listener.accept().await?;
            stream.set_nodelay(true)?;
            debug!("New connection from {peer}");
            let server = server.clone();
            std::mem::drop(tokio::spawn(async move {
                if let Err(e) = server.handle_connection(stream).await {
                    debug!("Connection error from {peer}: {e}");
                }
            }));
        }
    }

    fn symlinks(&self) -> Option<&dyn Symlinks<F::Handle>> {
        self.fs.symlinks()
    }

    fn hard_links(&self) -> Option<&dyn HardLinks<F::Handle>> {
        self.fs.hard_links()
    }

    fn named_attrs(&self) -> Option<&dyn Xattrs<F::Handle>> {
        self.fs.xattrs()
    }

    fn syncer(&self) -> Option<&dyn CommitSupport<F::Handle>> {
        self.fs.commit_support()
    }

    fn lifecycle(&self) -> Option<&dyn OpenLifecycle<F::Handle>> {
        self.fs.open_lifecycle()
    }

    fn fh_has_valid_format(fh: &NfsFh4) -> bool {
        fh.0.len() == std::mem::size_of::<u64>()
    }

    fn parse_auth_sys(body: &Bytes) -> Option<AuthSysParams> {
        let mut body = body.clone();
        let params = AuthSysParams::decode(&mut body).ok()?;
        if body.is_empty() { Some(params) } else { None }
    }

    fn validate_rpc_auth(&self, call: &RpcCallHeader) -> Result<(), AuthStat> {
        // Reject a disallowed credential flavor before any further work — this is
        // the protocol-boundary enforcement of the auth policy. The verifier
        // flavor is not policy-checked: an AUTH_SYS call carries an AUTH_NONE
        // verifier, which must still be accepted under a sys-only policy.
        if !self.auth_policy.allows(call.cred.flavor) {
            return Err(AuthStat::TooWeak);
        }

        if call.cred.flavor == AuthFlavor::Sys as u32
            && Self::parse_auth_sys(&call.cred.body).is_none()
        {
            return Err(AuthStat::BadCred);
        }

        if call.verf.flavor == AuthFlavor::Sys as u32
            && Self::parse_auth_sys(&call.verf.body).is_none()
        {
            return Err(AuthStat::BadVerf);
        }

        Ok(())
    }

    /// The `SECINFO`/`SECINFO_NO_NAME` flavor list derived from the auth policy.
    pub(crate) fn secinfo_flavors(&self) -> Vec<SecinfoEntry4> {
        self.auth_policy
            .flavors()
            .iter()
            .map(|flavor| SecinfoEntry4 {
                flavor: *flavor as u32,
            })
            .collect()
    }

    fn request_context(cred: &OpaqueAuth) -> RequestContext {
        let auth = match cred.flavor {
            x if x == AuthFlavor::None as u32 => AuthContext::None,
            x if x == AuthFlavor::Sys as u32 => match Self::parse_auth_sys(&cred.body) {
                Some(params) => AuthContext::Sys {
                    uid: params.uid,
                    gid: params.gid,
                    supplemental_gids: params.gids,
                },
                None => AuthContext::Unknown {
                    flavor: cred.flavor,
                },
            },
            flavor => AuthContext::Unknown { flavor },
        };

        RequestContext { auth }
    }

    fn capabilities(&self) -> FsCapabilities {
        self.fs.capabilities()
    }

    fn limits(&self) -> FsLimits {
        self.fs.limits()
    }

    async fn statfs(&self, ctx: &RequestContext, id: ObjectId) -> NfsResult<FsStats> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs.statfs(ctx, &handle).await
    }

    async fn statfs_for_object(
        &self,
        ctx: &RequestContext,
        object: &ServerObject,
    ) -> NfsResult<FsStats> {
        match object {
            ServerObject::Fs(id)
            | ServerObject::NamedAttrDir(id)
            | ServerObject::NamedAttrFile { parent: id, .. } => self.statfs(ctx, *id).await,
        }
    }

    async fn getattr(&self, ctx: &RequestContext, id: ObjectId) -> NfsResult<Attrs> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs.getattr(ctx, &handle).await
    }

    async fn kind_of(&self, ctx: &RequestContext, id: ObjectId) -> NfsResult<ObjectType> {
        self.getattr(ctx, id).await.map(|attrs| attrs.object_type)
    }

    async fn access_for(
        &self,
        ctx: &RequestContext,
        id: ObjectId,
        requested: AccessMask,
    ) -> NfsResult<AccessMask> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs.access(ctx, &handle, requested).await
    }

    /// Central authorization gate: requires every bit in `need` to be granted on
    /// `id`, mapping a shortfall to `NFS4ERR_ACCESS` and backend errors through
    /// `to_nfsstat4`. The synthetic named-attribute namespace routes its
    /// LOOKUP/READDIR/READ/WRITE/REMOVE through here against the parent object's
    /// xattr permissions, so those ops agree with ACCESS and the RFC 8276 xattr
    /// ops rather than trusting each backend `Xattrs` method to self-police.
    async fn require_access(
        &self,
        ctx: &RequestContext,
        id: ObjectId,
        need: AccessMask,
    ) -> Result<(), NfsStat4> {
        match self.access_for(ctx, id, need).await {
            Ok(granted) if granted.contains(need) => Ok(()),
            Ok(_) => Err(NfsStat4::Access),
            Err(e) => Err(e.to_nfsstat4()),
        }
    }

    async fn lookup(
        &self,
        ctx: &RequestContext,
        dir_id: ObjectId,
        name: &str,
    ) -> NfsResult<ObjectId> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        let child = self.fs.lookup(ctx, &handle, name).await?;
        Ok(self.register_handle(&child).await)
    }

    async fn lookup_parent(&self, ctx: &RequestContext, dir_id: ObjectId) -> NfsResult<ObjectId> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        let parent = self.fs.parent(ctx, &handle).await?;
        match parent {
            Some(handle) => Ok(self.register_handle(&handle).await),
            None => Err(FsError::NotFound),
        }
    }

    async fn read(
        &self,
        ctx: &RequestContext,
        id: ObjectId,
        offset: u64,
        count: u32,
    ) -> NfsResult<(Bytes, bool)> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs
            .read(ctx, &handle, offset, count)
            .await
            .map(|res| (res.data, res.eof))
    }

    async fn write(
        &self,
        ctx: &RequestContext,
        id: ObjectId,
        offset: u64,
        data: Bytes,
        requested: WriteStability,
    ) -> NfsResult<WriteResult> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs.write(ctx, &handle, offset, data, requested).await
    }

    async fn create_file(
        &self,
        ctx: &RequestContext,
        dir_id: ObjectId,
        name: &str,
        attrs: SetAttrs,
    ) -> NfsResult<CreateResult<ObjectId>> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        let created = self
            .fs
            .create(
                ctx,
                &handle,
                name,
                CreateRequest {
                    kind: CreateKind::File,
                    attrs,
                },
            )
            .await?;
        Ok(CreateResult {
            handle: self.register_handle(&created.handle).await,
            attrs: created.attrs,
        })
    }

    async fn create_dir(
        &self,
        ctx: &RequestContext,
        dir_id: ObjectId,
        name: &str,
        attrs: SetAttrs,
    ) -> NfsResult<CreateResult<ObjectId>> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        let created = self
            .fs
            .create(
                ctx,
                &handle,
                name,
                CreateRequest {
                    kind: CreateKind::Directory,
                    attrs,
                },
            )
            .await?;
        Ok(CreateResult {
            handle: self.register_handle(&created.handle).await,
            attrs: created.attrs,
        })
    }

    async fn remove(&self, ctx: &RequestContext, dir_id: ObjectId, name: &str) -> NfsResult<()> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        self.fs.remove(ctx, &handle, name).await
    }

    async fn rename(
        &self,
        ctx: &RequestContext,
        from_dir: ObjectId,
        from_name: &str,
        to_dir: ObjectId,
        to_name: &str,
    ) -> NfsResult<()> {
        let from_handle = self.resolve_backend_handle(from_dir).await?;
        let to_handle = self.resolve_backend_handle(to_dir).await?;
        self.fs
            .rename(ctx, &from_handle, from_name, &to_handle, to_name)
            .await
    }

    async fn readdir(
        &self,
        ctx: &RequestContext,
        dir_id: ObjectId,
        cookie: u64,
        max_entries: u32,
        with_attrs: bool,
    ) -> NfsResult<DirPage<ObjectId>> {
        let handle = self.resolve_backend_handle(dir_id).await?;
        let page = self
            .fs
            .readdir(ctx, &handle, cookie, max_entries, with_attrs)
            .await?;
        let mut entries = Vec::with_capacity(page.entries.len());
        for entry in page.entries {
            let object_id = self.register_handle(&entry.handle).await;
            entries.push(DirEntry {
                name: entry.name,
                handle: object_id,
                cookie: entry.cookie,
                attrs: entry.attrs,
            });
        }
        Ok(DirPage {
            entries,
            eof: page.eof,
        })
    }

    async fn setattr_real(
        &self,
        ctx: &RequestContext,
        id: ObjectId,
        attrs: &SetAttrs,
    ) -> NfsResult<Attrs> {
        let handle = self.resolve_backend_handle(id).await?;
        self.fs.setattr(ctx, &handle, attrs).await
    }

    fn nfs_access_mask(bits: u32) -> AccessMask {
        let mut out = AccessMask::NONE;
        if bits & ACCESS4_READ != 0 {
            out |= AccessMask::READ;
        }
        if bits & ACCESS4_LOOKUP != 0 {
            out |= AccessMask::LOOKUP;
        }
        if bits & ACCESS4_MODIFY != 0 {
            out |= AccessMask::MODIFY;
        }
        if bits & ACCESS4_EXTEND != 0 {
            out |= AccessMask::EXTEND;
        }
        if bits & ACCESS4_DELETE != 0 {
            out |= AccessMask::DELETE;
        }
        if bits & ACCESS4_EXECUTE != 0 {
            out |= AccessMask::EXECUTE;
        }
        if bits & ACCESS4_XAREAD != 0 {
            out |= AccessMask::XATTR_READ;
        }
        if bits & ACCESS4_XAWRITE != 0 {
            out |= AccessMask::XATTR_WRITE;
        }
        if bits & ACCESS4_XALIST != 0 {
            out |= AccessMask::XATTR_LIST;
        }
        out
    }

    fn access_bits(mask: AccessMask) -> u32 {
        let mut out = 0;
        if mask.intersects(AccessMask::READ) {
            out |= ACCESS4_READ;
        }
        if mask.intersects(AccessMask::LOOKUP) {
            out |= ACCESS4_LOOKUP;
        }
        if mask.intersects(AccessMask::MODIFY) {
            out |= ACCESS4_MODIFY;
        }
        if mask.intersects(AccessMask::EXTEND) {
            out |= ACCESS4_EXTEND;
        }
        if mask.intersects(AccessMask::DELETE) {
            out |= ACCESS4_DELETE;
        }
        if mask.intersects(AccessMask::EXECUTE) {
            out |= ACCESS4_EXECUTE;
        }
        if mask.intersects(AccessMask::XATTR_READ) {
            out |= ACCESS4_XAREAD;
        }
        if mask.intersects(AccessMask::XATTR_WRITE) {
            out |= ACCESS4_XAWRITE;
        }
        if mask.intersects(AccessMask::XATTR_LIST) {
            out |= ACCESS4_XALIST;
        }
        out
    }

    fn committed_how(stability: WriteStability) -> u32 {
        match stability {
            WriteStability::Unstable => UNSTABLE4,
            WriteStability::DataSync => DATA_SYNC4,
            WriteStability::FileSync => FILE_SYNC4,
        }
    }

    fn validate_component_name(&self, name: &str) -> Result<(), NfsStat4> {
        if name.is_empty() {
            return Err(NfsStat4::Inval);
        }
        if name == "." || name == ".." {
            return Err(NfsStat4::Badname);
        }
        if name.contains('/') {
            return Err(NfsStat4::Badchar);
        }
        if name.len() > self.limits().max_name_bytes as usize {
            return Err(NfsStat4::Nametoolong);
        }
        Ok(())
    }

    // ===== Individual operation handlers =====
}

fn xdr_opaque_len(len: usize) -> usize {
    4 + len + xdr_pad(len)
}

fn hex_bytes(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn replay_fingerprint(cred: &OpaqueAuth, payload: &Bytes) -> Vec<u8> {
    let mut out = BytesMut::with_capacity(8 + cred.body.len() + payload.len());
    cred.flavor.encode(&mut out);
    encode_opaque(&mut out, &cred.body);
    out.extend_from_slice(payload);
    out.to_vec()
}

fn xdr_bitmap4_len(bitmap: &Bitmap4) -> usize {
    4 + (bitmap.0.len() * 4)
}

fn xdr_fattr4_len(fattr: &Fattr4) -> usize {
    xdr_bitmap4_len(&fattr.attrmask) + xdr_opaque_len(fattr.attr_vals.len())
}

fn readdir_dir_info_len(entry: &Entry4) -> usize {
    8 + xdr_opaque_len(entry.name.len())
}

fn readdir_entry_len(entry: &Entry4) -> usize {
    8 + xdr_opaque_len(entry.name.len()) + xdr_fattr4_len(&entry.attrs)
}

fn readdir_entry_list_item_len(entry: &Entry4) -> usize {
    4 + readdir_entry_len(entry)
}

fn readdir_resok_len(entries: &[Entry4], _eof: bool) -> usize {
    8 + entries
        .iter()
        .map(readdir_entry_list_item_len)
        .sum::<usize>()
        + 4
        + 4
}

#[cfg(test)]
mod tests;
