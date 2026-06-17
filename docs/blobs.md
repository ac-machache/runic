# Blobs

`runic-blobs` is the **pluggable, content-addressed, multi-tenant** blob
storage layer for runic agents. Users upload files (images, PDFs, audio,
etc.); the conversation log carries only small references; the bytes
live in storage and are materialized to inline data only when a provider
needs them.

## Why not just inline base64 in messages?

That's what LangGraph (and most early-2025 frameworks) do. It breaks at scale:
- A 5MB image in 100 checkpoints = 500MB of duplicated bytes
- No deduplication, no GC, no streaming
- Checkpoint blobs balloon, replay slows down
- Tightly couples your data layout to the LLM provider's expected message format

We split it: byte storage is one concern (`BlobStore`), conversation
content is another (`ContentBlock::Blob` ref), provider format is a third
(materialized just-in-time, never persisted).

## The two primitives

### 1. `ContentBlock::Blob(BlobRef)` — the message-level reference

```rust
pub struct BlobRef {
    pub id: String,           // sha256 hex of the bytes
    pub mime: String,         // "image/png", "application/pdf", ...
    pub size: u64,
    pub name: Option<String>, // original filename — informational only
}
```

Lives in `runic-message-types`. Embedded directly in `Message` content,
just like `Text` or `Image`. Tiny — typically <200 bytes per blob,
regardless of the actual file size.

### 2. `BlobStore` trait — the storage abstraction

```rust
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, tenant: &str, input: BlobInput) -> Result<BlobRef, BlobError>;
    async fn read(&self, tenant: &str, blob_id: &str) -> Result<Vec<u8>, BlobError>;
    async fn metadata(&self, tenant: &str, blob_id: &str) -> Result<BlobMetadata, BlobError>;
    async fn exists(&self, tenant: &str, blob_id: &str) -> Result<bool, BlobError>;
    async fn delete(&self, tenant: &str, blob_id: &str) -> Result<(), BlobError>;
    async fn list(&self, tenant: &str) -> Result<Vec<BlobMetadata>, BlobError>;
}
```

Same shape as `SessionStore`: **multi-tenant first-class**. Every method
takes `tenant: &str`; tenants are storage-isolated even when they upload
identical bytes (they get the same id, but separate storage entries).

## Content addressing in one paragraph

`put` computes sha256 of the bytes. That hash IS the id. Uploading the
same bytes twice returns the same id — no extra storage used. The id
makes the blob tamper-evident (different bytes → different hash). It
also makes the blob deduplicate-by-default across all callers in the
same tenant.

## Reference impl: `FileBlobStore`

Built on `Arc<dyn StorageBackend>`. Layout:

```
{root}/{tenant}/{hash[:2]}/{full_hash}/
   ├─ data           ← the bytes
   └─ meta.json      ← { id, mime, size, name, tenant, uploaded_at }
```

The 2-character hash prefix avoids one giant flat directory.
Sidecar `meta.json` lets `metadata()` / `list()` work without reading
the (potentially huge) bytes.

Built on `StorageBackend` — so swapping LocalFs for S3 happens at the
storage layer, not here.

## Provider materialization

When you send a `Message` containing `ContentBlock::Blob(BlobRef)`,
the provider needs actual bytes (Anthropic wants base64-encoded image
blocks, Gemini wants `inlineData`, etc.). `BlobMaterializingProvider`
handles this as a decorator:

```rust
use runic_blobs::{BlobMaterializingProvider, BlobStoreResolver};

let raw_provider: Arc<dyn Provider> = AnthropicProvider::new(...);
let provider: Arc<dyn Provider> = Arc::new(BlobMaterializingProvider::new(
    raw_provider,
    Arc::new(BlobStoreResolver::new(blob_store.clone(), "alice")),
));
```

The wrapper walks every outgoing message, finds `Blob` references, fetches
bytes via the `BlobResolver`, base64-encodes them, and substitutes
`ContentBlock::Image` blocks in their place. The provider underneath sees
a "normal" message with inline image data — it has no idea blobs exist.

Failures during resolution log a warning and drop the blob (so a stale
reference doesn't crash the agent). Original messages in state stay
intact; only the per-turn provider input is rewritten.

**Works with any `Provider` impl.** Same wrapper handles Anthropic,
Gemini, and any future provider.

## Programmatic usage

Upload a file and embed it in a message:

```rust
use runic_blobs::{BlobInput, BlobStore, FileBlobStore};
use runic_message_types::{ContentBlock, Message, Role};

let store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(storage));

let bytes = tokio::fs::read("photo.png").await?;
let blob = store
    .put("alice", BlobInput::new(bytes, "image/png").with_name("photo.png"))
    .await?;

let msg = Message {
    role: Role::User,
    content: vec![
        ContentBlock::Blob(blob),
        ContentBlock::Text { text: "What's in this image?".into(), cache_control: None },
    ],
    timestamp: Some(chrono::Utc::now()),
    tool_duration_ms: None,
};
```

Then push that message into the agent's state and run a turn — the
materializing provider takes care of the rest.

See the runnable example: `cargo run --example with_blob -- path/to/image.png`.

## Multi-tenant in practice

For a multi-tenant server:

```rust
// Per-request: extract user from auth, build a tenant-scoped resolver
let user_id = req.user_id();
let resolver = Arc::new(BlobStoreResolver::new(blob_store.clone(), user_id.clone()));
let provider = Arc::new(BlobMaterializingProvider::new(raw_provider, resolver));

// Build a fresh agent for this turn
let mut agent = Agent::builder(provider).build();
```

Different users → different resolvers → different storage paths.
Alice's blob `abc123...` is at `blobs/alice/ab/abc123.../data`; Bob's
identical upload is at `blobs/bob/ab/abc123.../data`. Same content
hash, separate storage.

## Composability: store decorators

`BlobStore` is a trait — wrap one in another for cross-cutting concerns:

```rust
let store = Arc::new(FileBlobStore::new(storage));
let store = Arc::new(QuotaBlobStore::new(store, /* max bytes per tenant */));
let store = Arc::new(EncryptingBlobStore::new(store, kms_handle));
let store = Arc::new(MetricsBlobStore::new(store, prometheus_registry));
```

Same pattern as `tower::Layer` or our `ContextEngine` decorators.

| Decorator | What it adds |
|---|---|
| `QuotaBlobStore` | Enforce per-tenant size limits at `put` time |
| `TtlBlobStore` | Expire blobs after some time |
| `EncryptingBlobStore` | Encrypt at rest |
| `MetricsBlobStore` | Prometheus counters |
| `RoutingBlobStore` | tenant A → S3, tenant B → local FS |

## Binary integration

Wrap the provider in `BlobMaterializingProvider` so any
`ContentBlock::Blob` in a message — crafted programmatically or received
from a client — is resolved to bytes on the way to the provider, keyed by
the run's tenant. The `with_blob` example shows this end to end:

```sh
ANTHROPIC_API_KEY=sk-... cargo run --example with_blob -- path/to/image.png
```

## What's deferred

| Deferred | When to revisit |
|---|---|
| Multi-part HTTP upload endpoint on the server | When clients need to send files directly |
| Garbage collection of unreferenced blobs | When storage costs start mattering |
| Provider-specific Files API offloading (Anthropic/Gemini) | When inline base64 hits provider size limits |
| Vector indexing of blob contents | Different concern; would be a separate crate |

## Recommended reading

- `crates/runic-blobs/src/store.rs` — the `BlobStore` trait
- `crates/runic-blobs/src/file.rs` — `FileBlobStore` reference impl
  (content-addressing + sidecar metadata + 2-char prefix layout)
- `crates/runic-blobs/src/provider.rs` — `BlobMaterializingProvider` and
  the resolver-driven decoration pattern
- `crates/runic-blobs/src/resolver.rs` — the read-only `BlobResolver`
  trait + default `BlobStoreResolver`
- `crates/runic-blobs/tests/integration.rs` — 19 tests covering
  round-trip, dedup, tenant isolation, real-FS interaction
- `crates/runic-examples/examples/with_blob.rs` — end-to-end demo
