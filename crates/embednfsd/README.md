# embednfsd

[![crates.io](https://img.shields.io/crates/v/embednfsd)](https://crates.io/crates/embednfsd)

NFSv4.1 server daemon powered by [embednfs](https://crates.io/crates/embednfs).

By default it serves `/tmp/embednfs-root` on `0.0.0.0:2049`.

```bash
EMBEDNFS_ROOT=/tmp/embednfs-root \
EMBEDNFS_LISTEN=127.0.0.1:12049 \
cargo run -p embednfsd
```

Directory delegations and the recall control listener are opt-in:

```bash
EMBEDNFS_DIRECTORY_DELEGATIONS=1 \
EMBEDNFS_RECALL_TIMEOUT_MS=1000 \
EMBEDNFS_CONTROL_LISTEN=127.0.0.1:12050 \
cargo run -p embednfsd
```

The control listener accepts `RECALL /` over a local TCP connection. It exists
for embedder-style smoke tests that need to recall the exported directory before
applying an external namespace change.

See the [main project README](https://github.com/PeronGH/embednfs) for full documentation.
