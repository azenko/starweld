// starweld-core — engine library for the starweld CLI.
//
// Author : Asuka Zenko <contact@gungnirnet.eu>
// Version: 1.0.0
// License: MIT OR Apache-2.0
//
// Made with love using Cursor (https://cursor.com).

#![forbid(unsafe_code)]
#![doc = include_str!("../../../README.md")]

/// Crate-level authorship and version information, exposed for tools that want
/// to print a banner.
pub const AUTHOR: &str = "Asuka Zenko <contact@gungnirnet.eu>";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TAGLINE: &str = "Made with love using Cursor.";

pub mod bat;
pub mod chain;
pub mod compact;
pub mod compress;
pub mod crc32c;
pub mod disk;
pub mod error;
pub mod flatten;
pub mod format;
pub mod guid;
pub mod header;
pub mod io_util;
pub mod log;
pub mod merge;
pub mod metadata;
pub mod parent_locator;
pub mod region;
pub mod repair;
pub mod tar_header;
pub mod writer;
pub mod zst_compress;
pub mod zst_writer;

pub use chain::Chain;
pub use disk::VhdxDisk;
pub use error::{Error, Result};
