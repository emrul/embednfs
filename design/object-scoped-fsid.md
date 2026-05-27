# Object-Scoped FSID And Stats

## Summary

Make FSID and filesystem stats object-scoped. `fsid` belongs on `Attrs`, while `statfs` should be called for the object whose filesystem attributes are being encoded. This supports one `FileSystem` implementation exposing a pseudo-root with multiple child trees that report distinct FSIDs and df values.

RFC basis: `fsid` identifies the filesystem holding the object, mount crossing is detected by filesystem boundaries, `GETATTR` returns object attributes, and `mounted_on_fileid` remains recommended for mountpoint handling. See RFC 8881 Section 5.8.1.9, Section 5.4, Section 7.7, Section 18.7.3, and Section 18.13.4.

## Key Changes

- Add public `FsId { major: u64, minor: u64 }` with `Clone`, `Copy`, `Debug`, `Eq`, `PartialEq`, `Hash`, and a manual `Default` returning `{ major: 1, minor: 1 }`.
- Add `pub fsid: FsId` to `Attrs`; `Attrs::new()` uses `FsId::default()`.
- Add `fsid: FsId` to internal `ServerFileAttr`; encode `FATTR4_FSID` from that field, never from `AttrEncodingContext`.
- Change `FileSystem::statfs` to:

  ```rust
  async fn statfs(&self, ctx: &RequestContext, handle: &Self::Handle) -> FsResult<FsStats>;
  ```

- For normal `GETATTR`, fetch stats for the current object only when the requested bitmap includes `files_*` or `space_*`.
- For `READDIR`, encode every entry using that entry's own `Attrs.fsid`; fetch that entry's stats only if the entry attr request includes stats bits.
- For synthetic NFS-only objects, inherit FSID and stats identity from the backing parent object.
- Add an internal helper such as `request_needs_fs_stats(&Bitmap4)` so normal `GETATTR` and `READDIR` avoid unnecessary stats calls and `READDIR` does not take an unconditional N+1 hit.
- Keep `mounted_on_fileid` unchanged for now, but put it on the macOS/Linux pseudo-root compatibility watchlist.

## Test Plan

- Add a custom integration filesystem with one `FileSystem` implementation and handles `Root`, `TreeA`, and `TreeB`.
- Assert `GETATTR(FATTR4_FSID)` returns distinct configured FSIDs for root and both trees.
- Assert root `READDIR` with `FATTR4_FSID` returns each child's own FSID.
- Cover both inline `READDIR` attrs and fallback-to-`getattr()` attrs.
- Add a stats regression: `GETATTR`/`READDIR` without stats attrs must not call `statfs`; with `space_*` or `files_*`, it must call `statfs` for the exact object being encoded.
- Preserve required integration-test doc comments:

  ```rust
  /// Short description.
  /// Origin: ...
  /// RFC: ...
  ```

## Assumptions

- This is an intentional public API break because `getattr()` already represents the complete exported attribute view.
- Default FSID preserves current single-export behavior.
- Backends may return different FSIDs for different handles to model pseudo-roots, junctions, or multiple exports under one `FileSystem`.
