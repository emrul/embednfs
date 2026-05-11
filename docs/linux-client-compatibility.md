# Linux NFSv4.1 Client Compatibility Design Notes

This note identifies where `embednfs` may differ from Linux NFS client expectations. It is a planning document, not a claim of validated Linux support.

## Sources

- RFC 8881, NFSv4.1: <https://www.ietf.org/rfc/rfc8881.html>
- RFC 5531, ONC RPC v2: <https://www.rfc-editor.org/rfc/rfc5531>
- Linux kernel NFSv4.1 server operation/status reference: <https://docs.kernel.org/filesystems/nfs/nfs41-server.html>
- Linux NFS client mount behavior: <https://man7.org/linux/man-pages/man5/nfs.5.html>
- Linux client source, current upstream:
  - `fs/nfs/nfs4client.c`: <https://github.com/torvalds/linux/blob/master/fs/nfs/nfs4client.c>
  - `fs/nfs/nfs4proc.c`: <https://github.com/torvalds/linux/blob/master/fs/nfs/nfs4proc.c>
  - `fs/nfs/nfs4xdr.c`: <https://github.com/torvalds/linux/blob/master/fs/nfs/nfs4xdr.c>

The local implementation points referenced below are current as of this repository state:

- COMPOUND/session gate and op dispatch: `crates/embednfs/src/server/compound.rs`
- client/session state: `crates/embednfs/src/session/clients.rs`
- attributes: `crates/embednfs/src/attrs.rs`
- file I/O/open/stateid validation: `crates/embednfs/src/server/ops/file.rs`
- directory and security-info ops: `crates/embednfs/src/server/ops/directory.rs`
- locking: `crates/embednfs/src/server/ops/locking.rs`

## Mount Baseline

Use an explicit NFSv4.1 mount. Linux will otherwise try newer minor versions first, and this library intentionally rejects `minorversion != 1`.

```bash
mount -t nfs4 -o vers=4.1,proto=tcp,port=2049,sec=sys 127.0.0.1:/ /mnt/embednfs
```

Useful debug variants:

```bash
mount -t nfs4 -o vers=4.1,proto=tcp,port=2049,sec=sys,soft,timeo=10,retrans=2 127.0.0.1:/ /mnt/embednfs
nfsstat -m
dmesg -w
```

The Linux `nfs(5)` behavior to account for:

- `vers=4.1` is equivalent to `vers=4,minorversion=1`.
- NFSv4 defaults to TCP.
- If `port` is omitted, Linux uses TCP port 2049 for NFSv4 without rpcbind. A custom port must be explicit.
- If `vers` is omitted, Linux may try 4.2 before 4.1, which will correctly fail against `embednfs`.

## Current Compatibility Surface

`embednfs` already implements the core operations Linux needs for a basic NFSv4.1 mount and file workflow:

- sessions: `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`, `BIND_CONN_TO_SESSION`, `DESTROY_SESSION`, `DESTROY_CLIENTID`
- namespace: `PUTROOTFH`, `PUTFH`, `GETFH`, `LOOKUP`, `LOOKUPP`, `SAVEFH`, `RESTOREFH`
- metadata: `GETATTR`, `SETATTR`, `ACCESS`, `VERIFY`, `NVERIFY`
- file lifecycle: `OPEN`, `CLOSE`, `OPEN_DOWNGRADE`, `READ`, `WRITE`, `COMMIT`
- directories and namespace mutation: `READDIR`, `CREATE`, `REMOVE`, `RENAME`
- locking: `LOCK`, `LOCKT`, `LOCKU`
- security probing: `SECINFO`, `SECINFO_NO_NAME`
- recovery/state cleanup: `RECLAIM_COMPLETE`, `TEST_STATEID`, `FREE_STATEID`

The highest-value Linux path to validate first is therefore not broad protocol coverage. It is whether Linux accepts this server's exact session negotiation, auth flavor advertisement, root filehandle probing, and advertised attributes.

## Likely Breakage Points

### 1. Mandatory-but-practically-optional v4.1 Operations

Linux's NFSv4.1 server documentation lists `BACKCHANNEL_CTL` and `SET_SSV` as mandatory-to-implement operations, while also noting that Linux nfsd itself does not implement practical SSV support and returns encryption-algorithm unsupported for SSV negotiation. `embednfs` currently decodes these operations but returns `NFS4ERR_NOTSUPP`.

Risk:

- `SET_SSV`: probably low for `sec=sys` mounts. Linux documentation says current clients do not request GSS on the backchannel, and common Linux server behavior treats SSV as not deployed.
- `BACKCHANNEL_CTL`: medium. If the Linux client sends it during callback setup, `NFS4ERR_NOTSUPP` may fail mount or disable callback-dependent features. If it never sends it for `sec=sys`, current behavior is acceptable.

Design direction:

- Trace a real Linux mount before implementing callbacks.
- If Linux sends `BACKCHANNEL_CTL`, add a minimal success response that records no callback capability rather than building full callback RPC machinery.
- Keep `SET_SSV` unsupported unless a trace shows Linux asks for it under a supported mount mode. If implemented as a compatibility shim, follow Linux nfsd's documented approach and fail SSV negotiation with `NFS4ERR_ENCR_ALG_UNSUPP`, not generic `NOTSUPP`, where RFC context requires it.

### 2. Session Negotiation Limits

Linux uses NFSv4.1 sessions and performs `EXCHANGE_ID` then `CREATE_SESSION` before normal mounted I/O. `embednfs` enforces that non-session operations after session creation start with `SEQUENCE`, and negotiates fore-channel limits by clamping the client's requested values.

Risk:

- `CREATE_SESSION` currently sets `maxoperations` to `min(client.maxoperations, MAX_FORE_CHAN_SLOTS)`. The name implies slot count, not compound operation count. If `MAX_FORE_CHAN_SLOTS` is small, Linux may limit COMPOUND construction unnecessarily, or reject a response that violates its requested minimums.
- Backchannel attributes are synthetic and small. Linux nfsd ignores backchannel attributes, but Linux client behavior against third-party servers should be traced.
- Session trunking identity must remain stable. `server_owner` and `server_scope` are fixed, which is good for localhost, but multiple independent `embednfs` instances on different ports may look like the same trunkable server to Linux.

Design direction:

- Confirm negotiated `CREATE_SESSION` response with packet capture or tracepoints.
- Introduce explicit constants for `maxoperations` vs `maxrequests` if they are currently conflated.
- Consider deriving `server_scope` or `server_owner` from the listener/export identity when supporting multiple simultaneous local servers.

### 3. Root Filehandle and Export Path Semantics

Linux mounts NFSv4 as a pseudo-filesystem rooted at the server path. For `127.0.0.1:/`, the client probes the root filehandle and then asks for server capabilities and attributes.

`embednfs` maps both `PUTROOTFH` and `PUTPUBFH` to the backend root. That is a good fit for a single-root localhost export.

Risk:

- Non-root export paths such as `127.0.0.1:/some/path` are not modeled. Linux may issue component `LOOKUP`s after `PUTROOTFH`; this should work only if the backend root contains those path components.
- Linux may request referral/migration attributes such as `fs_locations`; `embednfs` does not advertise or encode them. This should be acceptable for a single local export, but traces should confirm Linux treats absence as no referral rather than mount failure.

Design direction:

- Document `/` as the only supported export path for Linux until explicit pseudo-root/export mapping is added.
- Add a Linux mount test that attempts only `server:/` first.

### 4. Attribute Bitmap Differences

Linux's client source uses standard GETATTR bitmaps including type, change, size, fsid, fileid, mode, nlink, owner, owner_group, rawdev, space_used, access/create/metadata/modify times, and mounted_on_fileid. It also probes fsinfo/pathconf/statfs data such as max read/write, max name, lease time, time delta, space totals, and file totals.

`embednfs` already encodes the main Linux-required set. It also includes Apple-oriented attributes such as archive, hidden, system, named attributes, and time_create.

Risk:

- Linux may ask for attributes not currently advertised, especially newer attributes such as xattr support, change attribute type, layout types, layout block size, clone block size, or security labels. `embednfs` currently returns the subset it can encode for `GETATTR`; this is likely fine, but must be validated.
- `supported_attrs` advertises `FATTR4_NAMED_ATTR` only when `FsCapabilities::xattrs` is true. Linux native xattr behavior uses newer NFS extensions rather than macOS `OPENATTR` named-attribute directories. Do not assume Linux user xattrs work just because macOS named attrs work.
- Directory cache behavior depends on directory `mtime` and change attributes. Linux aggressively caches positive and negative LOOKUP results based on parent directory attributes. Backend implementations must update directory `change` and `mtime` on create/remove/rename/link.

Design direction:

- Capture the exact GETATTR bitmaps during mount and basic `stat`, `ls`, `touch`, `cp`, `mv`, `rm`.
- Add compatibility tests around directory change/mtime invalidation because stale Linux dcache behavior will look like client bugs but usually means server metadata is not advancing.
- Treat Linux xattrs as a separate feature from macOS named attributes. Plan no Linux xattr support until the specific operation path is identified.

Current validation result:

- Linux `setfattr` on a mounted `embednfs` export fails locally with `EOPNOTSUPP`.
- Enabling the server's NFSv4.1 `OPENATTR`/named-attribute backend does not change this; debug COMPOUND logs show no `OPENATTR` or xattr-like v4.1 operation is sent for `setfattr`.
- Linux user xattrs use the NFSv4.2 extended-attribute operations from RFC 8276 (`GETXATTR`, `SETXATTR`, `LISTXATTR`, `REMOVEXATTR`), which are outside this project's current strict NFSv4.1 scope.

### 5. Owner/Group Identity Mapping

Linux NFSv4 traditionally uses owner strings and idmapping, but can also interact with raw numeric owner strings. `embednfs` defaults to numeric strings via `NumericIdMapper` and decodes numeric or `number@domain` values in `SETATTR`.

Risk:

- Some Linux configurations may send names like `user@domain` instead of numeric strings for `SETATTR owner` and `owner_group`. Current decode ignores non-numeric names, which can make `chown` appear to succeed without changing ownership if the backend returns success for the rest of the attributes.
- Linux client source has fallback behavior around `NFS4ERR_BADOWNER` for servers that do not accept raw uid/gid. Silent ignore is harder to diagnose than a clear error.

Design direction:

- For Linux support, either document `nfs4_disable_idmapping=Y`/numeric-id expectations or add a configurable reverse id mapper.
- If owner/group strings cannot be parsed, return `NFS4ERR_BADOWNER` for explicit owner/group changes rather than silently ignoring them.

### 6. Authentication Flavor Probing

`embednfs` accepts `AUTH_NONE` and `AUTH_SYS`, parses `AUTH_SYS`, and advertises both flavors in `SECINFO` and `SECINFO_NO_NAME`.

Risk:

- Linux mounts should be forced to `sec=sys` during initial validation. If the client chooses `AUTH_NONE`, backend policy may see anonymous auth and reject operations unexpectedly.
- `SECINFO` currently returns a static list without checking object/path policy. That is acceptable for a single-security export but will be insufficient if per-subtree security appears later.

Design direction:

- Use `sec=sys` in all Linux validation commands.
- Consider advertising only `AUTH_SYS` by default unless `AUTH_NONE` has a real use case.

### 7. Locking and Stateid Semantics

Linux uses integrated NFSv4 locking for v4.1, not the NLM sideband used by older NFS versions. `embednfs` has in-process open/lock/share-deny state.

Risk:

- The implementation is server-local and memory-only. Client reboot recovery, lease expiry, and reclaim behavior may be enough for localhost tests but needs Linux-specific validation with interrupted mounts and remounts.
- Linux maps some NFSv4 state errors into client recovery workflows. Returning the wrong state error can trigger recovery loops rather than a clean application error.

Design direction:

- Validate `flock`, POSIX byte-range locks, lock conflict, client process death, unmount/remount, and server restart.
- Add traces for `BAD_STATEID`, `STALE_STATEID`, `OLD_STATEID`, `EXPIRED`, and `STALE_CLIENTID`.

### 8. Unsupported Optional Features

`embednfs` intentionally returns `NFS4ERR_NOTSUPP` for pNFS layouts, directory delegation, device info/list, and wanted delegations. That should be fine for a non-pNFS localhost server.

Risk:

- Linux may probe optional features. Correct `NOTSUPP` is acceptable; malformed decoding or wrong COMPOUND positioning is not.
- Delegation-related no-op success for `DELEGPURGE` and `DELEGRETURN` should be revisited. If the server never grants delegations, success is usually harmless, but strict clients may expect `BAD_STATEID` or `NOTSUPP` for invalid delegation state.

Design direction:

- Keep pNFS and delegation support out of scope for first Linux support.
- Prefer explicit, RFC-consistent negative responses over no-op success where state-bearing arguments are invalid.

## Proposed Validation Matrix

The executable first-pass harness is `scripts/smoke-linux-nfs41.sh`. Run it inside the Linux VM:

```bash
./scripts/smoke-linux-nfs41.sh
```

It starts `embednfsd`, mounts `127.0.0.1:/` with NFSv4.1, writes per-probe logs under `/tmp/embednfs-linux-smoke-*`, and emits a tab-separated summary.

### Phase 1: Mount and Metadata

- `mount -t nfs4 -o vers=4.1,proto=tcp,port=2049,sec=sys 127.0.0.1:/ /mnt/embednfs`
- `stat /mnt/embednfs`
- `ls -la /mnt/embednfs`
- `find /mnt/embednfs -maxdepth 2 -ls`
- verify no repeated recovery loop in `dmesg`

Expected result: mount succeeds, root attrs are stable, no `BADSESSION`, `SEQ_MISORDERED`, `WRONGSEC`, or `ATTRNOTSUPP` loops.

### Phase 2: Basic File Workflows

- `touch`, `cat`, `cp`, `dd`, `truncate`, `mv`, `rm`
- create/remove directories
- symlink and hardlink if backend capabilities advertise them
- large reads/writes at the negotiated max read/write sizes

Expected result: data integrity holds and directory listings update without `lookupcache=none`.

### Phase 3: Linux-Specific Metadata

- `chmod`, `chown`, `chgrp`, `utimensat` via `touch -d`
- `stat -c '%i %F %s %U %G %a %X %Y %Z %W'`
- test with numeric-id and idmapped Linux configurations

Expected result: unsupported owner formats fail clearly or are mapped correctly; times and mode round-trip.

### Phase 4: Locking and Recovery

- POSIX byte-range locks from two processes.
- `flock` if the Linux client maps it through NFSv4 locks for this mount.
- kill locking process and verify unlock behavior.
- restart the server while mounted and observe client recovery.
- unmount/remount with open files if practical.

Expected result: no indefinite recovery loops; state errors trigger expected Linux recovery or clear application errors.

### Phase 5: Optional Feature Probes

- inspect traffic for `BACKCHANNEL_CTL`, layout ops, `GETDEVICEINFO`, `WANT_DELEGATION`, and xattr paths.
- run with `nconnect`/`max_connect` options only after single-connection behavior is stable.

Expected result: optional probes either do not occur or receive RFC-consistent responses that Linux tolerates.

## Initial Code Change Candidates

These are candidates only; implement them after a Linux mount trace confirms the actual failure mode.

1. Return `NFS4ERR_ENCR_ALG_UNSUPP` in the SSV negotiation path when RFC/Linux behavior calls for it, instead of generic `NOTSUPP`.
2. Implement a minimal `BACKCHANNEL_CTL` compatibility response if Linux sends it during normal `sec=sys` mounts.
3. Separate session `maxoperations` from slot-count constants and verify Linux accepts the negotiated values.
4. Make owner/group `SETATTR` parsing fail with `NFS4ERR_BADOWNER` when an explicit owner/group change cannot be mapped.
5. Add a Linux kernel-client smoke script gated on root privileges, separate from the no-root RPC integration tests.
6. Add docs stating Linux support requires `vers=4.1,proto=tcp,sec=sys` and currently supports only the `/` export path.

## Open Questions

- Does Linux send `BACKCHANNEL_CTL` to this server during a normal `sec=sys` v4.1 mount?
- Does Linux tolerate the current `SECINFO` order of `AUTH_SYS` then `AUTH_NONE`, or should `AUTH_NONE` be removed from the default advertised list?
- Which exact fsinfo/pathconf attributes does Linux require to set sane `rsize`, `wsize`, name length, and cache behavior?
- Does Linux ever issue `OPENATTR` against this server, or does Linux xattr support require newer non-OPENATTR NFS operations? Current finding: Linux `setfattr` does not issue `OPENATTR`; user xattrs require NFSv4.2 RFC 8276 operations.
- Are current directory `change` and `mtime` updates sufficient for Linux dentry-cache invalidation across all backend implementations?
- Should `server_owner`/`server_scope` include per-instance identity to avoid accidental Linux session trunking across multiple localhost servers?
