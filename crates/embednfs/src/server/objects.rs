use embednfs_proto::{NfsFh4, NfsStat4};

use crate::fs::{FileSystem, FsError};
use crate::internal::{ObjectId, ServerObject};

use super::{NfsResult, NfsServer};

impl<F: FileSystem> NfsServer<F> {
    /// Give `handle` an object id, unless it names something retired.
    ///
    /// The fence check happens **while holding the registration write lock**,
    /// the same lock `retire_handles` takes to install a fence and snapshot the
    /// map. That is what closes the window: a registration either finishes
    /// before the retirement, and is therefore in the snapshot it drops, or
    /// takes the lock afterwards and sees the fence. It cannot land in between
    /// and survive.
    ///
    /// The fast path reads the map without the fence, which is safe because a
    /// hit means the mapping predates any retirement that would have removed
    /// it — retirement drops matching entries under the same lock.
    pub(super) async fn register_handle(&self, handle: &F::Handle) -> NfsResult<ObjectId> {
        if let Some(id) = self.handle_to_object.read().await.get(handle).copied() {
            return Ok(id);
        }

        let mut handle_to_object = self.handle_to_object.write().await;
        if let Some(id) = handle_to_object.get(handle).copied() {
            return Ok(id);
        }
        if self.retired.read().await.is_retired(handle) {
            // Withdrawn. The only honest answer is that the object is gone.
            return Err(FsError::Stale);
        }

        let id = self
            .next_object_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = handle_to_object.insert(handle.clone(), id);
        let _ = self
            .object_to_handle
            .write()
            .await
            .insert(id, handle.clone());
        // Before the id escapes, so the first filehandle minted for it is
        // already the persistent one.
        if let Some(encoded) = self
            .fs
            .persistent_handles()
            .and_then(|codec| codec.encode_handle(handle))
            .filter(|e| !e.is_empty() && e.len() <= crate::fs::MAX_PERSISTENT_HANDLE_LEN)
        {
            self.state.record_persistent_fh(id, encoded);
        }
        Ok(id)
    }

    pub(super) async fn object_from_handle(&self, handle: &F::Handle) -> NfsResult<ServerObject> {
        Ok(ServerObject::Fs(self.register_handle(handle).await?))
    }

    pub(super) async fn root_object(&self) -> NfsResult<ServerObject> {
        self.object_from_handle(&self.fs.root()).await
    }

    pub(super) async fn resolve_backend_handle(&self, object_id: ObjectId) -> NfsResult<F::Handle> {
        self.object_to_handle
            .read()
            .await
            .get(&object_id)
            .cloned()
            .ok_or(FsError::BadHandle)
    }

    pub(super) async fn resolve_object(
        &self,
        fh: &Option<NfsFh4>,
    ) -> Result<(NfsFh4, ServerObject), NfsStat4> {
        let fh = fh.clone().ok_or(NfsStat4::Nofilehandle)?;
        if let Some(object) = self.state.fh_to_object(&fh) {
            return Ok((fh, object));
        }
        let object = self.decode_persistent_fh(&fh).await?;
        Ok((fh, object))
    }

    /// Resolve a persistent filehandle this boot has not seen, through the
    /// backend's decoder.
    ///
    /// The decoded handle goes through the same registration as any other, so
    /// a retirement fence still refuses it, and the handle is re-minted rather
    /// than trusted: the bytes the client sent must be exactly the bytes this
    /// server would mint for the object now, or they are stale.
    async fn decode_persistent_fh(&self, fh: &NfsFh4) -> Result<ServerObject, NfsStat4> {
        let body = crate::session::StateManager::persistent_fh_body(fh).ok_or(NfsStat4::Stale)?;
        let codec = self.fs.persistent_handles().ok_or(NfsStat4::Stale)?;
        let handle = codec.decode_handle(body).await.map_err(|e| match e {
            FsError::Delay => NfsStat4::Delay,
            FsError::Stale | FsError::NotFound | FsError::BadHandle | FsError::InvalidInput => {
                NfsStat4::Stale
            }
            other => other.to_nfsstat4(),
        })?;
        let object = ServerObject::Fs(
            self.register_handle(&handle)
                .await
                .map_err(|e| e.to_nfsstat4())?,
        );
        match self.state.object_to_fh(&object) {
            Some(minted) if minted == *fh => Ok(object),
            _ => Err(NfsStat4::Stale),
        }
    }

    pub(super) async fn parent_change_after_xattr_mutation(
        &self,
        ctx: &crate::fs::RequestContext,
        parent: ObjectId,
    ) {
        let _ = self.getattr(ctx, parent).await;
    }
}
