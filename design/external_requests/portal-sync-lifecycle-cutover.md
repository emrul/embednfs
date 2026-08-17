# External Request - Portal lifecycle cutover support in embednfs

Status: approved; partially implemented
Local design: `portal-sync/docs/design/granted-tree-revocation-lifecycle.md`
(branch `phase6-aster-registry-and-fetch-ranking`)
External project: [embednfs](https://github.com/emrul/embednfs)

## Summary

portal-sync's grant lifecycle coordinator needs two embednfs contracts before it
can replace the legacy granted-Tree path:

- `nfs-delay-status`: a backend error that maps precisely to
  `NFS4ERR_DELAY`; and
- `nfs-lifecycle-control`: a released `NfsServerControl` surface containing
  permanent handle retirement and truthful write-delegation recall.

Most lifecycle-control machinery already exists. Boot-scoped filehandles,
`NfsServer::control_handle`, and the permanent `retire_handles` fence are on
`main` at `567c5ba`; the write-delegation contract is on
`write-delegation-recall-contract` at `0b3fbc3`. This request asks for the
remaining status mapping and one released revision containing both lifecycle
operations. Portal owns capturing the control handle and adapting it to its
coordinator.

## Local Context

Each granted Tree has an immutable NFS generation and an operation gate. A role
swap temporarily pauses admission while preserving the generation and existing
filehandles. A revocation permanently hides, drains, retires, and tombstones the
generation.

Those transitions require different NFS meanings:

- During the temporary pause, `Denied::NotReady` and `Denied::Overloaded` mean
  "retry shortly; the handle remains valid." The only correct NFSv4.1 status is
  `NFS4ERR_DELAY`.
- During permanent withdrawal, all filehandle mappings and associated
  OPEN/lock/delegation state must be retired and fenced so old handles return
  `NFS4ERR_STALE` and cannot be minted again.
- Before Write-to-Read narrowing closes writes, Portal must ask the NFS server
  whether any client still holds a write delegation over that Tree. This is
  server state; the filesystem backend must not infer it from current policy.

The coordinator already orders and bounds these effects. It needs embednfs to
express them truthfully; it must not replace a retry with an I/O failure or
pretend server-side state was recalled when it was not.

## Requested Change

### 1. `nfs-delay-status`

Add a public retryable `FsError` variant (preferred spelling: `FsError::Delay`)
whose `to_nfsstat4` mapping is exactly `NfsStat4::Delay`.

The variant should be usable from any filesystem operation that returns
`FsResult<T>`. It denotes a transient refusal, not stale identity, permission
denial, or an unrecoverable server fault.

The variant's public documentation is part of the contract. It must say that
the refusal is transient, the current filehandle remains valid, and the NFS
client is expected to retry. It must also warn backend authors not to use
`Delay` as a generic "busy" result: when an operation has actually failed,
`Io` or another specific error remains the honest answer.

### 2. `nfs-lifecycle-control`

Ship one released embednfs revision containing these existing/branched
contracts:

1. `NfsServer::control_handle() -> NfsServerControl<_>` so an embedder can keep
   server lifecycle control before moving the server into `listen`/`serve`.
2. `NfsServerControl::retire_handles(scope)`, with its permanent,
   registration-safe and idempotent retirement fence.
3. `NfsServerControl::recall_write_delegations(scope)`, from
   `write-delegation-recall-contract` (`0b3fbc3`). The result must be derived
   from live delegation state. `Ok(0)` means no scoped client holds authority
   to write file bytes; it must not mean merely that the current implementation
   normally grants none.

The caller owns transition deadlines. If a future delegation kind conveys
write authority, its classification must be explicit and the operation must
either recall it or return a failure that lets Portal remain fail-closed. It
must never claim successful recall without performing it.

No additional daemon-control primitive is requested: `control_handle()` already
provides the required ownership boundary. Portal will retain and wire that
handle after adopting the released dependency.

## Why Current Behavior Is Insufficient

`NfsStat4::Delay` exists in `embednfs-proto`, but `FsError` has no corresponding
variant. Mapping Portal's transient barrier to `Io` or `ServerFault` says the
operation failed; mapping it to `Stale` tells a client to discard a handle that
remains valid. Portal therefore cannot expose a role-swap pause honestly.

For lifecycle control, Portal cannot adopt a branch-only contract as a stable
dependency. The retirement fence is on `main`, while write-delegation recall is
one commit later on a feature branch. A released revision containing both is
needed before Portal can replace its refusing production adapter.

## Evidence

| Evidence | Link / command / artifact |
| --- | --- |
| Missing backend delay variant | `crates/embednfs/src/fs.rs`: `FsError` and `FsError::to_nfsstat4` have no `Delay`, while `embednfs-proto::NfsStat4::Delay` exists |
| Permanent handle retirement | `revocation-support-nonce-and-retire`, through `567c5ba`; `NfsServerControl::retire_handles` |
| Control ownership already exists | `NfsServer::control_handle()` in `crates/embednfs/src/server/mod.rs` |
| Write-delegation query | `write-delegation-recall-contract` at `0b3fbc3`; `NfsServerControl::recall_write_delegations` |
| Consumer cutover keys | portal-sync `crates/portal-syncd/src/cutover.rs`: `nfs-delay-status` and `nfs-lifecycle-control` |
| Consumer design | portal-sync `docs/design/granted-tree-revocation-lifecycle.md`, External Project Dependencies |

## Compatibility And Migration

`FsError::Delay` adds an enum variant. Exhaustive downstream matches will need
to classify it; that compile-time visibility is desirable because silently
folding a retry into another status changes protocol semantics.

The control methods are additive. Existing embedders need not retain a control
handle. Portal will update to the released embednfs version, retain the handle
before server startup, and implement its `NfsLifecycleControl` adapter.

Permanent retirement fences are generation-scoped. Portal never reuses a
generation, so a re-grant receives a fresh generation rather than attempting to
remove a fence.

## Local Workarounds Or Fallbacks

There is no correct substitute for `NFS4ERR_DELAY`. Portal currently keeps the
coordinator cutover disabled rather than expose a transient barrier with the
wrong wire meaning.

The lifecycle acceptance suite uses an off-by-default test substitute for NFS
control. Production methods refuse with an error naming the missing dependency;
they do not log and pretend success.

## Acceptance Criteria

| Criterion | How to verify |
| --- | --- |
| Backend delay maps exactly | Unit-test `FsError::Delay.to_nfsstat4() == NfsStat4::Delay` |
| Public documentation preserves the wire contract | `FsError::Delay` documents that the condition is transient, the handle remains valid, and the client should retry; it distinguishes this from a generic busy or failed-I/O result |
| Delay survives an NFS operation boundary | A test filesystem returns `FsError::Delay`; the compound response carries `NFS4ERR_DELAY` |
| Retirement remains permanent and race-safe | Existing retirement-fence, post-fence registration, OPEN, lock, delegation, and reboot-nonce tests pass |
| Current server truthfully reports no scoped write delegations | Exercise `recall_write_delegations` with live in-scope directory delegation state and receive `Ok(0)` |
| Future write-delegation kinds cannot bypass classification | Adding a write-capable `DelegationKind` fails compilation until `conveys_write` and recall behavior are handled |
| One adoptable revision contains all contracts | `control_handle`, `retire_handles`, `recall_write_delegations`, and `FsError::Delay` are present on the released/tagged revision used by Portal |
| Repository gate remains green | `cargo test -p embednfs --lib`, formatting, and clippy for the changed targets pass |

## Proposed Wording For External Issue / PR

```text
Portal's generation-preserving Tree lifecycle needs two embednfs contracts.

First, add FsError::Delay mapped exactly to NFS4ERR_DELAY. During a role swap
Portal temporarily refuses operations while keeping filehandles valid; Io,
ServerFault and Stale all communicate the wrong client behavior.

Second, publish one revision containing the existing control_handle and
permanent retire_handles fence together with recall_write_delegations from
write-delegation-recall-contract. Recall must query live server state: Ok(0)
means no scoped client holds write authority, not merely that the server does
not normally grant it. Portal owns retaining and adapting the control handle.
```

## User Approval

Approved to file/request externally: yes

Notes: approved by the Portal/Aster maintainer on 2026-08-17. This request does
not authorize Portal production cutover; Portal retains its own independent
`portal-legacy-ownership` gate.
