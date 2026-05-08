//! Flatten a parent>child chain into a fresh standalone VHDX.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bitvec::prelude::*;

use crate::bat::total_bat_entries;
use crate::chain::Chain;
use crate::disk::{bit_is_set, BlockRead, VhdxDisk};
use crate::error::{Error, Result};
use crate::io_util::{drop_page_cache, is_all_zero};
use crate::repair::{fsck, FsckOptions};
use crate::tar_header::{
    padding_to_block, write_ustar_eof, write_ustar_header, TAR_EOF_SIZE, TAR_HEADER_SIZE,
};
use crate::writer::{BlockSource, VhdxWriter, WriterParams};
use crate::zst_compress::{open_encoder, ZstdParams};
use crate::zst_writer::{build_bat_for_layout, Layout, ZstStreamWriter};

#[derive(Debug, Clone, Copy)]
pub enum FlattenSubformat {
    Dynamic,
    Fixed,
}

#[derive(Debug, Clone, Copy)]
pub struct FlattenOptions {
    pub subformat: FlattenSubformat,
    pub skip_fsck: bool,
    /// When true, blocks consisting entirely of zeros are stored as
    /// `PAYLOAD_BLOCK_ZERO` (sparse) instead of being allocated.
    pub elide_zero_blocks: bool,
    /// When true, request transparent NTFS LZNT1 compression on the output
    /// file before any payload block is written, so every cluster is
    /// compressed by the filesystem as it lands. Only effective on Windows
    /// when the destination volume is NTFS.
    ///
    /// While this is enabled, the flattener also:
    /// - issues each payload block's `WriteFile` in 1 MiB sub-chunks with a
    ///   `sync_data` between each chunk
    ///   (see [`VhdxWriter::set_chunked_write_size`](crate::writer::VhdxWriter::set_chunked_write_size)),
    ///   so NTFS only ever needs ~1 MiB of physically free clusters at any
    ///   instant — even though one VHDX block is up to 32 MiB and would
    ///   otherwise force the volume to reserve a full block's worth of
    ///   uncompressed-equivalent space until the lazy writer compresses it;
    /// - calls [`VhdxWriter::sync_data`](crate::writer::VhdxWriter::sync_data)
    ///   after every block as a final back-pressure point, so the OS write
    ///   cache cannot grow without bound on multi-GiB flattens;
    /// - then [`reopen`](crate::writer::VhdxWriter::reopen)s the file, so
    ///   the `CloseHandle` triggers NTFS's on-close maintenance pass and
    ///   the volume's free-space accounting is updated to reflect the
    ///   file's actual (compressed) on-disk footprint instead of its
    ///   uncompressed-equivalent peak — without that step, the next
    ///   `WriteFile` can fail with `ENOSPC` even though the file is in
    ///   reality far smaller than the uncompressed accounting suggests.
    pub compress_ntfs: bool,
    /// When `Some`, the flatten output is streamed directly into a
    /// `<output>.zst` zstd-compressed file with no intermediate uncompressed
    /// VHDX ever touching disk. The streaming writer in
    /// [`crate::zst_writer`] is used; a one-time pre-pass over the source
    /// chain detects zero blocks so the inner VHDX stays sparse. Implies
    /// the dynamic subformat (zstd-streaming a fixed image is rejected by
    /// the entry point) and is incompatible with [`Self::compress_ntfs`].
    pub compress_zstd: Option<ZstdParams>,
}

impl Default for FlattenOptions {
    fn default() -> Self {
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: None,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FlattenReport {
    pub leaf: PathBuf,
    pub output: PathBuf,
    pub blocks_total: u64,
    pub blocks_written: u64,
    pub blocks_zero: u64,
    /// `Some(path)` when [`FlattenOptions::compress_zstd`] was set: this is
    /// the final `.zst` artifact that landed on disk (`output` is then
    /// just the logical "uncompressed" name the user supplied).
    #[cfg_attr(feature = "serde", serde(default))]
    pub compressed_output: Option<PathBuf>,
    /// Total bytes of the inner (uncompressed) VHDX stream the writer
    /// produced. `0` on the non-zstd path.
    #[cfg_attr(feature = "serde", serde(default))]
    pub bytes_uncompressed: u64,
    /// Size of the resulting `.zst` file on disk. `0` on the non-zstd path.
    #[cfg_attr(feature = "serde", serde(default))]
    pub bytes_compressed: u64,
}

/// Coarse-grained progress events emitted by [`flatten_with_progress`].
///
/// The callback is invoked synchronously on the same thread that drives the
/// flatten, so it should be cheap and non-blocking (a UI frame update is
/// fine, a network call is not).
#[derive(Debug, Clone, Copy)]
pub enum FlattenEvent {
    /// Emitted exactly once, just before the first payload block is touched.
    /// `total_bytes` is the virtual disk size rounded up to a whole number of
    /// blocks, i.e. `total_blocks * block_size`.
    Started {
        total_blocks: u64,
        block_size: u32,
        total_bytes: u64,
    },
    /// Emitted after each payload block has been processed (written or
    /// recorded as a sparse zero block). `bytes` is the size of that block,
    /// so the caller can drive a byte-granular progress bar with a simple
    /// running counter.
    BlockDone {
        index: u64,
        bytes: u64,
        wrote_bytes: bool,
    },
    /// Emitted exactly once when the writer has been finalized successfully.
    Finished,

    // ---- Events emitted only on the `compress_zstd` path ----
    //
    /// Emitted at the very start of the zstd flatten, before any source
    /// payload byte is touched. Drives the "scanning chain" progress phase.
    ScanStarted { total_bytes: u64 },
    /// Emitted periodically during the pre-scan with the running total of
    /// payload bytes that have been read from the source chain so far.
    ScanProgress { bytes_scanned: u64 },
    /// Emitted exactly once when the pre-scan has finished, just before the
    /// zstd encoder starts producing bytes. The caller can use this to
    /// reset the progress bar to the "writing + zstd" phase whose target
    /// length is `non_zero_blocks * block_size`.
    ScanFinished {
        non_zero_blocks: u64,
        zero_blocks: u64,
    },
}

/// Convenience wrapper around [`flatten_with_progress`] for callers that
/// don't care about progress events.
pub fn flatten<P1: AsRef<Path>, P2: AsRef<Path>>(
    leaf: P1,
    output: P2,
    opts: FlattenOptions,
) -> Result<FlattenReport> {
    flatten_with_progress(leaf, output, opts, |_| {})
}

/// Like [`flatten`], but invokes `on_event` at well-defined milestones (see
/// [`FlattenEvent`]) so the caller can drive a progress bar, log telemetry,
/// or stream updates over IPC.
pub fn flatten_with_progress<P1, P2, F>(
    leaf: P1,
    output: P2,
    opts: FlattenOptions,
    mut on_event: F,
) -> Result<FlattenReport>
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
    F: FnMut(FlattenEvent),
{
    // Route to the streaming zstd flow when requested. The zstd path uses a
    // completely separate writer with no random-access seeks; everything is
    // emitted into the encoder in strict ascending offset order.
    if let Some(zstd_params) = opts.compress_zstd {
        return flatten_to_zst(leaf, output, opts, zstd_params, on_event);
    }

    let leaf_path = leaf.as_ref().to_path_buf();
    let output_path = output.as_ref().to_path_buf();

    if !opts.skip_fsck {
        fsck(
            &leaf_path,
            FsckOptions {
                fix: true,
                dry_run: false,
                allow_log_discard: true,
            },
        )?;
    }

    let chain = Chain::discover(&leaf_path)?;
    if !opts.skip_fsck {
        for node in &chain.nodes {
            fsck(
                &node.path,
                FsckOptions {
                    fix: true,
                    dry_run: false,
                    allow_log_discard: true,
                },
            )?;
        }
    }

    // Open every chain link in read-only mode and keep their files open.
    let mut links: Vec<(VhdxDisk, File)> = Vec::with_capacity(chain.nodes.len());
    for node in &chain.nodes {
        links.push(VhdxDisk::open(&node.path, true)?);
    }

    let leaf = &links[0].0;
    let virtual_disk_size = leaf.virtual_disk_size();
    let block_size = leaf.block_size();
    let logical_sector_size = leaf.metadata.logical_sector_size;
    let physical_sector_size = leaf.metadata.physical_sector_size;
    let block_count = leaf.block_count();

    // The VHDX spec recommends that all links in a chain share the same
    // virtual disk size and sector size, but Hyper-V occasionally produces
    // chains where the differencing AVHDX has a smaller `block_size` than its
    // VHDX parent (e.g. 2 MiB leaf vs 32 MiB parent). We must handle that;
    // however virtual_disk_size and logical_sector_size MUST match for the
    // sector-level merging logic below to be valid.
    for (disk, _) in &links[1..] {
        if disk.virtual_disk_size() != virtual_disk_size {
            return Err(Error::InvalidStructure(format!(
                "virtual disk size mismatch in chain: leaf={} bytes, parent {} = {} bytes",
                virtual_disk_size,
                disk.path.display(),
                disk.virtual_disk_size()
            )));
        }
        if disk.metadata.logical_sector_size != logical_sector_size {
            return Err(Error::InvalidStructure(format!(
                "logical sector size mismatch in chain: leaf={} bytes, parent {} = {} bytes",
                logical_sector_size,
                disk.path.display(),
                disk.metadata.logical_sector_size
            )));
        }
    }

    let params = WriterParams {
        virtual_disk_size,
        block_size,
        logical_sector_size,
        physical_sector_size,
        fixed: matches!(opts.subformat, FlattenSubformat::Fixed),
        parent_locator: None,
        log_size: 1024 * 1024,
        data_write_guid: uuid::Uuid::new_v4(),
        file_write_guid: uuid::Uuid::new_v4(),
        page_83_data: leaf.metadata.page_83_data,
    };

    // Stamp the NTFS compression attribute on the (still empty) destination
    // before the writer fills it in: that way every cluster the writer later
    // emits is transparently LZNT1-compressed by the filesystem instead of
    // having to be re-read and re-written after the fact.
    if opts.compress_ntfs {
        crate::compress::set_ntfs_compression(&output_path)?;
    }

    let mut writer = VhdxWriter::create(&output_path, params)?;

    // On a compressed destination, write each payload block in 1 MiB
    // sub-chunks with a sync_data between each. NTFS reserves clusters
    // against the *uncompressed* size of every WriteFile call until the
    // data is committed and compressed, so without chunking a single
    // 32 MiB block write would briefly require 32 MiB of physically free
    // space on the volume — enough to trigger ENOSPC near the end of a
    // tight flatten even when the eventual on-disk footprint comfortably
    // fits.
    if opts.compress_ntfs {
        const NTFS_COMPRESSED_CHUNK: u32 = 1 << 20;
        writer.set_chunked_write_size(NTFS_COMPRESSED_CHUNK);
    }

    let mut report = FlattenReport {
        leaf: leaf_path.clone(),
        output: output_path.clone(),
        blocks_total: block_count,
        blocks_written: 0,
        blocks_zero: 0,
        compressed_output: None,
        bytes_uncompressed: 0,
        bytes_compressed: 0,
    };

    let leaf_block_size = block_size as u64;
    let sector_size = logical_sector_size as u64;
    let sectors_per_block = (leaf_block_size / sector_size) as usize;

    on_event(FlattenEvent::Started {
        total_blocks: block_count,
        block_size,
        total_bytes: block_count.saturating_mul(leaf_block_size),
    });

    for blk in 0..block_count {
        let v_start = blk * leaf_block_size;
        let mut composite = vec![0u8; leaf_block_size as usize];
        let mut sector_filled = vec![false; sectors_per_block];

        for (disk, file) in links.iter_mut() {
            fill_from_link(
                disk,
                file,
                v_start,
                &mut composite,
                &mut sector_filled,
                sector_size,
            )?;
            if sector_filled.iter().all(|&v| v) {
                break;
            }
        }

        let wrote_bytes = if opts.elide_zero_blocks
            && !matches!(opts.subformat, FlattenSubformat::Fixed)
            && is_all_zero(&composite)
        {
            writer.write_block(blk, BlockSource::Zero)?;
            report.blocks_zero += 1;
            false
        } else {
            writer.write_block(blk, BlockSource::Buf(&composite))?;
            report.blocks_written += 1;
            true
        };

        // When the destination is NTFS-compressed, force the OS to finish
        // compressing this block before we queue the next one. The chunked
        // writer has already `sync_data`'d every 1 MiB chunk, but we
        // additionally close and re-open the file handle here: dropping the
        // `File` invokes `CloseHandle`, which is the operation NTFS uses to
        // run its on-close maintenance pass and reconcile the volume's
        // free-space accounting against the file's *compressed* footprint.
        // Without this round-trip, long runs of extending writes through a
        // single handle can keep the volume's accounting stuck on the
        // uncompressed-equivalent estimate even after every chunk has been
        // flushed, and the very next `WriteFile` then fails with `ENOSPC`
        // while the file's size on disk leaves plenty of headroom.
        if wrote_bytes && opts.compress_ntfs {
            writer.sync_data()?;
            writer.reopen()?;
        }

        on_event(FlattenEvent::BlockDone {
            index: blk,
            bytes: leaf_block_size,
            wrote_bytes,
        });
    }

    writer.finish()?;
    on_event(FlattenEvent::Finished);
    Ok(report)
}

/// Fill the leaf-aligned virtual byte range `[v_start, v_start + composite.len())`
/// with data from `disk`. Only sectors that are not already marked in
/// `sector_filled` are considered, and only those for which `disk` actually has
/// payload (FullyPresent / Zero, or PartiallyPresent with the matching sector
/// bit set) are copied.
///
/// `disk` may have a different `block_size` than the leaf — this function
/// computes the overlap with each of `disk`'s blocks that intersect the leaf
/// range, so a 2 MiB leaf range can be served by a slice of a 32 MiB parent
/// block (or by several smaller parent blocks).
fn fill_from_link(
    disk: &VhdxDisk,
    file: &mut File,
    v_start: u64,
    composite: &mut [u8],
    sector_filled: &mut [bool],
    sector_size: u64,
) -> Result<()> {
    let leaf_len = composite.len() as u64;
    let v_end = v_start + leaf_len;
    let link_bs = disk.block_size() as u64;
    let link_block_count = disk.block_count();
    if link_block_count == 0 {
        return Ok(());
    }

    let first_link_blk = v_start / link_bs;
    let last_link_blk = (v_end - 1) / link_bs;

    for link_blk in first_link_blk..=last_link_blk {
        if link_blk >= link_block_count {
            break;
        }

        let link_v_start = link_blk * link_bs;
        let ov_start = v_start.max(link_v_start);
        let ov_end = v_end.min(link_v_start + link_bs);
        if ov_start >= ov_end {
            continue;
        }

        let dst_sec_lo = ((ov_start - v_start) / sector_size) as usize;
        let dst_sec_hi = ((ov_end - v_start) / sector_size) as usize;
        if (dst_sec_lo..dst_sec_hi).all(|s| sector_filled[s]) {
            continue;
        }

        let read = disk.read_block(file, link_blk)?;
        match read {
            BlockRead::Absent => continue,
            BlockRead::Full(data) => {
                // `s` is the *destination* sector index; we use it both to
                // gate the work (sector_filled[s]) and to derive the byte
                // offsets `dlo` / `dhi` into the composite buffer, so the
                // range-loop form is the clearest expression of the intent.
                #[allow(clippy::needless_range_loop)]
                for s in dst_sec_lo..dst_sec_hi {
                    if sector_filled[s] {
                        continue;
                    }
                    let dlo = s * sector_size as usize;
                    let dhi = dlo + sector_size as usize;
                    let src_v = v_start + dlo as u64;
                    let slo = (src_v - link_v_start) as usize;
                    let shi = slo + sector_size as usize;
                    composite[dlo..dhi].copy_from_slice(&data[slo..shi]);
                    sector_filled[s] = true;
                }
            }
            BlockRead::Partial { payload, bitmap } =>
            {
                #[allow(clippy::needless_range_loop)]
                for s in dst_sec_lo..dst_sec_hi {
                    if sector_filled[s] {
                        continue;
                    }
                    let dlo = s * sector_size as usize;
                    let dhi = dlo + sector_size as usize;
                    let src_v = v_start + dlo as u64;
                    let slo = (src_v - link_v_start) as usize;
                    let link_sector_idx = slo / sector_size as usize;
                    if bit_is_set(&bitmap, link_sector_idx) {
                        let shi = slo + sector_size as usize;
                        composite[dlo..dhi].copy_from_slice(&payload[slo..shi]);
                        sector_filled[s] = true;
                    }
                }
            }
        }
    }
    Ok(())
}

// =============================================================================
// Streaming-zstd flatten path
// =============================================================================

/// Merge one leaf-aligned block from the chain. Used by both the zero-block
/// pre-scan and the streaming-write phase of [`flatten_to_zst`]. The
/// returned buffer is exactly `leaf_block_size` bytes long.
fn merge_block(
    links: &mut [(VhdxDisk, File)],
    block_index: u64,
    leaf_block_size: u64,
    sector_size: u64,
    sectors_per_block: usize,
) -> Result<Vec<u8>> {
    let v_start = block_index * leaf_block_size;
    let mut composite = vec![0u8; leaf_block_size as usize];
    let mut sector_filled = vec![false; sectors_per_block];

    for (disk, file) in links.iter_mut() {
        fill_from_link(
            disk,
            file,
            v_start,
            &mut composite,
            &mut sector_filled,
            sector_size,
        )?;
        if sector_filled.iter().all(|&v| v) {
            break;
        }
    }
    Ok(composite)
}

/// Walk the chain once and return a bitmap whose `i`-th bit is set when the
/// composite of leaf block `i` (after sector-level merging through every
/// parent) is entirely zero.
///
/// The composite buffer is materialised per block but immediately dropped
/// after the zero-check; total RAM use is `O(block_size)`.
///
/// `on_progress(bytes_scanned)` is invoked once per block with the running
/// total of bytes that have been processed (bytes from the *virtual* address
/// space, i.e. `block_index * block_size`), so the caller can drive a
/// byte-granular progress bar.
fn analyze_chain_zero_blocks<F: FnMut(u64)>(
    links: &mut [(VhdxDisk, File)],
    block_count: u64,
    leaf_block_size: u64,
    sector_size: u64,
    sectors_per_block: usize,
    mut on_progress: F,
) -> Result<BitVec> {
    let mut is_zero: BitVec = BitVec::repeat(false, block_count as usize);
    let mut bytes_scanned: u64 = 0;

    // Number of leaf blocks between two `POSIX_FADV_DONTNEED` rounds. We
    // group the calls into batches so the per-block syscall overhead stays
    // negligible while still capping the page-cache footprint to roughly
    // `FADV_BATCH * leaf_block_size` bytes per chain link at any instant.
    // 16 * 32 MiB = 512 MiB worth of cached pages — well under any sane
    // host's RAM budget while still letting the kernel batch I/O sensibly.
    const FADV_BATCH: u64 = 16;

    for blk in 0..block_count {
        let composite = merge_block(links, blk, leaf_block_size, sector_size, sectors_per_block)?;
        if is_all_zero(&composite) {
            is_zero.set(blk as usize, true);
        }
        bytes_scanned = bytes_scanned.saturating_add(leaf_block_size);
        on_progress(bytes_scanned);

        // Periodically tell the kernel it can drop everything we've read
        // from each chain link so far. Without this, every byte of every
        // source link would stay resident in the page cache for the
        // duration of the pre-scan; on memory-constrained hosts that
        // squeezes out the rest of the system's working set and the user
        // has no recourse short of `echo 3 > /proc/sys/vm/drop_caches`
        // (which requires root).
        if (blk + 1) % FADV_BATCH == 0 || blk + 1 == block_count {
            for (_, file) in links.iter() {
                drop_page_cache(file, 0, 0);
            }
        }
    }
    Ok(is_zero)
}

/// Streaming-zstd flatten: collapse a chain into `<output>` (which the CLI
/// has already suffixed with `.zst`) by feeding every byte of a sparse VHDX
/// directly into a `zstd::Encoder` — no intermediate uncompressed file ever
/// touches the destination filesystem.
///
/// Implementation:
/// 1. Pass 1 walks the chain once and records, in a packed bitmap, which
///    leaf blocks merge to all-zero.
/// 2. The layout is computed (BAT-after-payload; see [`crate::zst_writer`]).
/// 3. The output `.zst` is opened, wrapped in a 4 MiB `BufWriter`, then in
///    a `zstd::Encoder` configured per `zstd_params`. A
///    [`ZstStreamWriter`] writes the file type identifier, both headers,
///    both region tables, the empty log and the metadata region into the
///    encoder.
/// 4. Pass 2 re-walks the chain in leaf-block-index order; for each
///    non-zero block it materialises the composite via [`merge_block`] and
///    feeds it into the writer. Zero blocks are skipped (their BAT entry
///    is already `PAYLOAD_BLOCK_ZERO`).
/// 5. The writer pads to the BAT region, writes the encoded BAT, and the
///    encoder is finished + the underlying file fsynced.
///
/// On error before `Encoder::finish` succeeds, the partial `.zst` is left
/// in place; the caller can simply remove it and retry.
pub fn flatten_to_zst<P1, P2, F>(
    leaf: P1,
    output: P2,
    opts: FlattenOptions,
    zstd_params: ZstdParams,
    mut on_event: F,
) -> Result<FlattenReport>
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
    F: FnMut(FlattenEvent),
{
    if opts.compress_ntfs {
        return Err(Error::Unsupported(
            "--compress-ntfs is incompatible with --zstd (compressing a .zst file is pointless)"
                .into(),
        ));
    }
    if matches!(opts.subformat, FlattenSubformat::Fixed) {
        return Err(Error::Unsupported(
            "--ty fixed is incompatible with --zstd (a fixed image would emit the entire virtual disk size as literal bytes into the encoder; use the default dynamic subformat)"
                .into(),
        ));
    }

    let leaf_path = leaf.as_ref().to_path_buf();
    let output_path = output.as_ref().to_path_buf();

    if !opts.skip_fsck {
        fsck(
            &leaf_path,
            FsckOptions {
                fix: true,
                dry_run: false,
                allow_log_discard: true,
            },
        )?;
    }

    let chain = Chain::discover(&leaf_path)?;
    if !opts.skip_fsck {
        for node in &chain.nodes {
            fsck(
                &node.path,
                FsckOptions {
                    fix: true,
                    dry_run: false,
                    allow_log_discard: true,
                },
            )?;
        }
    }

    let mut links: Vec<(VhdxDisk, File)> = Vec::with_capacity(chain.nodes.len());
    for node in &chain.nodes {
        links.push(VhdxDisk::open(&node.path, true)?);
    }

    let leaf_disk = &links[0].0;
    let virtual_disk_size = leaf_disk.virtual_disk_size();
    let block_size = leaf_disk.block_size();
    let logical_sector_size = leaf_disk.metadata.logical_sector_size;
    let physical_sector_size = leaf_disk.metadata.physical_sector_size;
    let block_count = leaf_disk.block_count();
    let page_83_data = leaf_disk.metadata.page_83_data;

    for (disk, _) in &links[1..] {
        if disk.virtual_disk_size() != virtual_disk_size {
            return Err(Error::InvalidStructure(format!(
                "virtual disk size mismatch in chain: leaf={} bytes, parent {} = {} bytes",
                virtual_disk_size,
                disk.path.display(),
                disk.virtual_disk_size()
            )));
        }
        if disk.metadata.logical_sector_size != logical_sector_size {
            return Err(Error::InvalidStructure(format!(
                "logical sector size mismatch in chain: leaf={} bytes, parent {} = {} bytes",
                logical_sector_size,
                disk.path.display(),
                disk.metadata.logical_sector_size
            )));
        }
    }

    let leaf_block_size = block_size as u64;
    let sector_size = logical_sector_size as u64;
    let sectors_per_block = (leaf_block_size / sector_size) as usize;
    let total_bytes = block_count.saturating_mul(leaf_block_size);

    // ---- Pass 1: pre-scan for zero blocks ----
    on_event(FlattenEvent::ScanStarted { total_bytes });
    let is_zero = analyze_chain_zero_blocks(
        &mut links,
        block_count,
        leaf_block_size,
        sector_size,
        sectors_per_block,
        |bytes_scanned| on_event(FlattenEvent::ScanProgress { bytes_scanned }),
    )?;
    let zero_blocks: u64 = is_zero.count_ones() as u64;
    let non_zero_blocks: u64 = block_count - zero_blocks;
    on_event(FlattenEvent::ScanFinished {
        non_zero_blocks,
        zero_blocks,
    });

    // ---- Layout + BAT ----
    let params = WriterParams {
        virtual_disk_size,
        block_size,
        logical_sector_size,
        physical_sector_size,
        fixed: false,
        parent_locator: None,
        log_size: 1024 * 1024,
        data_write_guid: uuid::Uuid::new_v4(),
        file_write_guid: uuid::Uuid::new_v4(),
        page_83_data,
    };
    let chunk_ratio = ((1u64 << 23) * params.logical_sector_size as u64) / leaf_block_size;
    let total_entries = total_bat_entries(block_count, chunk_ratio, false);
    let layout = Layout::compute(&params, non_zero_blocks, total_entries);
    let (bat, built_non_zero) = build_bat_for_layout(
        block_count,
        chunk_ratio,
        false,
        layout.payload_start,
        leaf_block_size,
        is_zero.iter().by_vals(),
    );
    debug_assert_eq!(built_non_zero, non_zero_blocks);

    // ---- Open destination + zstd encoder ----
    let dest_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&output_path)?;
    let buffered = BufWriter::with_capacity(4 << 20, dest_file);
    let mut encoder = open_encoder(buffered, zstd_params)?;

    // ---- Tar header (entry for the inner VHDX) ----
    //
    // We wrap the inner VHDX in a one-file POSIX ustar archive so the
    // resulting `.vhdx.tar.zst` is recognised as an archive by `tar`,
    // libarchive, Windows Explorer (24H2+), 7-Zip, etc. The header is
    // emitted into the encoder *before* the VHDX bytes; the encoder sees
    // a single linear stream of {header, payload, padding, eof}.
    //
    // The inner-archive filename is derived from the on-disk name the
    // user asked for, with any `.zst` / `.tar.zst` suffix stripped and a
    // `.vhdx` suffix appended if missing. Only the basename is used —
    // tar entries are conventionally relative paths.
    let inner_name = inner_archive_name(&output_path);
    let inner_size = layout.total_size;
    debug_assert_eq!(
        padding_to_block(inner_size),
        0,
        "inner VHDX size must be 512-aligned (1 MiB-aligned via Layout)"
    );
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_ustar_header(&mut encoder, &inner_name, inner_size, mtime)?;

    let mut writer = ZstStreamWriter::begin(encoder, &params, bat, layout)?;

    // ---- Pass 2: stream payload through encoder ----
    on_event(FlattenEvent::Started {
        total_blocks: block_count,
        block_size,
        total_bytes,
    });

    // Same FADV_DONTNEED batching as the pre-scan; see the comment in
    // `analyze_chain_zero_blocks` for the rationale.
    const PASS2_FADV_BATCH: u64 = 16;

    let mut blocks_written: u64 = 0;
    for blk in 0..block_count {
        if is_zero[blk as usize] {
            on_event(FlattenEvent::BlockDone {
                index: blk,
                bytes: leaf_block_size,
                wrote_bytes: false,
            });
        } else {
            let composite = merge_block(
                &mut links,
                blk,
                leaf_block_size,
                sector_size,
                sectors_per_block,
            )?;
            // The pre-scan said this block was non-zero; if a concurrent
            // reader has somehow zeroed it since then we'd silently waste
            // a payload slot — but the chain is opened read-only and we
            // do not support mutation under our feet, so this is purely
            // defensive.
            debug_assert!(!is_all_zero(&composite));
            writer.write_payload_block(&composite)?;
            blocks_written += 1;
            on_event(FlattenEvent::BlockDone {
                index: blk,
                bytes: leaf_block_size,
                wrote_bytes: true,
            });
        }

        // Drop cached source pages as we go. The pre-scan already walked
        // the entire chain, so without this every block we re-touch in
        // pass 2 would also stay resident.
        if (blk + 1) % PASS2_FADV_BATCH == 0 || blk + 1 == block_count {
            for (_, file) in links.iter() {
                drop_page_cache(file, 0, 0);
            }
        }
    }

    // ---- Finish: BAT, tar EOF, encoder footer, fsync ----
    let mut encoder = writer.finish()?;
    // Two zero blocks terminate the tar archive; the inner VHDX size is
    // already a multiple of 512 (asserted above) so no per-file padding
    // is required between the payload and these blocks.
    write_ustar_eof(&mut encoder)?;
    let buffered = encoder.finish()?;
    let mut dest_file = buffered.into_inner().map_err(|e| {
        // Bring the BufWriter's inner io::Error back through our error type.
        Error::Io(std::io::Error::other(format!(
            "failed to flush BufWriter to {}: {}",
            output_path.display(),
            e.error()
        )))
    })?;
    dest_file.flush()?;
    dest_file.sync_all()?;
    let bytes_compressed = dest_file.metadata()?.len();
    // We just wrote and fsync'd the destination .zst. The compressed bytes
    // are still pinned in the page cache; release them too so the caller
    // (typically a CLI invocation that exits immediately afterwards) does
    // not leave behind another N-MiB block of cached pages on top of the
    // pages we already evicted from the source side.
    drop_page_cache(&dest_file, 0, 0);
    drop(dest_file);

    on_event(FlattenEvent::Finished);

    // The tar wrapping adds one 512-byte header and two 512-byte zero
    // blocks of EOF marker around the inner VHDX, so the *uncompressed*
    // byte count seen by the encoder is the inner size plus that fixed
    // overhead. Reflecting this in `bytes_uncompressed` keeps the CLI's
    // compression-ratio math honest.
    let bytes_uncompressed = layout
        .total_size
        .saturating_add((TAR_HEADER_SIZE + TAR_EOF_SIZE) as u64);

    Ok(FlattenReport {
        leaf: leaf_path,
        output: output_path.clone(),
        blocks_total: block_count,
        blocks_written,
        blocks_zero: zero_blocks,
        compressed_output: Some(output_path),
        bytes_uncompressed,
        bytes_compressed,
    })
}

/// Derive the filename to embed inside the tar archive from the on-disk
/// `.tar.zst` path the CLI chose. The result is always a bare basename
/// (no directory components) ending in `.vhdx`, which is what the user
/// will see when they extract the archive.
fn inner_archive_name(output_path: &Path) -> String {
    // Start with the full file_name and strip any compression / archive
    // suffixes the CLI may have appended.
    let raw = output_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "disk.vhdx".to_string());

    let mut name = raw;
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.zst") {
        name.truncate(name.len() - ".tar.zst".len());
    } else if lower.ends_with(".zst") {
        name.truncate(name.len() - ".zst".len());
        let lower2 = name.to_ascii_lowercase();
        if lower2.ends_with(".tar") {
            name.truncate(name.len() - ".tar".len());
        }
    } else if lower.ends_with(".tar") {
        name.truncate(name.len() - ".tar".len());
    }

    // Make sure the inner filename has a `.vhdx` extension so users can
    // double-click the extracted file and have Windows / Hyper-V open it.
    if !name.to_ascii_lowercase().ends_with(".vhdx") {
        if name.is_empty() {
            name.push_str("disk");
        }
        name.push_str(".vhdx");
    }

    name
}

#[cfg(test)]
mod inner_name_tests {
    use super::inner_archive_name;
    use std::path::PathBuf;

    #[test]
    fn strips_tar_zst_suffix() {
        let p = PathBuf::from("/tmp/flat.vhdx.tar.zst");
        assert_eq!(inner_archive_name(&p), "flat.vhdx");
    }

    #[test]
    fn strips_zst_only_suffix() {
        let p = PathBuf::from("/tmp/flat.vhdx.zst");
        assert_eq!(inner_archive_name(&p), "flat.vhdx");
    }

    #[test]
    fn strips_zst_and_inner_tar() {
        let p = PathBuf::from("flat.tar.zst");
        assert_eq!(inner_archive_name(&p), "flat.vhdx");
    }

    #[test]
    fn appends_vhdx_when_missing() {
        let p = PathBuf::from("backup.tar.zst");
        assert_eq!(inner_archive_name(&p), "backup.vhdx");
    }

    #[test]
    fn keeps_existing_vhdx_basename() {
        let p = PathBuf::from("/var/backups/disk0.vhdx.tar.zst");
        assert_eq!(inner_archive_name(&p), "disk0.vhdx");
    }

    #[test]
    fn case_insensitive_extensions() {
        let p = PathBuf::from("DISK.VHDX.TAR.ZST");
        // Suffix stripping is case-insensitive; the surviving basename
        // keeps its original casing.
        assert_eq!(inner_archive_name(&p), "DISK.VHDX");
    }
}
