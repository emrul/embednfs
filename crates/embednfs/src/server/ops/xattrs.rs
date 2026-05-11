use bytes::Bytes;
use embednfs_proto::*;

use crate::fs::{AccessMask, FileSystem, FsError, RequestContext, XattrSetMode};
use crate::internal::ServerObject;

use super::super::NfsServer;

impl<F: FileSystem> NfsServer<F> {
    pub(crate) async fn op_getxattr(
        &self,
        request_ctx: &RequestContext,
        args: &GetxattrArgs4,
        current_fh: &Option<NfsFh4>,
    ) -> NfsResop4 {
        let id = match self
            .resolve_xattr_target(request_ctx, current_fh, AccessMask::XATTR_READ)
            .await
        {
            Ok(id) => id,
            Err(status) => return NfsResop4::Getxattr(status, None),
        };
        if let Err(status) = self.validate_xattr_key(&args.name) {
            return NfsResop4::Getxattr(status, None);
        }

        match self.get_xattr_value(request_ctx, id, &args.name).await {
            Ok(value) => NfsResop4::Getxattr(NfsStat4::Ok, Some(value)),
            Err(e) => NfsResop4::Getxattr(xattr_error(e), None),
        }
    }

    pub(crate) async fn op_setxattr(
        &self,
        request_ctx: &RequestContext,
        args: &SetxattrArgs4,
        current_fh: &Option<NfsFh4>,
    ) -> NfsResop4 {
        let id = match self
            .resolve_xattr_target(request_ctx, current_fh, AccessMask::XATTR_WRITE)
            .await
        {
            Ok(id) => id,
            Err(status) => return NfsResop4::Setxattr(status, None),
        };
        if let Err(status) = self.validate_xattr_key(&args.key) {
            return NfsResop4::Setxattr(status, None);
        }
        let object = ServerObject::Fs(id);
        let before = match self.build_attr(request_ctx, &object).await {
            Ok(attr) => attr.change_id,
            Err(e) => return NfsResop4::Setxattr(e.to_nfsstat4(), None),
        };

        let mode = match args.option {
            SetxattrOption4::Either => XattrSetMode::CreateOrReplace,
            SetxattrOption4::Create => XattrSetMode::CreateOnly,
            SetxattrOption4::Replace => XattrSetMode::ReplaceOnly,
        };

        match self
            .set_xattr_value(request_ctx, id, &args.key, args.value.clone(), mode)
            .await
        {
            Ok(()) => {
                let cinfo = self
                    .mutation_change_info(request_ctx, &object, before)
                    .await;
                NfsResop4::Setxattr(NfsStat4::Ok, Some(cinfo))
            }
            Err(e) => NfsResop4::Setxattr(xattr_error(e), None),
        }
    }

    pub(crate) async fn op_listxattrs(
        &self,
        request_ctx: &RequestContext,
        args: &ListxattrsArgs4,
        current_fh: &Option<NfsFh4>,
    ) -> NfsResop4 {
        let id = match self
            .resolve_xattr_target(request_ctx, current_fh, AccessMask::XATTR_LIST)
            .await
        {
            Ok(id) => id,
            Err(status) => return NfsResop4::Listxattrs(status, None),
        };

        match self.list_xattr_keys(request_ctx, id).await {
            Ok(mut names) => {
                names.sort();
                match listxattrs_page(&names, args.cookie, args.maxcount) {
                    Ok(res) => NfsResop4::Listxattrs(NfsStat4::Ok, Some(res)),
                    Err(status) => NfsResop4::Listxattrs(status, None),
                }
            }
            Err(e) => NfsResop4::Listxattrs(xattr_error(e), None),
        }
    }

    pub(crate) async fn op_removexattr(
        &self,
        request_ctx: &RequestContext,
        args: &RemovexattrArgs4,
        current_fh: &Option<NfsFh4>,
    ) -> NfsResop4 {
        let id = match self
            .resolve_xattr_target(request_ctx, current_fh, AccessMask::XATTR_WRITE)
            .await
        {
            Ok(id) => id,
            Err(status) => return NfsResop4::Removexattr(status, None),
        };
        if let Err(status) = self.validate_xattr_key(&args.name) {
            return NfsResop4::Removexattr(status, None);
        }
        let object = ServerObject::Fs(id);
        let before = match self.build_attr(request_ctx, &object).await {
            Ok(attr) => attr.change_id,
            Err(e) => return NfsResop4::Removexattr(e.to_nfsstat4(), None),
        };

        match self.remove_xattr_value(request_ctx, id, &args.name).await {
            Ok(()) => {
                let cinfo = self
                    .mutation_change_info(request_ctx, &object, before)
                    .await;
                NfsResop4::Removexattr(NfsStat4::Ok, Some(cinfo))
            }
            Err(e) => NfsResop4::Removexattr(xattr_error(e), None),
        }
    }

    async fn resolve_xattr_target(
        &self,
        request_ctx: &RequestContext,
        current_fh: &Option<NfsFh4>,
        access: AccessMask,
    ) -> Result<crate::internal::ObjectId, NfsStat4> {
        let (_, object) = self.resolve_object(current_fh).await?;
        let ServerObject::Fs(id) = object else {
            return Err(NfsStat4::WrongType);
        };
        if self.named_attrs().is_none() {
            return Err(NfsStat4::Notsupp);
        }
        match self.access_for(request_ctx, id, access).await {
            Ok(mask) if mask.contains(access) => Ok(id),
            Ok(_) => Err(NfsStat4::Access),
            Err(e) => Err(e.to_nfsstat4()),
        }
    }

    fn validate_xattr_key(&self, name: &str) -> Result<(), NfsStat4> {
        self.validate_component_name(name)
    }

    async fn list_xattr_keys(
        &self,
        ctx: &RequestContext,
        id: crate::internal::ObjectId,
    ) -> Result<Vec<String>, FsError> {
        let handle = self.resolve_backend_handle(id).await?;
        let named = self.named_attrs().ok_or(FsError::Unsupported)?;
        named.list_xattrs(ctx, &handle).await
    }

    async fn get_xattr_value(
        &self,
        ctx: &RequestContext,
        id: crate::internal::ObjectId,
        name: &str,
    ) -> Result<Bytes, FsError> {
        let handle = self.resolve_backend_handle(id).await?;
        let named = self.named_attrs().ok_or(FsError::Unsupported)?;
        named.get_xattr(ctx, &handle, name).await
    }

    async fn set_xattr_value(
        &self,
        ctx: &RequestContext,
        id: crate::internal::ObjectId,
        name: &str,
        value: Bytes,
        mode: XattrSetMode,
    ) -> Result<(), FsError> {
        let handle = self.resolve_backend_handle(id).await?;
        let named = self.named_attrs().ok_or(FsError::Unsupported)?;
        named.set_xattr(ctx, &handle, name, value, mode).await
    }

    async fn remove_xattr_value(
        &self,
        ctx: &RequestContext,
        id: crate::internal::ObjectId,
        name: &str,
    ) -> Result<(), FsError> {
        let handle = self.resolve_backend_handle(id).await?;
        let named = self.named_attrs().ok_or(FsError::Unsupported)?;
        named.remove_xattr(ctx, &handle, name).await
    }
}

fn xattr_error(error: FsError) -> NfsStat4 {
    match error {
        FsError::NotFound => NfsStat4::NoXattr,
        FsError::FileTooLarge => NfsStat4::Xattr2Big,
        other => other.to_nfsstat4(),
    }
}

fn listxattrs_page(
    names: &[String],
    cookie: u64,
    maxcount: u32,
) -> Result<ListxattrsResOk4, NfsStat4> {
    let start = usize::try_from(cookie).map_err(|_| NfsStat4::Inval)?;
    if start > names.len() {
        return Err(NfsStat4::Inval);
    }

    let maxcount = maxcount as usize;
    let mut used = 8 + 4 + 4;
    let mut end = start;
    while let Some(name) = names.get(end) {
        let item_len = xdr_opaque_len(name.len());
        if used + item_len > maxcount {
            if end == start {
                return Err(NfsStat4::Toosmall);
            }
            break;
        }
        used += item_len;
        end += 1;
    }

    Ok(ListxattrsResOk4 {
        cookie: end as u64,
        names: names.get(start..end).ok_or(NfsStat4::Serverfault)?.to_vec(),
        eof: end == names.len(),
    })
}

fn xdr_opaque_len(len: usize) -> usize {
    4 + len + ((4 - (len % 4)) % 4)
}
