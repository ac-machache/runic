//! `Tenant` extractor — pulls the value of `X-Runic-Tenant` off the
//! request (or falls back to `"default"`).
//!
//! This is the lightest possible auth surface: we trust the header.
//! Behind a real gateway this is fine; for production you'd swap this
//! out for a proper auth middleware that validates a token and emits
//! the same `Tenant` extension.

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use std::convert::Infallible;

pub const TENANT_HEADER: &str = "x-runic-tenant";
pub const DEFAULT_TENANT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant(pub String);

impl Tenant {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Tenant {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        let value = parts
            .headers
            .get(TENANT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_TENANT.to_string());
        Ok(Tenant(value))
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for Tenant {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        let Tenant(t) = <Tenant as FromRequestParts<S>>::from_request_parts(parts, state).await?;
        Ok(Some(Tenant(t)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts_with_header(value: Option<&str>) -> Parts {
        let mut builder = Request::builder();
        if let Some(v) = value {
            builder = builder.header(TENANT_HEADER, v);
        }
        builder
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[tokio::test]
    async fn missing_header_yields_default() {
        let mut parts = parts_with_header(None);
        let Tenant(t) =
            <Tenant as FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .unwrap();
        assert_eq!(t, "default");
    }

    #[tokio::test]
    async fn header_value_is_extracted() {
        let mut parts = parts_with_header(Some("alice"));
        let Tenant(t) =
            <Tenant as FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .unwrap();
        assert_eq!(t, "alice");
    }

    #[tokio::test]
    async fn empty_header_falls_back_to_default() {
        let mut parts = parts_with_header(Some("   "));
        let Tenant(t) =
            <Tenant as FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .unwrap();
        assert_eq!(t, "default");
    }
}
