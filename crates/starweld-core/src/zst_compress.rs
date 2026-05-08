//! Thin wrapper around `zstd::stream::write::Encoder` with a small parameter
//! struct so the rest of the crate doesn't depend directly on the `zstd`
//! type names.

use std::io::Write;

use crate::error::Result;

/// Knobs for the zstd encoder.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ZstdParams {
    /// Compression level (`1`..=`22`). Default in the CLI is `3`.
    pub level: i32,
    /// Number of worker threads. `0` keeps the single-threaded encoder; a
    /// value `>=1` enables `ZSTD_c_nbWorkers` and runs the encoder
    /// asynchronously. Pass `num_cpus::get() as u32` to use every available
    /// logical core.
    pub threads: u32,
}

impl ZstdParams {
    pub fn new(level: i32, threads: u32) -> Self {
        ZstdParams { level, threads }
    }
}

/// Wrap `out` in a `zstd::stream::write::Encoder` configured per `params`.
///
/// The returned encoder writes a single zstd frame; call
/// [`zstd::stream::write::Encoder::finish`] to flush the frame footer when
/// done. Dropping the encoder without `finish`ing produces a truncated
/// `.zst` that decompressors will reject — the streaming flatten path
/// always calls `finish` explicitly.
pub fn open_encoder<W: Write>(
    out: W,
    params: ZstdParams,
) -> Result<zstd::stream::write::Encoder<'static, W>> {
    let mut enc = zstd::stream::write::Encoder::new(out, params.level)?;
    if params.threads >= 1 {
        // `multithread` calls into ZSTD_CCtx_setParameter(ZSTD_c_nbWorkers).
        // 1 already activates the worker thread pool (separate from the
        // application thread); higher values fan out further.
        enc.multithread(params.threads)?;
    }
    Ok(enc)
}
