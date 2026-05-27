# embednfs — Embeddable NFSv4.0 + NFSv4.1 Server Library in Rust

## Goal

Build a production-quality, high-performance Rust NFSv4 server library. The user implements a small, opinionated filesystem trait; the library handles the wire protocol, state management, locking, and synthetic NFS-only objects, all served over TCP. The primary use case is embedding as a localhost NFS server — a FUSE replacement that needs no kernel modules. Apple/macOS client compatibility is the main implementation target; Linux compatibility comes from the v4.1 path where it is needed for xattrs.

This is not a toy or proof-of-concept. It should be correct, fast, and suitable for real workloads. Aim for zero-copy where possible, minimal allocations on the hot path, and a design that can saturate the I/O capabilities of the underlying filesystem implementation.

Supported COMPOUND minor versions are 0, 1, and 2. NFSv4.0 (RFC 7530) exists primarily because the macOS in-kernel `mount_nfs(8)` caps at minor version 0 — embednfs must serve that client through the SETCLIENTID / SETCLIENTID_CONFIRM / RENEW / OPEN_CONFIRM / RELEASE_LOCKOWNER path. NFSv4.1 (RFC 8881) is the path Linux clients use; it adds sessions and the xattr ops (RFC 8276) needed for Linux extended-attribute workflows. Any minor version outside 0..=2 must be rejected with `NFS4ERR_MINOR_VERS_MISMATCH`.

Licensed under the MIT License. The project uses Rust edition 2024. Determine the implementation scope yourself based on what the spec requires, what real clients actually need, and what matters for the localhost FUSE-replacement use case. Keep the public trait as simple as it can be and as complex as it needs to be. Prefer a minimal core trait plus optional capability extensions over one large catch-all interface. Implement what real clients actually expect; do not keep extra protocol surface unless it is effectively free and low-complexity.

## Commands

```bash
cargo clippy --workspace
cargo test --workspace
cargo run -p embednfsd --release

# macOS — kernel mount_nfs(8) caps at vers=4 (minor version 0).
mount_nfs -o vers=4,tcp,port=2049 127.0.0.1:/ /tmp/embednfs
# Linux — vers=4.1 picks up the xattr ops.
mount -t nfs4 -o vers=4.1,proto=tcp,port=2049 127.0.0.1:/ /mnt/embednfs
```

## How to Work

### Reference Materials

Before writing any code, you MUST first prepare the following:

- RFC 8881 and RFC 5531 texts.
- Apple NFS client source code (`https://github.com/apple-oss-distributions/NFS.git`).
- `pynfs` source code (`git://git.linux-nfs.org/projects/cdmackay/pynfs.git`).
- Other RFCs or implementations, if needed.

If they are missing, ask the user for them.

During planning and implementation, you MUST reference these materials and use them as ground truth.

### Testing

Integration tests exercise the full RPC path over TCP using ephemeral port binding — no root or kernel mounts required. Also test with the kernel NFS client when useful. For macOS-facing behavior, prefer real `mount_nfs` validation over inference when possible, including Finder/file-copy/xattr workflows if the change could affect them.

Every integration test must have a doc comment in exactly this shape:

```
/// Short description.
/// Origin: ... (single line)
/// RFC: ... (single line)
```

## Coding Standards

### Structure

Keep the code modular. The recommended file size is under 500 lines. The hard limit is 1000 lines; if you reach it, you must break the file down.

### Abstraction

The filesystem trait is the most important API surface. The library handles all NFSv4 state internally — clientid leases, session state, opens, locks, and version negotiation — so the trait implementor never thinks about minor versions or protocol details.

### Dependencies

Never hand-edit `Cargo.toml`. Use `cargo` for all related changes.

### Commits

Use conventional commits (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`). Commit every meaningful change as soon as possible instead of accumulating them. Each commit should compile, pass formatter, linter, and tests.

### Correctness Over Momentum

If an abstraction is wrong, rewrite it. Large-scale rewrites are encouraged. Layered patches are disallowed — always make the codebase look as if it was written this way from the beginning.

### Documentation

Doc comments on every public item. `cargo doc` should produce useful, navigable documentation. The README should cover what the library is, how to use it, and a minimal example.

### Lint Suppressions

Follow the workspace lint policy. Any non-test lint suppression must use the narrowest scope possible and include an explicit `reason = "..."`.

### Panic and Unsafe Policy

Follow the workspace lint policy for panic-prone constructs and unsafe code. In non-test code, avoid them when practical; when they are the right choice, make the invariant or tradeoff explicit in the code.
