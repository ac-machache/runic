mod def;
mod local;

#[cfg(feature = "cloud")]
mod cloud;

pub use def::SkillSource;
pub use local::local;

#[cfg(feature = "cloud")]
pub use cloud::{CloudSource, from_operator};

#[cfg(feature = "azblob")]
pub use cloud::azblob;
#[cfg(feature = "gcs")]
pub use cloud::gcs;
#[cfg(feature = "s3")]
pub use cloud::s3;
