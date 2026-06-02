use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);

/// NFSv4.1 server identity returned by `EXCHANGE_ID`.
///
/// RFC 8881 clients use `server_owner` and `server_scope` to decide whether
/// two connections refer to the same server and may be trunked together. The
/// default identity is unique per [`crate::NfsServer`] instance to avoid
/// accidental trunking between independent localhost servers. Embedders that
/// intentionally expose the same server on multiple addresses should configure
/// a stable shared identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsServerIdentity {
    owner_major_id: Bytes,
    owner_minor_id: u64,
    scope: Bytes,
}

impl NfsServerIdentity {
    /// Creates an explicit NFSv4.1 server identity.
    pub fn new(
        owner_major_id: impl Into<Bytes>,
        owner_minor_id: u64,
        scope: impl Into<Bytes>,
    ) -> Self {
        Self {
            owner_major_id: owner_major_id.into(),
            owner_minor_id,
            scope: scope.into(),
        }
    }

    /// Returns the `server_owner.so_major_id` opaque value.
    pub fn owner_major_id(&self) -> &Bytes {
        &self.owner_major_id
    }

    /// Returns the `server_owner.so_minor_id` value.
    pub fn owner_minor_id(&self) -> u64 {
        self.owner_minor_id
    }

    /// Returns the `eir_server_scope` opaque value.
    pub fn scope(&self) -> &Bytes {
        &self.scope
    }

    /// Replaces the `server_owner.so_major_id` opaque value.
    pub fn with_owner_major_id(mut self, owner_major_id: impl Into<Bytes>) -> Self {
        self.owner_major_id = owner_major_id.into();
        self
    }

    /// Replaces the `server_owner.so_minor_id` value.
    pub fn with_owner_minor_id(mut self, owner_minor_id: u64) -> Self {
        self.owner_minor_id = owner_minor_id;
        self
    }

    /// Replaces the `eir_server_scope` opaque value.
    pub fn with_scope(mut self, scope: impl Into<Bytes>) -> Self {
        self.scope = scope.into();
        self
    }
}

impl Default for NfsServerIdentity {
    fn default() -> Self {
        let counter = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let major_id = Bytes::from(format!("embednfs:{}:{now}:{counter}", std::process::id()));
        Self {
            owner_major_id: major_id.clone(),
            owner_minor_id: 0,
            scope: major_id,
        }
    }
}
