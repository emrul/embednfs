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

The daemon derives its default NFSv4.1 `server_owner`/`server_scope` identity
from the canonical root and listen address. That keeps server restart recovery
stable for one daemon while avoiding accidental Linux trunking between separate
ports or exports. Override the identity when a deployment needs an explicit
stable value:

```bash
EMBEDNFS_SERVER_OWNER_MAJOR_ID=my-embednfs-server \
EMBEDNFS_SERVER_OWNER_MINOR_ID=0 \
EMBEDNFS_SERVER_SCOPE=my-embednfs-scope \
cargo run -p embednfsd
```

See the [main project README](https://github.com/PeronGH/embednfs) for full documentation.
