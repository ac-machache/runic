mod def;
mod files;

pub use def::{Artifact, ArtifactSource, ArtifactStore};
pub use files::{ArtifactFiles, from_operator, local, memory};

#[cfg(feature = "azblob")]
pub use files::azblob;
#[cfg(feature = "gcs")]
pub use files::gcs;
#[cfg(feature = "s3")]
pub use files::s3;
