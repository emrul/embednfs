# NFSv4 Delegations

## Summary

Add NFSv4 delegation support to embednfs in a way that improves Linux
directory-cache coherence without changing the existing macOS client path.

The first implementation should support **NFSv4.1 read-only directory
delegations** only. This is the feature portal-sync needs for sub-second
namespace freshness without `actimeo=0` metadata storms: a Linux client can keep
directory and negative-lookup cache entries while the server recalls the
delegation when the directory changes.

Do not implement file read/write delegations in the first pass. Do not change
the NFSv4.0/macOS OPEN behavior.

## Implementation Status

This design has been implemented through the first Linux kernel validation
gate:

- Directory delegation support remains disabled by default and opt-in through
  `DelegationConfig`.
- `GET_DIR_DELEGATION` grants read-only directory delegations to v4.1+ clients
  with a valid backchannel.
- `CB_SEQUENCE` + `CB_RECALL` callbacks are sent over the negotiated v4.1
  backchannel.
- `DELEGRETURN`, `TEST_STATEID`, `FREE_STATEID`, client expiry, and client
  destruction are delegation-state aware.
- NFS-originated directory mutations recall other delegation holders, but they
  do not recall the mutating client's own delegation when the mutation is sent
  on a session associated with that clientid, per RFC 8881 Section 10.9.2.
- A cloneable server control handle can recall directory delegations before an
  embedder applies external namespace changes.
- Lima `podman` kernel validation on Fedora kernel `7.0.9-105.fc43.aarch64`
  passed the strict protocol and product gates with real
  `GET_DIR_DELEGATION`, `CB_RECALL`, and `DELEGRETURN` traffic.

## Starting State Before This Work

embednfs already has partial protocol surface for delegations:

- `embednfs-proto` defines `OpenDelegation4`, `GetDirDelegationRes4`,
  `DelegReturnArgs4`, `OP_GET_DIR_DELEGATION`, `OP_WANT_DELEGATION`,
  `OP_BACKCHANNEL_CTL`, and `NFS_CB_PROGRAM`.
- The server currently returns `NFS4ERR_NOTSUPP` for `GET_DIR_DELEGATION`,
  `WANT_DELEGATION`, and `BACKCHANNEL_CTL` in `server/compound.rs`.
- `OPEN` always returns `OpenDelegation4::None` in `server/ops/file.rs`.
- `DELEGPURGE` currently returns success, and `DELEGRETURN` is accepted by the
  existing stub path.
- Session state tracks clients, sessions, opens, locks, metadata, and lease
  expiry, but it does not track delegation stateids or callback channel state.

macOS compatibility depends on NFSv4.0:

- macOS `mount_nfs` uses `vers=4`, i.e. NFSv4.0.
- That path uses `SETCLIENTID`, `SETCLIENTID_CONFIRM`, `RENEW`,
  `OPEN_CONFIRM`, and the current OPEN/CLOSE/READ/WRITE stateid flow.
- macOS may send callback-shaped `SETCLIENTID` data, but embednfs should not
  start using that callback path as part of this feature.

Linux compatibility depends on NFSv4.1:

- Linux uses `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`, and v4.1 operation
  dispatch.
- Directory delegations are requested through `GET_DIR_DELEGATION`, not by
  changing normal `OPEN` results.

## Goals

- Preserve existing macOS behavior byte-for-byte unless a macOS-specific test
  says a change is required.
- Add opt-in NFSv4.1 directory delegations for Linux clients.
- Recall a granted directory delegation before allowing a conflicting namespace
  mutation to complete.
- Provide a backend hook so an embedder can recall delegations when the backing
  namespace changes outside the NFS request path, for example a remote sync
  apply.
- Keep the default server behavior unchanged: delegations disabled unless
  explicitly configured.
- Make every state-bearing delegation operation validate stateids instead of
  silently accepting invalid state.

## Non-Goals

- No NFSv4.0/macOS delegation support.
- No file read delegations or file write delegations in the first milestone.
- No directory notification stream in the first milestone. Recall is enough for
  coherence and is the safer starting point.
- No pNFS, layouts, device info, SSV, or RPCSEC_GSS callback work beyond what
  is needed for a `sec=sys` Linux client.
- No dependency on a kernel mount in the unit/integration test suite. Kernel
  mount smoke tests are an additional validation layer.

## Compatibility Contract

The safe rollout rule is simple: **delegation support must be dark by default**.

Add server configuration:

```rust
pub struct DelegationConfig {
    pub directory_delegations: bool,
    pub recall_timeout: Duration,
    pub max_delegations_per_client: usize,
    pub max_delegations_total: usize,
}
```

Expose it through `NfsServerBuilder`, defaulting to disabled:

```rust
NfsServer::builder(fs)
    .directory_delegations(false)
    .build();
```

When disabled:

- `GET_DIR_DELEGATION` continues to return `NFS4ERR_NOTSUPP` or a documented
  `GDD4_UNAVAIL` response after decoder coverage is complete.
- `WANT_DELEGATION` continues to return `NFS4ERR_NOTSUPP`.
- `BACKCHANNEL_CTL` behavior remains as it is unless Linux mount traces require
  a compatibility response.
- `OPEN` continues to return `OpenDelegation4::None`.
- macOS v4.0 smoke tests must pass unchanged.

When enabled:

- Only minorversion 1 and later may receive directory delegations.
- Minorversion 0 always behaves as disabled.
- `OPEN` still returns `OpenDelegation4::None` until file delegations are
  designed separately.
- v4.0 `SETCLIENTID` callback information is recorded only for existing client
  identity behavior; it is not used to issue callbacks.

## Protocol Model

Directory delegations are read-only delegations on a directory. The delegation
covers the directory attributes and the directory entries. If either changes,
the server recalls the delegation. Attribute changes on children do not require
a directory-delegation recall.

When a client holding a directory delegation mutates that directory through an
NFSv4.1 operation sent on a session associated with the same clientid, the
server must not recall that client's delegation. If other clients also hold a
delegation on the directory, the server recalls those other clients before the
mutation completes.

First milestone response policy:

- `GET_DIR_DELEGATION` on a non-directory returns the appropriate type error.
- `GET_DIR_DELEGATION` without a current filehandle returns `NFS4ERR_NOFILEHANDLE`.
- If delegation support is disabled or callback capability is unavailable, return
  unavailable/not-supported without changing server state.
- If support is enabled and the client has a callback-capable v4.1 session,
  return `GDD4_OK` with a delegation stateid.
- Return empty notification and attribute bitmaps initially unless the Linux
  client requires a specific bitmap. Recall is the coherence mechanism for v1.

Callback policy:

- Implement the minimum callback path required for `CB_RECALL` over an NFSv4.1
  backchannel.
- A successful recall is one where the client returns success to `CB_RECALL` and
  then sends `DELEGRETURN`, or where the callback response itself is sufficient
  for the client behavior validated by Linux tests.
- If callback transport is unavailable, do not grant new delegations.
- If recall fails or times out, mark the delegation revoked and proceed only
  according to the revocation policy below.

## State Model

Add delegation state to `session/model.rs`:

```rust
pub(super) struct DelegationState {
    pub object: ServerObject,
    pub clientid: Clientid4,
    pub sessionid: Option<Sessionid4>,
    pub stateid_seq: u32,
    pub kind: DelegationKind,
    pub status: DelegationStatus,
    pub granted_at: Instant,
    pub last_recall_at: Option<Instant>,
}

pub(super) enum DelegationKind {
    DirectoryRead,
}

pub(super) enum DelegationStatus {
    Granted,
    RecallInProgress,
    Returned,
    Revoked,
}
```

Store it in `StateInner`:

```rust
pub delegations: HashMap<[u8; 12], DelegationState>,
pub dir_delegations: HashMap<ServerObject, HashSet<[u8; 12]>>,
pub client_delegations: HashMap<Clientid4, HashSet<[u8; 12]>>,
```

Rules:

- Delegation stateids use the same stateid allocation style as open and lock
  stateids, but live in a distinct map.
- `TEST_STATEID` must recognize delegation stateids.
- `FREE_STATEID` must free revoked delegation stateids.
- `DELEGRETURN` must validate the stateid and client/session ownership, mark it
  returned, and remove it from all indexes.
- Client lease expiry, `DESTROY_CLIENTID`, and server-side client replacement
  must remove or revoke that client's delegations.
- Directory delegations are not reclaimable after server restart. On restart the
  server starts with no delegation state.

## Backchannel And Callback Transport

Implement callback support as an internal server subsystem, not as part of the
`FileSystem` trait.

Required pieces:

- Extend `embednfs-proto` with callback COMPOUND encoding for:
  - `CB_SEQUENCE`
  - `CB_RECALL`
  - callback result decoding for those operations
- Track per-session backchannel capability from `CREATE_SESSION`:
  - client id
  - session id
  - `cb_program`
  - negotiated backchannel limits
  - security flavor accepted for callbacks
  - one or more live connections that can carry backchannel calls
- Refactor transport enough that the server can send a framed RPC CALL on a
  callback-capable connection while normal forechannel requests continue to be
  served.

The transport refactor is the main implementation risk. The current connection
handler is request/response oriented. Backchannel callbacks need an outbound RPC
CALL initiated by the server. The clean shape is:

- split each TCP connection into a read task and a write task;
- route all writes through a per-connection sender;
- register connection ids in session state;
- allow the callback subsystem to enqueue a callback RPC on a suitable
  connection and await the matching RPC reply.

Keep this isolated behind an internal `CallbackClient` or `BackchannelManager`
so normal COMPOUND dispatch stays readable.

Do not use the NFSv4.0 callback address from `SETCLIENTID` for this milestone.
That avoids surprising macOS and avoids building two callback transports at once.

## Grant Policy

Grant directory delegations conservatively:

- Only when `DelegationConfig.directory_delegations` is true.
- Only for v4.1+ sessions with confirmed client state.
- Only after the session has usable backchannel capability.
- Only for directory objects.
- Only for clients under configured per-client and global delegation limits.
- Do not grant if a recall is already in progress for the directory.
- Prefer one active directory delegation per `(clientid, directory)`; return the
  existing delegation stateid if the client asks again and the prior state is
  still valid.

The first implementation can allow multiple clients to hold read-only directory
delegations for the same directory. An external namespace mutation recalls all
holders before completing. An NFS-originated namespace mutation recalls holders
other than the mutating client's own clientid.

## Recall Policy

Add one internal method on `StateManager`:

```rust
async fn recall_directory_delegations(
    &self,
    object: &ServerObject,
    reason: RecallReason,
) -> Result<(), RecallError>;
```

This method:

- finds all granted delegations for the directory;
- excludes the origin clientid for NFS-originated mutations;
- marks them `RecallInProgress`;
- sends `CB_RECALL` for each;
- waits for `DELEGRETURN` or timeout;
- removes returned delegations;
- marks timed-out delegations revoked.

Directory namespace mutations must recall before mutating:

- `CREATE`
- `OPEN` with create
- `LINK`
- `REMOVE`
- `RENAME` source directory
- `RENAME` target directory
- directory `SETATTR` when directory attributes covered by the delegation change

Child-file metadata changes do not recall the parent directory delegation.

For NFS operations served by embednfs, call recall from the server operation
before invoking the `FileSystem` mutator, excluding the session clientid when
the operation is sent by the delegation holder. If recall fails because the
client is temporarily busy, return `NFS4ERR_DELAY` rather than applying the
mutation with a stale delegation outstanding. If recall times out and the server
revokes the state, proceed only after setting sequence status flags so the
client can learn that state was revoked.

## External Namespace Changes

embednfs also needs a path for embedders whose namespace changes outside the NFS
request path, such as sync engines.

Add an optional public method on `NfsServer` or a cloneable handle returned by
the builder:

```rust
pub async fn recall_directory(&self, handle: &F::Handle) -> FsResult<()>;
```

or:

```rust
pub struct NfsControl { ... }

impl NfsControl {
    pub async fn recall_directory_by_handle<H>(&self, handle: &H) -> FsResult<()>;
}
```

The handle-based API should resolve the backend handle to the internal
`ServerObject` using existing object mapping. This lets portal-sync recall a
delegation before applying a remote link/unlink/rename to its store-backed
directory.

If the object is unknown because the directory has not been exposed to the NFS
client yet, the method is a no-op.

## Operation Handling

### GET_DIR_DELEGATION

Add `op_get_dir_delegation` in a new `server/ops/delegation.rs`.

Flow:

1. Require minorversion 1+ and an active session client id.
2. Resolve `CURRENT_FH` and verify it is a directory.
3. Check `DelegationConfig`.
4. Check callback capability for the session.
5. Grant or reuse a `DirectoryRead` delegation stateid.
6. Return `GetDirDelegationRes4::Ok`.

### DELEGRETURN

Replace the current permissive behavior with state-aware behavior for v4.1
delegation mode:

1. Normalize and validate the concrete stateid.
2. Ensure it belongs to the sequence client id.
3. If it is a known delegation stateid, remove it and return `OK`.
4. If it is unknown, return `NFS4ERR_BAD_STATEID`.

For minorversion 0, preserve today's compatibility unless macOS validation shows
that stricter behavior is safe.

### DELEGPURGE

Implement state-aware purge for the requesting client:

- v4.1 delegation mode: remove that client's delegation state and return `OK`.
- disabled mode or v4.0: preserve current behavior unless tests justify
  tightening.

### WANT_DELEGATION

Keep unsupported for the first directory-delegation milestone. It is primarily
for wanted file delegations and push-delegation flows, not needed for the portal
directory freshness target.

### OPEN

Keep returning `OpenDelegation4::None`. If the client sets WANT_DELEG bits in
`share_access`, mask them as today for share-mode validation and do not grant.

## Error And Revocation Semantics

Use explicit status values:

- `NFS4ERR_NOTSUPP`: feature disabled, unsupported operation, or minorversion 0
  path where delegations are not implemented.
- `GDD4_UNAVAIL`: feature enabled but this specific delegation cannot be granted
  because of resource limits or callback unavailability, if Linux handles it
  correctly.
- `NFS4ERR_BAD_STATEID`: invalid `DELEGRETURN`/`FREE_STATEID` stateid.
- `NFS4ERR_DELAY`: a namespace mutation cannot proceed because recall is in
  progress or callback returned a retryable busy response.
- `NFS4ERR_CB_PATH_DOWN`: if the protocol path calls for surfacing a broken
  callback path on a state operation.

When revoking state, set appropriate `SEQUENCE` status flags for the client.
embednfs already has revoked client lease concepts; reuse that pattern for
delegation revocation rather than inventing an unrelated notification channel.

## Testing Plan

### Unit And Protocol Tests

- Decode and encode callback COMPOUNDs for `CB_SEQUENCE` and `CB_RECALL`.
- `GET_DIR_DELEGATION` returns `NFS4ERR_NOTSUPP` when disabled.
- `GET_DIR_DELEGATION` rejects missing filehandle and non-directory filehandles.
- With delegation enabled and a fake callback-capable session, it grants a
  `DirectoryRead` stateid and indexes it by directory and client.
- Repeated `GET_DIR_DELEGATION` for the same client/directory reuses or replaces
  state according to the chosen policy.
- `DELEGRETURN` removes a valid delegation and rejects an invalid one.
- `TEST_STATEID` recognizes a granted delegation.
- `FREE_STATEID` frees a revoked delegation.
- Client lease expiry and `DESTROY_CLIENTID` remove delegations.
- Namespace mutation calls recall before `FileSystem` mutation.
- Recall timeout returns `NFS4ERR_DELAY` or revokes according to the policy.

### macOS Regression Tests

Run the existing macOS-facing tests unchanged:

- NFSv4.0 `SETCLIENTID` handshake.
- NFSv4.0 `OPEN_CONFIRM` lifecycle.
- Basic create/write/read/rename/remove through the integration RPC path.
- Kernel `mount_nfs -o vers=4,tcp,...` smoke when available.

Add one regression:

- NFSv4.0 `OPEN` still returns `OpenDelegation4::None`, and v4.0
  `DELEGRETURN` behavior is unchanged unless the compatibility decision is
  explicitly updated.

### Linux Kernel Smoke Tests

Add a Linux smoke script or extend the existing one:

1. Start an embednfs test server with directory delegations disabled. Confirm
   mount and ordinary file operations match current behavior.
2. Start with directory delegations enabled against a Linux kernel with directory
   delegation client support.
3. Confirm the client sends `GET_DIR_DELEGATION` and the server grants it.
4. Populate a directory, run repeated negative lookups/stat scans, and confirm
   the server sees fewer repeated LOOKUP/GETATTR probes than with no delegation.
5. Mutate the directory from another server-side path, recall the delegation, and
   confirm the client sees the new name without waiting for the mount's
   attribute-cache timeout.
6. Confirm normal create/unlink/rename from the delegated client still works.

Record kernel version, mount options, whether Linux's `directory_delegations`
module parameter is enabled, and packet/trace evidence for callback traffic.

## Implementation Phases

### Phase 0: Trace And Compatibility Baseline

- Capture Linux v4.1 mount traffic with current `NFS4ERR_NOTSUPP` stubs.
- Confirm whether Linux sends `BACKCHANNEL_CTL` during a normal `sec=sys` mount.
- Run and record macOS v4.0 smoke before any behavior change.

### Phase 1: State And Negative Operation Semantics

- Add delegation state maps and stateid allocation.
- Make `DELEGRETURN`, `DELEGPURGE`, `TEST_STATEID`, and `FREE_STATEID`
  delegation-aware behind config.
- Keep `GET_DIR_DELEGATION` disabled.
- Ensure macOS v4.0 tests remain unchanged.

### Phase 2: Backchannel Transport

- Add callback protocol encoding/decoding.
- Add callback connection registration from v4.1 session setup.
- Add a test callback peer that accepts `CB_SEQUENCE` + `CB_RECALL`.
- Prove the server can issue a callback without deadlocking normal COMPOUND
  traffic.

### Phase 3: Grant Directory Delegations

- Implement `op_get_dir_delegation`.
- Grant conservative directory delegations when config and callback capability
  allow.
- Keep `WANT_DELEGATION` and file delegations unsupported.

### Phase 4: Recall On Mutation

- Insert recall gates before directory namespace mutations.
- Add external recall API for embedders.
- Add timeout/revocation handling.

### Phase 5: Kernel Validation

- Validate against Linux kernel client support.
- Tune response details (`GDD4_UNAVAIL` vs `NFS4ERR_NOTSUPP`, bitmaps,
  callback security choices) based on observed Linux behavior.
- Re-run macOS v4.0 smoke after each protocol behavior change.

## Open Questions

- Does the target Linux kernel require `BACKCHANNEL_CTL` success before it will
  accept directory delegations, or is `CREATE_SESSION` backchannel setup enough?
- Which notification and attribute bitmaps does Linux require in a successful
  `GET_DIR_DELEGATION` response?
- Does Linux expect `DELEGRETURN` on successful `CB_RECALL`, or does it treat
  callback success as sufficient for directory delegations in the initial
  implementation?
- Should recall timeout revoke delegation state immediately, or should the
  mutating operation return `NFS4ERR_DELAY` for a bounded retry window first?
- Should the external recall API live on `NfsServer`, a separate `NfsControl`
  handle, or a trait object passed to embedders?

## References

- RFC 8881, NFSv4.1: Sections 10.2, 10.8, 10.9, 18.39, callback operations,
  `DELEGRETURN`, `FREE_STATEID`, and `TEST_STATEID`.
- RFC 7530, NFSv4.0: macOS client path and `SETCLIENTID` callback fields.
- `docs/macos-mount-investigation.md`: current macOS `mount_nfs` behavior.
- `docs/linux-client-compatibility.md`: current Linux v4.1 compatibility notes.
