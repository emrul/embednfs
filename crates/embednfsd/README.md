# embednfsd

[![crates.io](https://img.shields.io/crates/v/embednfsd)](https://crates.io/crates/embednfsd)

NFSv4.1 server daemon powered by [embednfs](https://crates.io/crates/embednfs).

By default it serves `/tmp/embednfs-root` on `0.0.0.0:2049`.

```bash
EMBEDNFS_ROOT=/tmp/embednfs-root \
EMBEDNFS_LISTEN=127.0.0.1:12049 \
cargo run -p embednfsd
```

See the [main project README](https://github.com/PeronGH/embednfs) for full documentation.
