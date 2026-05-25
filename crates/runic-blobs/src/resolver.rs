//! [`BlobResolver`] — tiny trait the provider layer uses to fetch
//! blob bytes when materializing messages.
//!
//! Decoupled from [`BlobStore`] so providers don't have to know about
//! the full store API (or worry about tenants — the resolver bakes the
//! tenant in at construction). The agent loop wraps a `BlobStore` in a
//! `BlobStoreResolver` for the current tenant and hands the resolver
//! to the provider adapter.

use async_trait::async_trait;
use std::sync::Arc;

use crate::error::BlobError;
use crate::store::BlobStore;

/// Read-only access to blob bytes by id. The tenant is baked in at
/// construction; resolvers are per-(tenant, BlobStore) pair.
#[async_trait]
pub trait BlobResolver: Send + Sync {
    async fn resolve(&self, blob_id: &str) -> Result<Vec<u8>, BlobError>;
    async fn metadata(&self, blob_id: &str) -> Result<crate::BlobMetadata, BlobError>;
}

/// Default resolver: thin wrapper around a [`BlobStore`] + a fixed
/// tenant. The provider adapter sees just `Arc<dyn BlobResolver>`,
/// without needing to know about tenancy.
pub struct BlobStoreResolver {
    store: Arc<dyn BlobStore>,
    tenant: String,
}

impl BlobStoreResolver {
    pub fn new(store: Arc<dyn BlobStore>, tenant: impl Into<String>) -> Self {
        Self {
            store,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl BlobResolver for BlobStoreResolver {
    async fn resolve(&self, blob_id: &str) -> Result<Vec<u8>, BlobError> {
        self.store.read(&self.tenant, blob_id).await
    }

    async fn metadata(&self, blob_id: &str) -> Result<crate::BlobMetadata, BlobError> {
        self.store.metadata(&self.tenant, blob_id).await
    }
}

impl std::fmt::Debug for BlobStoreResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStoreResolver")
            .field("tenant", &self.tenant)
            .finish()
    }
}
