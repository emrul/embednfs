//! Example NFSv4.1 server backed by a local directory.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use embednfs::{
    AccessMask, Attrs, CommitSupport, CreateKind, CreateRequest, CreateResult, DelegationConfig,
    DirEntry, DirPage, FileSystem, FsCapabilities, FsError, FsLimits, FsResult, FsStats, HardLinks,
    NfsServer, NfsServerControl, NfsServerIdentity, ObjectType, ReadResult, RequestContext,
    SetAttrs, SetTime, Symlinks, Timestamp, WriteResult, WriteStability,
};
#[cfg(target_os = "linux")]
use embednfs::{XattrSetMode, Xattrs};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

const DEFAULT_ROOT: &str = "/tmp/embednfs-root";
const DEFAULT_LISTEN: &str = "0.0.0.0:2049";
const DEFAULT_DIRECTORY_DELEGATIONS: bool = false;
const DEFAULT_RECALL_TIMEOUT_MS: u64 = 5_000;
const CONTROL_RECALL_ROOT: &str = "RECALL /";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LocalHandle(PathBuf);

#[derive(Debug)]
struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    fn new(root: PathBuf) -> FsResult<Self> {
        std::fs::create_dir_all(&root).map_err(map_io_error)?;
        let root = root.canonicalize().map_err(map_io_error)?;
        Ok(Self { root })
    }

    fn full_path(&self, handle: &LocalHandle) -> FsResult<PathBuf> {
        reject_unsafe_relative(&handle.0)?;
        Ok(self.root.join(&handle.0))
    }

    fn child_handle(parent: &LocalHandle, name: &str) -> LocalHandle {
        LocalHandle(parent.0.join(name))
    }

    fn attrs_for(path: &Path) -> FsResult<Attrs> {
        let meta = std::fs::symlink_metadata(path).map_err(map_io_error)?;
        let object_type = if meta.file_type().is_dir() {
            ObjectType::Directory
        } else if meta.file_type().is_symlink() {
            ObjectType::Symlink
        } else {
            ObjectType::File
        };

        let fileid = meta.ino().max(1);
        let mut attrs = Attrs::new(object_type, fileid);
        attrs.change = meta.ctime() as u64 ^ meta.ctime_nsec() as u64 ^ meta.mtime() as u64;
        attrs.size = meta.len();
        attrs.space_used = meta.blocks().saturating_mul(512);
        attrs.link_count = meta.nlink().try_into().unwrap_or(u32::MAX);
        attrs.mode = meta.mode() & 0o7777;
        attrs.uid = meta.uid();
        attrs.gid = meta.gid();
        attrs.atime = Timestamp {
            seconds: meta.atime(),
            nanos: meta.atime_nsec().try_into().unwrap_or(0),
        };
        attrs.mtime = Timestamp {
            seconds: meta.mtime(),
            nanos: meta.mtime_nsec().try_into().unwrap_or(0),
        };
        attrs.ctime = Timestamp {
            seconds: meta.ctime(),
            nanos: meta.ctime_nsec().try_into().unwrap_or(0),
        };
        attrs.birthtime = attrs.ctime;
        Ok(attrs)
    }

    fn apply_setattrs(path: &Path, attrs: &SetAttrs) -> FsResult<()> {
        if let Some(size) = attrs.size {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(map_io_error)?;
            file.set_len(size).map_err(map_io_error)?;
        }
        if let Some(mode) = attrs.mode {
            let perms = std::fs::Permissions::from_mode(mode & 0o7777);
            std::fs::set_permissions(path, perms).map_err(map_io_error)?;
        }

        if attrs.uid.is_some() || attrs.gid.is_some() {
            let c_path = c_path(path)?;
            let uid = attrs.uid.map_or(libc::uid_t::MAX, libc::uid_t::from);
            let gid = attrs.gid.map_or(libc::gid_t::MAX, libc::gid_t::from);
            // SAFETY: c_path is a valid NUL-terminated path derived from an
            // OsStr without interior NUL bytes. uid/gid use libc's sentinel
            // all-ones value when the corresponding field is unchanged.
            let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
            if rc != 0 {
                return Err(map_io_error(std::io::Error::last_os_error()));
            }
        }

        if attrs.atime.is_some() || attrs.mtime.is_some() {
            let meta = std::fs::symlink_metadata(path).map_err(map_io_error)?;
            let times = [
                timespec_for(attrs.atime, meta.atime(), meta.atime_nsec()),
                timespec_for(attrs.mtime, meta.mtime(), meta.mtime_nsec()),
            ];
            let c_path = c_path(path)?;
            // SAFETY: c_path is a valid NUL-terminated path and times points
            // to two initialized timespec values for atime and mtime.
            let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
            if rc != 0 {
                return Err(map_io_error(std::io::Error::last_os_error()));
            }
        }

        let _ = (attrs.birthtime, attrs.archive, attrs.hidden, attrs.system);

        Ok(())
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    type Handle = LocalHandle;

    fn root(&self) -> Self::Handle {
        LocalHandle(PathBuf::new())
    }

    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities {
            symlinks: true,
            hard_links: true,
            xattrs: cfg!(target_os = "linux"),
            explicit_sync: true,
            case_sensitive: true,
            case_preserving: true,
        }
    }

    fn limits(&self) -> FsLimits {
        FsLimits {
            max_name_bytes: 255,
            max_file_size: u64::MAX / 2,
            ..FsLimits::default()
        }
    }

    async fn statfs(&self, _ctx: &RequestContext, _handle: &Self::Handle) -> FsResult<FsStats> {
        Ok(FsStats::default())
    }

    async fn getattr(&self, _ctx: &RequestContext, handle: &Self::Handle) -> FsResult<Attrs> {
        Self::attrs_for(&self.full_path(handle)?)
    }

    async fn access(
        &self,
        _ctx: &RequestContext,
        handle: &Self::Handle,
        requested: AccessMask,
    ) -> FsResult<AccessMask> {
        let _ = std::fs::symlink_metadata(self.full_path(handle)?).map_err(map_io_error)?;
        Ok(requested)
    }

    async fn lookup(
        &self,
        _ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<Self::Handle> {
        let child = Self::child_handle(parent, name);
        let _ = std::fs::symlink_metadata(self.full_path(&child)?).map_err(map_io_error)?;
        Ok(child)
    }

    async fn parent(
        &self,
        _ctx: &RequestContext,
        dir: &Self::Handle,
    ) -> FsResult<Option<Self::Handle>> {
        if dir.0.as_os_str().is_empty() {
            return Ok(None);
        }
        Ok(Some(LocalHandle(
            dir.0
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf(),
        )))
    }

    async fn readdir(
        &self,
        _ctx: &RequestContext,
        dir: &Self::Handle,
        cookie: u64,
        max_entries: u32,
        with_attrs: bool,
    ) -> FsResult<DirPage<Self::Handle>> {
        let mut children = Vec::new();
        for entry in std::fs::read_dir(self.full_path(dir)?).map_err(map_io_error)? {
            let entry = entry.map_err(map_io_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| FsError::InvalidInput)?;
            children.push(name);
        }
        children.sort();

        let start = usize::try_from(cookie).map_err(|_| FsError::InvalidInput)?;
        let limit = max_entries as usize;
        let mut entries = Vec::new();
        for (idx, name) in children.iter().enumerate().skip(start).take(limit) {
            let handle = Self::child_handle(dir, name);
            let attrs = if with_attrs {
                Some(Self::attrs_for(&self.full_path(&handle)?)?)
            } else {
                None
            };
            entries.push(DirEntry {
                name: name.clone(),
                handle,
                cookie: (idx + 1) as u64,
                attrs,
            });
        }
        Ok(DirPage {
            eof: start.saturating_add(entries.len()) >= children.len(),
            entries,
        })
    }

    async fn read(
        &self,
        _ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> FsResult<ReadResult> {
        let path = self.full_path(handle)?;
        let mut file = std::fs::File::open(path).map_err(map_io_error)?;
        let _ = file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
        let mut buf = vec![0; count as usize];
        let read = file.read(&mut buf).map_err(map_io_error)?;
        buf.truncate(read);
        let size = file.metadata().map_err(map_io_error)?.len();
        Ok(ReadResult {
            data: Bytes::from(buf),
            eof: offset.saturating_add(read as u64) >= size,
        })
    }

    async fn write(
        &self,
        _ctx: &RequestContext,
        handle: &Self::Handle,
        offset: u64,
        data: Bytes,
        requested: WriteStability,
    ) -> FsResult<WriteResult> {
        let path = self.full_path(handle)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(map_io_error)?;
        let _ = file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
        file.write_all(&data).map_err(map_io_error)?;
        match requested {
            WriteStability::Unstable => {}
            WriteStability::DataSync => file.sync_data().map_err(map_io_error)?,
            WriteStability::FileSync => file.sync_all().map_err(map_io_error)?,
        }
        Ok(WriteResult {
            written: data.len().try_into().unwrap_or(u32::MAX),
            stability: requested,
        })
    }

    async fn create(
        &self,
        _ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
        req: CreateRequest,
    ) -> FsResult<CreateResult<Self::Handle>> {
        let handle = Self::child_handle(parent, name);
        let path = self.full_path(&handle)?;
        match req.kind {
            CreateKind::File => {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(map_io_error)?;
            }
            CreateKind::Directory => std::fs::create_dir(&path).map_err(map_io_error)?,
        }
        Self::apply_setattrs(&path, &req.attrs)?;
        Ok(CreateResult {
            attrs: Self::attrs_for(&path)?,
            handle,
        })
    }

    async fn remove(
        &self,
        _ctx: &RequestContext,
        parent: &Self::Handle,
        name: &str,
    ) -> FsResult<()> {
        let handle = Self::child_handle(parent, name);
        let path = self.full_path(&handle)?;
        let meta = std::fs::symlink_metadata(&path).map_err(map_io_error)?;
        if meta.is_dir() {
            std::fs::remove_dir(path).map_err(map_io_error)
        } else {
            std::fs::remove_file(path).map_err(map_io_error)
        }
    }

    async fn rename(
        &self,
        _ctx: &RequestContext,
        from_dir: &Self::Handle,
        from_name: &str,
        to_dir: &Self::Handle,
        to_name: &str,
    ) -> FsResult<()> {
        let from = self.full_path(&Self::child_handle(from_dir, from_name))?;
        let to = self.full_path(&Self::child_handle(to_dir, to_name))?;
        std::fs::rename(from, to).map_err(map_io_error)
    }

    async fn setattr(
        &self,
        _ctx: &RequestContext,
        handle: &Self::Handle,
        attrs: &SetAttrs,
    ) -> FsResult<Attrs> {
        let path = self.full_path(handle)?;
        Self::apply_setattrs(&path, attrs)?;
        Self::attrs_for(&path)
    }

    fn symlinks(&self) -> Option<&dyn Symlinks<Self::Handle>> {
        Some(self)
    }

    fn hard_links(&self) -> Option<&dyn HardLinks<Self::Handle>> {
        Some(self)
    }

    #[cfg(target_os = "linux")]
    fn xattrs(&self) -> Option<&dyn Xattrs<Self::Handle>> {
        Some(self)
    }

    fn commit_support(&self) -> Option<&dyn CommitSupport<Self::Handle>> {
        Some(self)
    }
}

#[async_trait]
impl Symlinks<LocalHandle> for LocalFs {
    async fn create_symlink(
        &self,
        _ctx: &RequestContext,
        parent: &LocalHandle,
        name: &str,
        target: &str,
        attrs: &SetAttrs,
    ) -> FsResult<CreateResult<LocalHandle>> {
        let handle = LocalFs::child_handle(parent, name);
        let path = self.full_path(&handle)?;
        std::os::unix::fs::symlink(target, &path).map_err(map_io_error)?;
        Self::apply_setattrs(&path, attrs)?;
        Ok(CreateResult {
            attrs: Self::attrs_for(&path)?,
            handle,
        })
    }

    async fn readlink(&self, _ctx: &RequestContext, handle: &LocalHandle) -> FsResult<String> {
        std::fs::read_link(self.full_path(handle)?)
            .map_err(map_io_error)?
            .into_os_string()
            .into_string()
            .map_err(|_| FsError::InvalidInput)
    }
}

#[async_trait]
impl HardLinks<LocalHandle> for LocalFs {
    async fn link(
        &self,
        _ctx: &RequestContext,
        source: &LocalHandle,
        parent: &LocalHandle,
        name: &str,
    ) -> FsResult<()> {
        let source = self.full_path(source)?;
        let dest = self.full_path(&LocalFs::child_handle(parent, name))?;
        std::fs::hard_link(source, dest).map_err(map_io_error)
    }
}

#[async_trait]
impl CommitSupport<LocalHandle> for LocalFs {
    async fn commit(
        &self,
        _ctx: &RequestContext,
        handle: &LocalHandle,
        _offset: u64,
        _count: u32,
    ) -> FsResult<()> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(self.full_path(handle)?)
            .map_err(map_io_error)?;
        file.sync_all().map_err(map_io_error)
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl Xattrs<LocalHandle> for LocalFs {
    async fn list_xattrs(
        &self,
        _ctx: &RequestContext,
        handle: &LocalHandle,
    ) -> FsResult<Vec<String>> {
        let path = self.full_path(handle)?;
        let c_path = c_path(&path)?;
        // SAFETY: c_path is a valid NUL-terminated path.
        let size = unsafe { libc::listxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(map_io_error(std::io::Error::last_os_error()));
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf is valid for size bytes and c_path is NUL-terminated.
        let written =
            unsafe { libc::listxattr(c_path.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if written < 0 {
            return Err(map_io_error(std::io::Error::last_os_error()));
        }
        buf.truncate(written as usize);
        xattr_names(&buf)
    }

    async fn get_xattr(
        &self,
        _ctx: &RequestContext,
        handle: &LocalHandle,
        name: &str,
    ) -> FsResult<Bytes> {
        let path = self.full_path(handle)?;
        let c_path = c_path(&path)?;
        let c_name = c_xattr_name(name)?;
        // SAFETY: c_path and c_name are valid NUL-terminated strings.
        let size =
            unsafe { libc::getxattr(c_path.as_ptr(), c_name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(map_xattr_error());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf is valid for size bytes; c_path and c_name are NUL-terminated.
        let read = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if read < 0 {
            return Err(map_xattr_error());
        }
        buf.truncate(read as usize);
        Ok(Bytes::from(buf))
    }

    async fn set_xattr(
        &self,
        _ctx: &RequestContext,
        handle: &LocalHandle,
        name: &str,
        value: Bytes,
        mode: XattrSetMode,
    ) -> FsResult<()> {
        let path = self.full_path(handle)?;
        let c_path = c_path(&path)?;
        let c_name = c_xattr_name(name)?;
        let flags = match mode {
            XattrSetMode::CreateOrReplace => 0,
            XattrSetMode::CreateOnly => libc::XATTR_CREATE,
            XattrSetMode::ReplaceOnly => libc::XATTR_REPLACE,
        };
        // SAFETY: c_path and c_name are valid NUL-terminated strings, and
        // value.as_ptr() is valid for value.len() bytes.
        let rc = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                flags,
            )
        };
        if rc != 0 {
            return Err(map_xattr_error());
        }
        Ok(())
    }

    async fn remove_xattr(
        &self,
        _ctx: &RequestContext,
        handle: &LocalHandle,
        name: &str,
    ) -> FsResult<()> {
        let path = self.full_path(handle)?;
        let c_path = c_path(&path)?;
        let c_name = c_xattr_name(name)?;
        // SAFETY: c_path and c_name are valid NUL-terminated strings.
        let rc = unsafe { libc::removexattr(c_path.as_ptr(), c_name.as_ptr()) };
        if rc != 0 {
            return Err(map_xattr_error());
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let root = std::env::var_os("EMBEDNFS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
    let listen = std::env::var("EMBEDNFS_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let control_listen = env_optional("EMBEDNFS_CONTROL_LISTEN");
    let delegation_config = delegation_config_from_env()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;

    let fs = LocalFs::new(root)
        .map_err(|err| std::io::Error::other(format!("failed to initialize local fs: {err}")))?;
    let server_identity = server_identity_from_env(default_server_identity(&fs.root, &listen))
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    info!(
        "serving {} on {listen}; directory_delegations={}",
        fs.root.display(),
        delegation_config.directory_delegations
    );
    let server = NfsServer::builder(fs)
        .delegation_config(delegation_config)
        .server_identity(server_identity)
        .build();

    if let Some(control_addr) = control_listen {
        let control_listener = TcpListener::bind(&control_addr).await?;
        let control = server.control_handle();
        std::mem::drop(tokio::spawn(async move {
            if let Err(err) = serve_control(control_listener, control).await {
                error!("control listener failed: {err}");
            }
        }));
    }

    server.listen(&listen).await
}

async fn serve_control(
    listener: TcpListener,
    control: NfsServerControl<LocalHandle>,
) -> std::io::Result<()> {
    let local_addr = listener.local_addr()?;
    info!("control listener accepting commands on {local_addr}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let control = control.clone();
        std::mem::drop(tokio::spawn(async move {
            if let Err(err) = handle_control_connection(stream, control).await {
                debug!("control connection error from {peer}: {err}");
            }
        }));
    }
}

async fn handle_control_connection(
    stream: TcpStream,
    control: NfsServerControl<LocalHandle>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut command = String::new();
    if reader.read_line(&mut command).await? == 0 {
        return Ok(());
    }

    let mut stream = reader.into_inner();
    match command.trim() {
        CONTROL_RECALL_ROOT => match control.recall_directory(&LocalHandle(PathBuf::new())).await {
            Ok(()) => stream.write_all(b"OK\n").await?,
            Err(err) => {
                let response = format!("ERR {err}\n");
                stream.write_all(response.as_bytes()).await?;
            }
        },
        _ => stream.write_all(b"ERR unknown command\n").await?,
    }
    stream.shutdown().await
}

fn delegation_config_from_env() -> Result<DelegationConfig, String> {
    let mut config = DelegationConfig {
        directory_delegations: env_bool(
            "EMBEDNFS_DIRECTORY_DELEGATIONS",
            DEFAULT_DIRECTORY_DELEGATIONS,
        )?,
        ..DelegationConfig::default()
    };
    let recall_ms = env_u64("EMBEDNFS_RECALL_TIMEOUT_MS", DEFAULT_RECALL_TIMEOUT_MS)?;
    config.recall_timeout = std::time::Duration::from_millis(recall_ms);
    Ok(config)
}

fn default_server_identity(root: &Path, listen: &str) -> NfsServerIdentity {
    let token = format!(
        "embednfsd:{:016x}",
        stable_identity_hash(root.as_os_str().as_bytes(), listen.as_bytes())
    );
    NfsServerIdentity::new(token.clone(), 0, token)
}

fn stable_identity_hash(root: &[u8], listen: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in listen.iter().chain([0].iter()).chain(root.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn server_identity_from_env(
    default_identity: NfsServerIdentity,
) -> Result<NfsServerIdentity, String> {
    let mut identity = default_identity;
    if let Some(owner_major_id) = env_optional_bytes("EMBEDNFS_SERVER_OWNER_MAJOR_ID") {
        identity = identity.with_owner_major_id(owner_major_id);
    }
    if let Some(owner_minor_id) = env_optional_u64("EMBEDNFS_SERVER_OWNER_MINOR_ID")? {
        identity = identity.with_owner_minor_id(owner_minor_id);
    }
    if let Some(scope) = env_optional_bytes("EMBEDNFS_SERVER_SCOPE") {
        identity = identity.with_scope(scope);
    }
    Ok(identity)
}

fn env_optional(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn env_optional_bytes(name: &str) -> Option<Bytes> {
    std::env::var_os(name).and_then(|value| {
        let bytes = value.as_os_str().as_bytes();
        if bytes.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(Bytes::copy_from_slice(bytes))
        }
    })
}

fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    match value.to_string_lossy().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        value => Err(format!("{name} must be a boolean, got {value:?}")),
    }
}

fn env_optional_u64(name: &str) -> Result<Option<u64>, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    if value.trim().is_empty() {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|err| format!("{name} must be an unsigned integer: {err}"))
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    value
        .to_string_lossy()
        .parse()
        .map_err(|err| format!("{name} must be an unsigned integer: {err}"))
}

fn reject_unsafe_relative(path: &Path) -> FsResult<()> {
    if path.is_absolute() {
        return Err(FsError::BadHandle);
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(FsError::BadHandle);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_server_identity_is_stable_for_same_root_and_listen() {
        let first = default_server_identity(Path::new("/tmp/embednfs-root"), "127.0.0.1:2049");
        let second = default_server_identity(Path::new("/tmp/embednfs-root"), "127.0.0.1:2049");

        assert_eq!(first.owner_major_id(), second.owner_major_id());
        assert_eq!(first.owner_minor_id(), second.owner_minor_id());
        assert_eq!(first.scope(), second.scope());
    }

    #[test]
    fn default_server_identity_differs_for_different_listen_addresses() {
        let first = default_server_identity(Path::new("/tmp/embednfs-root"), "127.0.0.1:2049");
        let second = default_server_identity(Path::new("/tmp/embednfs-root"), "127.0.0.1:2050");

        assert_ne!(first.owner_major_id(), second.owner_major_id());
        assert_ne!(first.scope(), second.scope());
    }
}

fn c_path(path: &Path) -> FsResult<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| FsError::InvalidInput)
}

#[cfg(target_os = "linux")]
fn xattr_names(buf: &[u8]) -> FsResult<Vec<String>> {
    let mut names = Vec::new();
    for raw in buf.split(|b| *b == 0).filter(|raw| !raw.is_empty()) {
        let name = std::str::from_utf8(raw).map_err(|_| FsError::InvalidInput)?;
        if let Some(user_name) = name.strip_prefix("user.") {
            names.push(user_name.to_string());
        }
    }
    Ok(names)
}

#[cfg(target_os = "linux")]
fn c_xattr_name(name: &str) -> FsResult<std::ffi::CString> {
    let storage_name = if has_linux_xattr_namespace(name) {
        name.to_string()
    } else {
        format!("user.{name}")
    };
    std::ffi::CString::new(storage_name).map_err(|_| FsError::InvalidInput)
}

#[cfg(target_os = "linux")]
fn has_linux_xattr_namespace(name: &str) -> bool {
    name.starts_with("user.")
        || name.starts_with("trusted.")
        || name.starts_with("security.")
        || name.starts_with("system.")
}

#[cfg(target_os = "linux")]
fn map_xattr_error() -> FsError {
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENODATA) => FsError::NotFound,
        Some(libc::EOPNOTSUPP) => FsError::Unsupported,
        _ => map_io_error(err),
    }
}

fn timespec_for(update: Option<SetTime>, current_sec: i64, current_nsec: i64) -> libc::timespec {
    let timestamp = match update {
        Some(SetTime::Client(timestamp)) => timestamp,
        Some(SetTime::ServerNow) => Timestamp::now(),
        None => Timestamp {
            seconds: current_sec,
            nanos: current_nsec.try_into().unwrap_or(0),
        },
    };
    libc::timespec {
        tv_sec: timestamp.seconds,
        tv_nsec: timestamp.nanos.into(),
    }
}

fn map_io_error(err: std::io::Error) -> FsError {
    match err.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        std::io::ErrorKind::PermissionDenied => FsError::PermissionDenied,
        std::io::ErrorKind::AlreadyExists => FsError::AlreadyExists,
        std::io::ErrorKind::InvalidInput => FsError::InvalidInput,
        std::io::ErrorKind::NotADirectory => FsError::NotDirectory,
        std::io::ErrorKind::IsADirectory => FsError::IsDirectory,
        std::io::ErrorKind::DirectoryNotEmpty => FsError::NotEmpty,
        std::io::ErrorKind::ReadOnlyFilesystem => FsError::ReadOnly,
        std::io::ErrorKind::FileTooLarge => FsError::FileTooLarge,
        std::io::ErrorKind::StorageFull => FsError::NoSpace,
        _ => FsError::Io,
    }
}
