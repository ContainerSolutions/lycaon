
use thiserror::Error;
use std::io::Read;
use std::io::Seek;

pub use digest::{Digest, DigestAlgorithm};
pub use manifest_storage::{ManifestStorage, ManifestReader};
pub use blob_storage::{BlobStorage, ContentInfo, UploadInfo, BlobReader};
pub use catalog_operations::CatalogOperations;
pub use metrics::{Metrics, MetricsError};
pub use validation::{Validation, ValidationError};

#[allow(dead_code)]
pub mod digest;
pub mod blob_storage;
pub mod catalog_operations;
pub mod manifest_storage;
pub mod validation;
pub mod metrics;

// TODO: move types to interface
// Move below code to separate files

//==================================================================================================\

// Storage Driver Error
#[derive(Error, Debug)]
pub enum StorageDriverError {
    #[error("the name `{0}` is not valid")]
    InvalidName(String),
    #[error("manifest is not valid")]
    InvalidManifest,
    #[error("Digest did not match content")]
    InvalidDigest,
    #[error("Unsupported Operation")]
    Unsupported,
    #[error("Requested index does not match actual")]
    InvalidContentRange,
    #[error("Internal storage error")]
    Internal,
}


//If there's a better solution, please let me know.
//I'd much rather not have to write an impl for every class :(
    pub trait SeekRead: Read + Seek {}
    impl SeekRead for std::fs::File {}

// Super trait
pub trait RegistryStorage: ManifestStorage + BlobStorage + CatalogOperations {
    /// Whether the specific name(space) exists
    fn exists(&self, name: &String) -> Result<bool, StorageDriverError>;

    /// Whether the driver supports processing of data chunks in a streaming mode
    /// For example when the client uploads chunks of data, instead of buffering them
    /// in memory and then passing the full data, the driver can process single chunks
    /// individually. This significantly decrease the memory usage of the registry
    fn support_streaming(&self) -> bool;
}