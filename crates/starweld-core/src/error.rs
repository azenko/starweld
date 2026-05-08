use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a VHDX file: bad signature {found:?} at offset 0")]
    BadSignature { found: [u8; 8] },

    #[error("both VHDX headers are invalid (no valid CRC32C)")]
    NoValidHeader,

    #[error("both VHDX region tables are invalid (no valid CRC32C)")]
    NoValidRegionTable,

    #[error("required region missing: {0}")]
    MissingRegion(&'static str),

    #[error("required metadata item missing: {0}")]
    MissingMetadata(&'static str),

    #[error("invalid VHDX structure: {0}")]
    InvalidStructure(String),

    #[error("log replay failed: {0}")]
    LogReplay(String),

    #[error("the file has a non-empty log; run `fsck --fix` to replay or invalidate it")]
    DirtyLog,

    #[error("unsupported feature: {0}")]
    Unsupported(String),

    #[error("parent VHDX could not be located for {child}: {reason}")]
    ParentNotFound { child: PathBuf, reason: String },

    #[error(
        "parent DataWriteGuid mismatch for child {child}: expected {expected}, found {actual}"
    )]
    ParentGuidMismatch {
        child: PathBuf,
        expected: uuid::Uuid,
        actual: uuid::Uuid,
    },

    #[error("virtual offset {0:#x} is out of range")]
    VirtOffsetOutOfRange(u64),

    #[error("encountered an unmapped block at virtual offset {0:#x}")]
    UnmappedBlock(u64),

    #[error("integrity check failed: {0}")]
    Integrity(String),
}
