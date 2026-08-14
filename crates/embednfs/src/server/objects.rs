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

    #[expect(
        clippy::unused_async,
        reason = "kept async to match the surrounding server helper call pattern"
    )]
    pub(super) async fn resolve_object(
        &self,
        fh: &Option<NfsFh4>,
    ) -> Result<(NfsFh4, ServerObject), NfsStat4> {
        let fh = fh.clone().ok_or(NfsStat4::Nofilehandle)?;
        let object = self.state.fh_to_object(&fh).ok_or(NfsStat4::Stale)?;
        Ok((fh, object))
    }

    pub(super) async fn parent_change_after_xattr_mutation(
        &self,
        ctx: &crate::fs::RequestContext,
        parent: ObjectId,
    ) {
        let _ = self.getattr(ctx, parent).await;
    }
}
