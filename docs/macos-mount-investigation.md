# macOS `mount_nfs(8)` Investigation

A handoff note for the next agent. This is a planning + diagnosis document,
not a claim of validated macOS support.

## TL;DR

On **macOS 15.5 (Darwin 24.5.0)** the kernel `mount_nfs(8)` tool can mount
an embednfs-served export over loopback when the command line is:

```bash
mount_nfs -o vers=4,tcp,port=2049,nobrowse 127.0.0.1:/ /tmp/embednfs
```

The previous failing `vers=4,tcp,port=2049,nolocks,nobrowse` command was
rejected by the Apple kernel before network I/O because `nolocks` is not
allowed for NFSv4 mounts. Apple `mount_nfs -vvv` prints `locks` for the
working command and the server then sees the expected v4.0 SETCLIENTID
handshake and mount COMPOUNDs.

The NFSv4.0 mount path inside embednfs works end-to-end: mount,
directory listing, `mkdir`, `echo hello > file.txt`, `cat file.txt`,
and `ls` all round-trip through the kernel client with no
`NFS4ERR_BAD_STATEID` in the server log. See "Write path — resolved"
below for the two bugs that previously caused the WRITE retry loop.

## Branch context

- Investigation done on `feat/restore-v40-mac-client`.
- Parent: `feat/linux-v42-xattrs` @ `488efcb feat: add Linux NFSv4.2 xattr support`.
- That parent ships the `scripts/smoke-macos-nfs41.sh` script whose
  `MOUNT_OPTS="vers=4.1,tcp,port=2049,nobrowse"` line is what originally
  motivated this investigation. The README block

  ```
  # macOS
  mount_nfs -o vers=4.1,tcp,port=2049 127.0.0.1:/ /tmp/embednfs
  Note: on macOS, vers=4 means NFSv4.0. Use vers=4.1 explicitly.
  ```

  does not work on macOS 15.5 — see "Evidence" below. The branch already
  rewrites the macOS quick-start to `vers=4`.

The branch adds NFSv4.0 protocol support (SETCLIENTID, SETCLIENTID_CONFIRM,
RENEW, OPEN_CONFIRM, RELEASE_LOCKOWNER) plus minorversion=0 dispatch, so
the macOS path can use `vers=4` (the only thing the current kernel client
will pass through). The new integration tests in
`crates/embednfs/tests/compound_session_cases/protocol_sequence.rs` cover:

- `test_minorversion_zero_accepts_putrootfh` — bare PUTROOTFH at v4.0.
- `test_v40_setclientid_handshake` — SETCLIENTID → SETCLIENTID_CONFIRM →
  PUTROOTFH + GETATTR end-to-end at v4.0.
- `test_v40_setclientid_confirm_rejects_bad_verifier` — RFC 7530 §16.34.
- `test_v40_ops_rejected_at_minorversion_one` — RFC 8881 §16 enforcement.

These pass against `tokio::net::TcpStream`; they do not exercise
`mount_nfs`.

## Symptoms

### A) `vers=4.1` — option silently dropped, falls back to v3, fails

```
$ mount_nfs -v -o vers=4.1,tcp,port=2049,nobrowse 127.0.0.1:/ /tmp/up-mnt
mount_nfs: illegal NFS version value -- 4.1
mount_nfs: can't mount with remote locks when server (127.0.0.1) is not running rpc.statd: RPC prog. not avail
mount 127.0.0.1:/ on /private/tmp/up-mnt
mount flags: 0x100000, nobrowse
socket: type:tcp,port=2049
file system locations:
/
  127.0.0.1
    inet 127.0.0.1
NFS options: fg,retrycnt=1     # <-- note: no vers=
```

The `NFS options:` line is what the kernel actually used. `vers=4.1` was
discarded. The macOS man page documents this:

> Currently NFSv4 is the highest supported version with a minor version
> of zero. … Specifying a non supported version or minor version will
> print a warning and ignore the `vers` or `nfsvers` option.

After dropping `vers=`, `mount_nfs` falls back to its default ("try v3,
fall back to v2"), and the NFSv3 path then complains about `rpc.statd`
not running. `nolocks` suppresses the `rpc.statd` requirement but does
not change the underlying "fell back to v3" problem.

### B) `vers=4` plus `nolocks` — rejected before RPC

```
$ mount_nfs -v -o vers=4,tcp,port=2049,nolocks,nobrowse 127.0.0.1:/ /tmp/up-mnt
mount_nfs: can't mount / from 127.0.0.1 onto /private/tmp/up-mnt: Invalid argument
mount 127.0.0.1:/ on /private/tmp/up-mnt
mount flags: 0x100000, nobrowse
socket: type:tcp,port=2049
file system locations:
/
  127.0.0.1
    inet 127.0.0.1
NFS options: fg,retrycnt=1,vers=4,nolocks
```

Apple's kernel source rejects disabled/local lock mode for NFSv4. In
`kext/nfs_vfsops.c`, lock modes `NFS_LOCK_MODE_DISABLED` and
`NFS_LOCK_MODE_LOCAL` return `EINVAL` when `nmp->nm_vers >= NFS_VER4`; the
post-connect mount setup also rejects any v4 mount whose lock mode is not
`NFS_LOCK_MODE_ENABLED`. This exactly matches the synchronous `Invalid
argument` before any RPC bytes reach embednfs.

Removing `nolocks` changes the verbose output to `locks` and allows the mount
to proceed.

### A vs B

In (A), `mount_nfs` is in v3 fallback because macOS rejects `vers=4.1`. In
(B), `mount_nfs` is in v4 but the `nolocks` option is invalid for v4. These
are two separate command-line problems, not a server transport problem.

## Has-our-branch-regressed check

The earlier A/B used the invalid `nolocks` command line, so it only proved
that both branches were equally rejected by the Apple kernel before network
I/O. It did not prove a server incompatibility.

With the corrected command, the current branch mounts and emits normal
NFSv4.0 traffic:

```bash
mount_nfs -vvv -o vers=4,tcp,port=2049,nobrowse 127.0.0.1:/ /tmp/embednfs
```

Observed server sequence:

- RPC NULL
- `SETCLIENTID`
- `SETCLIENTID_CONFIRM`
- `PUTROOTFH + GETATTR`
- `PUTFH + GETATTR`
- repeated `STATFS`, `LOOKUP`, and `ACCESS` traffic from Finder/kernel probes

Directory creation over the mounted filesystem succeeded. File writing exposed
the separate `WRITE`/stateid issue described below.

## Findings

### Eliminated hypotheses

- **~~Privileged source port enforcement.~~** Tested with `sudo` plus
  explicit `resvport`. `mount_nfs -v` confirmed the kernel was using
  `resvport` (the verbose `NFS options:` line listed it). Same
  synchronous `EINVAL`. Privileged-port hypothesis is dead.

- **~~kext / NetFS user-level policy.~~** Running under `sudo` would
  bypass any user-level sandboxing or policy refusal. The behavior was
  identical to the unprivileged run. NetFS as a *userland* policy gate
  is not the cause.

- **~~Server-side incompatibility with macOS mount negotiation.~~** The
  corrected command reaches embednfs and completes the v4.0 mount handshake.

- **macOS once accepted `vers=4.1`.** Recorded as a separate finding:
  the embednfs README's "Use vers=4.1 explicitly" no longer applies.
  macOS 15.5 `mount_nfs(8)` silently drops `vers=4.1` and falls back to
  v3. The branch's `docs:` commit already updates the README and quick-
  start.

- **`nolocks` is invalid for NFSv4 on macOS.** This is the direct cause of
  the `vers=4` / `Invalid argument` failure. NFSv4 locking is integrated into
  the protocol state model, so the Apple client requires enabled locks for v4
  mounts.

## Apple Source Notes

`mount_nfs` and the kernel-side NFS code are open source:

<https://github.com/apple-oss-distributions/NFS>

Relevant source path:

- `mount_nfs/mount_nfs.c`: parses `nolocks` into `NFS_LOCK_MODE_DISABLED` and
  passes XDR mount args to `mount("nfs", ...)`.
- `kext/nfs_vfsops.c`: rejects disabled/local lock mode for NFSv4 with
  `EINVAL`.

## Write path — resolved

The earlier `WRITE → NFS4ERR_BAD_STATEID → OPEN(claim=PREVIOUS)` retry
loop was two bugs in the new v4.0 path, both fixed in
`fix(server): NFSv4.0 OPEN_CONFIRM + I/O stateid path`:

1. **OPEN_CONFIRM never updated server-side `stateid_seq`.** The handler
   was bumping only the response, so the next WRITE arrived with
   `seqid = 2` while `open_files[other].stateid_seq` was still `1`, and
   `validate_stateid_seq` rejected it. The fix adds
   `StateManager::confirm_open_state` which validates the client's
   stateid, bumps the stored seqid, and returns the new one (RFC 7530
   §16.18).

2. **`resolve_io_stateid` demanded a `sequence_clientid`.** WRITE, READ,
   CLOSE, and OPEN_DOWNGRADE all extracted a `Clientid4` from the
   COMPOUND's SEQUENCE op and rejected with `NFS4ERR_BAD_STATEID` if it
   was missing. v4.0 has no SEQUENCE, so every IO op tripped that gate
   before the stateid was even looked up. `resolve_stateid` now accepts
   `Option<Clientid4>`: `Some` keeps the RFC 8881 §15.1.16.4 owner
   check for v4.1; `None` (v4.0) skips it and trusts the stateid lookup
   itself.

Live verification on macOS 15.5: `mount_nfs -o vers=4,tcp,port=2049,nobrowse`
mounts unprivileged, `echo hello > foo.txt && cat foo.txt && ls` all
succeed through the kernel client, and the server log shows zero
`BadStateid` results across the full round-trip.

## Validation Harness

The default macOS smoke options should be:

```bash
MOUNT_OPTS="vers=4,tcp,port=2049,nobrowse"
```

Do not add `nolocks` to macOS NFSv4 smoke commands. With the write
fix in place, `scripts/smoke-macos-nfs41.sh` should run end-to-end
without hanging.

## Definition of done

A clean `mount_nfs … 127.0.0.1:/ /tmp/embednfs` against `embednfsd` on
the current macOS, with `ls`, `mkdir`, `cat`, and a small write all succeeding
through the kernel client. The smoke script in `scripts/` should reproduce it
without hanging.

The integration tests in `crates/embednfs/tests/compound_session_cases/`
already cover the on-the-wire correctness for v4.0; the goal of this
investigation is **only** to unblock the kernel-client path.

## Sources

- macOS `mount_nfs(8)` man page (verify locally with `man mount_nfs`).
- Apple NFS client source: <https://github.com/apple-oss-distributions/NFS>.
- RFC 7530 (NFSv4.0): <https://www.ietf.org/rfc/rfc7530.html>.
- RFC 8881 (NFSv4.1): <https://www.ietf.org/rfc/rfc8881.html>.
- RFC 5531 (ONC RPC v2): <https://www.rfc-editor.org/rfc/rfc5531>.
- macOS 15.5 build observed: ProductVersion 15.5, BuildVersion 24F74,
  Darwin 24.5.0.
