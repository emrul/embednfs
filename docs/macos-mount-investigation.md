# macOS `mount_nfs(8)` Investigation

A handoff note for the next agent. This is a planning + diagnosis document,
not a claim of validated macOS support.

## TL;DR

On **macOS 15.5 (Darwin 24.5.0)** the kernel `mount_nfs(8)` tool **cannot
mount any embednfs-served export over loopback**, with or without the
`feat/restore-v40-mac-client` work. Two distinct failure modes were
observed; both happen *before* embednfs sees a single RPC byte.

The NFSv4.0 wire path inside embednfs works (proven by integration tests
that bypass `mount_nfs` and speak v4.0 directly over the TCP socket). The
remaining gap is between `mount_nfs` and the kernel NFS client — not in
embednfs itself.

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

### B) `vers=4` — TCP connects, no RPC ever flows, `Invalid argument`

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

Server-side trace logging (`RUST_LOG=embednfs=trace`) confirms:

```
INFO  embednfsd: serving /private/tmp/embednfs-root on 0.0.0.0:2049
INFO  embednfs::server: NFSv4.1 server listening on 0.0.0.0:2049
DEBUG embednfs::server: New connection from 127.0.0.1:57275
DEBUG embednfs::server: New connection from 127.0.0.1:57276
```

Two TCP connections accepted; **zero** `trace!("RPC request bytes=…")`
lines (that trace is at `crates/embednfs/src/server/transport.rs:113` —
it fires on any decoded RPC envelope). `mount_nfs` is opening sockets
and closing them without ever sending NFS traffic.

### A vs B: same root cause likely

In (A) `mount_nfs` is in v3 fallback; in (B) it is committed to v4. Both
fail before sending NFSv4 protocol, just with different error strings.
The "`Invalid argument`" form is the one to chase — it is the v4 path
giving up at a TCP-level / pre-RPC step.

## Has-our-branch-regressed check

Run from `/Users/emrul/dev/emrul/portal-sync` (or wherever):

```bash
git -C /Users/emrul/dev/github/emrul/embednfs worktree add /tmp/embednfs-upstream feat/linux-v42-xattrs
cd /tmp/embednfs-upstream && cargo build --release -p embednfsd
RUST_LOG=embednfs=trace ./target/release/embednfsd &
mount_nfs -v -o vers=4,tcp,port=2049,nolocks,nobrowse 127.0.0.1:/ /tmp/up-mnt
```

Result on macOS 15.5: identical "Invalid argument" plus zero RPC bytes
on the server side. The failure is pre-existing — both branches behave
the same way.

To clean up: `git -C /Users/emrul/dev/github/emrul/embednfs worktree remove /tmp/embednfs-upstream`.

## Hypotheses, in order of "cheap to test, likely to be the answer"

1. **`mount_nfs` is doing an rpcbind/portmap probe on UDP/111 even with
   `port=` set.** Failure of that probe is what produces "Invalid
   argument" before any NFSv4 traffic is attempted. Apple's NFS client
   source (see "Sources") documents portmap dependencies for legacy
   versions; the v4 path is supposed to skip rpcbind when `port=` is
   given, but the current `mount_nfs` may still issue an exploratory
   UDP/111 NULL.

2. **Privileged source port enforcement.** The kernel may require the
   NFS data socket to bind to a `< 1024` source port, ignoring
   `resvport=off` for the loopback NFS case. If `mount_nfs` cannot
   acquire a reserved port (because it is not setuid root, or because of
   sandboxing in the user's session), it might abort with "Invalid
   argument". Worth testing under `sudo`.

3. **`mount_nfs` expects a server-initiated "NULL RPC" or some banner.**
   Unlikely (RPC has no banner), but the precise wire trace would
   confirm.

4. **kext or NetFS pathing.** macOS routes NFS mounts through `NetFS`
   user-space helpers. If something there refuses an
   unprivileged-localhost mount on policy grounds, the error would
   surface as "Invalid argument" with no on-wire RPC.

5. **`vers=4.1` was never actually validated on a recent macOS.** The
   embednfs README's "Use vers=4.1 explicitly" note implies it once
   worked, but the man page on macOS 15.5 explicitly caps the kernel
   client at minor version 0. If older Macs ever accepted vers=4.1,
   that path may have been removed in a recent macOS release. The
   smoke script that uses it has likely been silently failing for some
   time.

## Suggested next steps

### Step 1 — Packet capture, in any order (15 min)

Settles (1) by definition.

```bash
# Terminal A
RUST_LOG=embednfs=trace cargo run --release -p embednfsd
# Terminal B
sudo tcpdump -i lo0 -nn -X 'port 2049 or port 111' -w /tmp/macos-mount.pcap &
mount_nfs -v -o vers=4,tcp,port=2049,nolocks,nobrowse 127.0.0.1:/ /tmp/test 2>&1
sudo killall tcpdump
```

Then inspect with:

```bash
sudo tcpdump -r /tmp/macos-mount.pcap -nn -A | head -200
# or in Wireshark with the "Sun RPC" / "NFS" dissectors
```

What to look for, in priority order:

- UDP/111 traffic at all → confirms hypothesis (1).
- TCP/2049 SYN/ACK then immediate FIN/RST from the client → look at the
  client TCP options for socket-buffer / source-port indicators of (2).
- Any RPC payload (`80 00 00 ..` record marker followed by `NFS
  PROGRAM=100003`) — would tell us the COMPOUND that's being attempted
  and the server's response (or non-response).

### Step 2 — Eliminate privileged-port theory (5 min)

```bash
sudo mount_nfs -v -o vers=4,tcp,port=2049,nolocks,nobrowse 127.0.0.1:/ /tmp/test
```

If `sudo` succeeds where unprivileged fails, hypothesis (2) is the
answer. The fix is documenting that macOS mounts need either
`sudo`/setuid or a kernel-side relaxation we cannot do from user space.

### Step 3 — Cross-reference Apple's NFS client source

Apple ships the macOS NFS client at:

<https://github.com/apple-oss-distributions/NFS>

Read `mount_nfs/mount_nfs.c` (the userland entry) and
`NFS/nfs_socket.c` (the kernel side) for the connect path. Specifically:

- Trace what happens between `nfsmount` syscall setup and the first
  `mbuf` send. The "Invalid argument" / `EINVAL` we see has to be
  emitted by one of those layers.
- Check whether the kernel client requires `mountport=` to be set for
  v4 mounts. Some legacy paths still do.

### Step 4 — If Steps 1–3 do not yield an answer, try the public-NFS path

NFSv4 has a `PUTPUBFH` op and a "public filehandle" model that some
kernel clients fall back to when the export path resolution is
ambiguous. `mount_nfs -o public 127.0.0.1:/ /tmp/test` may behave
differently — worth one attempt.

### Step 5 — Validation harness

Once a mount succeeds, fold the working command line back into
`scripts/smoke-macos-nfs41.sh` (or rename it `scripts/smoke-macos-nfs40.sh`
since macOS is on v4.0) so the regression is caught next time. The
existing script's `vers=4.1` should be replaced with whatever Step 1–4
proves to work.

## Definition of done

A clean `mount_nfs … 127.0.0.1:/ /tmp/embednfs` against `embednfsd` on
the current macOS, with `ls`, `cat`, `mkdir`, and a small write all
succeeding through the kernel client. The smoke script in `scripts/`
should reproduce it.

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
