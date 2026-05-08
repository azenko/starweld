//! Merge a leaf AVHDX into its immediate parent (one chain link).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::bat::{Bat, BatEntry, PayloadState};
use crate::chain::{resolve_parent, Chain};
use crate::disk::{bit_is_set, BlockRead, VhdxDisk};
use crate::error::{Error, Result};
use crate::format::PAYLOAD_BLOCK_FULLY_PRESENT;
use crate::header::HeaderPair;
use crate::io_util::{drop_page_cache, read_exact_at, write_all_at};
use crate::repair::{fsck, FsckOptions};

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeOptions {
    pub keep_child: bool,
    pub skip_fsck: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MergeReport {
    pub child: PathBuf,
    pub parent: PathBuf,
    pub blocks_copied: u64,
    pub partial_sectors_copied: u64,
    pub deleted_child: bool,
}

/// Coarse-grained progress events emitted by [`merge_with_progress`].
///
/// The callback is invoked synchronously on the same thread that drives the
/// merge, so it should be cheap and non-blocking (a UI frame update is
/// fine, a network call is not).
#[derive(Debug, Clone, Copy)]
pub enum MergeEvent {
    /// Emitted exactly once, just before the first block is read from the
    /// child. `total_bytes` is the virtual disk size rounded up to a whole
    /// number of blocks, i.e. `total_blocks * block_size`.
    Started {
        total_blocks: u64,
        block_size: u32,
        total_bytes: u64,
    },
    /// Emitted after each child block has been processed. `bytes` is the
    /// size of that block, so the caller can drive a byte-granular
    /// progress bar with a simple running counter. `wrote_bytes` is `true`
    /// when the child block was `FullyPresent` or `PartiallyPresent` (data
    /// was copied into the parent), and `false` when the child block was
    /// `Absent` (nothing changed in the parent for that block).
    BlockDone {
        index: u64,
        bytes: u64,
        wrote_bytes: bool,
    },
    /// Emitted exactly once when the parent has been fully written and
    /// fsynced.
    Finished,
}

/// Convenience wrapper around [`merge_with_progress`] for callers that
/// don't care about progress events.
pub fn merge<P: AsRef<Path>>(child: P, opts: MergeOptions) -> Result<MergeReport> {
    merge_with_progress(child, opts, |_| {})
}

/// Like [`merge`], but invokes `on_event` at well-defined milestones (see
/// [`MergeEvent`]) so the caller can drive a progress bar, log telemetry,
/// or stream updates over IPC.
pub fn merge_with_progress<P, F>(
    child: P,
    opts: MergeOptions,
    mut on_event: F,
) -> Result<MergeReport>
where
    P: AsRef<Path>,
    F: FnMut(MergeEvent),
{
    let child_path = child.as_ref().to_path_buf();

    if !opts.skip_fsck {
        fsck(
            &child_path,
            FsckOptions {
                fix: true,
                dry_run: false,
                allow_log_discard: true,
            },
        )?;
    }

    let (child_disk, mut child_file) = VhdxDisk::open(&child_path, true)?;
    if !child_disk.has_parent() {
        return Err(Error::InvalidStructure(format!(
            "{} is not a differencing disk; nothing to merge",
            child_path.display()
        )));
    }
    let pl = child_disk
        .metadata
        .parent_locator
        .as_ref()
        .ok_or_else(|| Error::InvalidStructure("HasParent set but no parent locator".into()))?;
    let parent_path = resolve_parent(&child_path, pl).ok_or_else(|| Error::ParentNotFound {
        child: child_path.clone(),
        reason: "parent locator did not resolve to an existing file".into(),
    })?;

    if !opts.skip_fsck {
        fsck(
            &parent_path,
            FsckOptions {
                fix: true,
                dry_run: false,
                allow_log_discard: true,
            },
        )?;
    }

    // Verify the parent's DataWriteGuid matches the linkage.
    let (parent_disk, _ph_file) = VhdxDisk::open(&parent_path, true)?;
    if let Some(expected) = pl.parent_linkage() {
        if parent_disk.active_header().data_write_guid != expected {
            return Err(Error::ParentGuidMismatch {
                child: child_path.clone(),
                expected,
                actual: parent_disk.active_header().data_write_guid,
            });
        }
    }
    if parent_disk.virtual_disk_size() != child_disk.virtual_disk_size() {
        return Err(Error::InvalidStructure(format!(
            "virtual disk size mismatch: child={}, parent={}",
            child_disk.virtual_disk_size(),
            parent_disk.virtual_disk_size()
        )));
    }
    let bs = child_disk.block_size() as u64;
    if parent_disk.block_size() as u64 != bs {
        return Err(Error::InvalidStructure(format!(
            "block size mismatch: child={}, parent={}",
            child_disk.block_size(),
            parent_disk.block_size()
        )));
    }
    // The merge loop copies child sectors directly onto parent sectors at
    // matching byte offsets, so a logical-sector-size mismatch would
    // silently shift every PartiallyPresent block by the difference. The
    // VHDX spec mandates that all chain links share the same logical
    // sector size, but Hyper-V is not always strict about it, so we reject
    // the combination explicitly.
    if parent_disk.metadata.logical_sector_size != child_disk.metadata.logical_sector_size {
        return Err(Error::InvalidStructure(format!(
            "logical sector size mismatch: child={}, parent={}",
            child_disk.metadata.logical_sector_size, parent_disk.metadata.logical_sector_size
        )));
    }
    drop(parent_disk);

    // Open parent for write.
    let mut parent_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&parent_path)?;
    crate::header::verify_file_type(&mut parent_file)?;
    let parent_header_pair = HeaderPair::read(&mut parent_file)?;
    let parent_region = crate::region::RegionTablePair::read(&mut parent_file)?;
    let parent_region_active = parent_region.active().clone();
    let parent_bat_region = *parent_region_active
        .bat()
        .ok_or(Error::MissingRegion("BAT"))?;
    let parent_metadata_region = *parent_region_active
        .metadata()
        .ok_or(Error::MissingRegion("Metadata"))?;
    let mut parent_metadata_buf = vec![0u8; parent_metadata_region.length as usize];
    read_exact_at(
        &mut parent_file,
        parent_metadata_region.file_offset,
        &mut parent_metadata_buf,
    )?;
    let parent_metadata = crate::metadata::Metadata::parse(&parent_metadata_buf)?;
    let mut parent_bat_buf = vec![0u8; parent_bat_region.length as usize];
    read_exact_at(
        &mut parent_file,
        parent_bat_region.file_offset,
        &mut parent_bat_buf,
    )?;
    let mut parent_bat = Bat::parse(
        &parent_bat_buf,
        parent_metadata.block_count(),
        parent_metadata.chunk_ratio(),
        parent_metadata.file_parameters.has_parent,
    );

    let mut report = MergeReport {
        child: child_path.clone(),
        parent: parent_path.clone(),
        blocks_copied: 0,
        partial_sectors_copied: 0,
        deleted_child: false,
    };

    let mut payload_cursor = parent_file.metadata()?.len();
    // Round payload_cursor up to the next 1 MiB boundary so new blocks start
    // aligned (a VHDX requirement for FullyPresent blocks).
    let aligned = crate::io_util::align_up(payload_cursor, 1024 * 1024);
    if aligned != payload_cursor {
        parent_file.set_len(aligned)?;
        payload_cursor = aligned;
    }

    let block_count = child_disk.block_count();
    let logical_sector_size = child_disk.metadata.logical_sector_size as u64;
    let sectors_per_block = (bs / logical_sector_size) as usize;

    on_event(MergeEvent::Started {
        total_blocks: block_count,
        block_size: bs as u32,
        total_bytes: block_count.saturating_mul(bs),
    });

    // Number of leaf blocks between two `POSIX_FADV_DONTNEED` rounds. We
    // batch the syscalls so the per-block overhead stays negligible while
    // still capping the page-cache footprint of both the child reads and
    // the parent writes to roughly `FADV_BATCH * block_size` bytes per
    // file at any instant. 16 * 32 MiB = 512 MiB worth of cached pages —
    // well under any sane host's RAM budget while still letting the
    // kernel batch I/O sensibly.
    const FADV_BATCH: u64 = 16;

    for blk in 0..block_count {
        let read = child_disk.read_block(&mut child_file, blk)?;
        let wrote_bytes = !matches!(read, BlockRead::Absent);
        match read {
            BlockRead::Absent => {}
            BlockRead::Full(data) => {
                // Allocate / overwrite a parent block.
                let off = match parent_bat
                    .payload_entry(blk)
                    .map(|e| (PayloadState::from_raw(e.state()), e.file_offset()))
                {
                    Some((PayloadState::FullyPresent, off)) if off != 0 => off,
                    _ => {
                        let o = payload_cursor;
                        payload_cursor += bs;
                        parent_file.set_len(payload_cursor)?;
                        o
                    }
                };
                write_all_at(&mut parent_file, off, &data)?;
                parent_bat.set_payload_entry(blk, BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, off));
                report.blocks_copied += 1;
            }
            BlockRead::Partial { payload, bitmap } => {
                let off = match parent_bat
                    .payload_entry(blk)
                    .map(|e| (PayloadState::from_raw(e.state()), e.file_offset()))
                {
                    Some((PayloadState::FullyPresent, off)) if off != 0 => off,
                    _ => {
                        let o = payload_cursor;
                        payload_cursor += bs;
                        parent_file.set_len(payload_cursor)?;
                        o
                    }
                };
                // For each sector in the child that is "present", copy it onto
                // the parent's block (which already contains parent's data, or
                // we fetched it via VhdxDisk above).
                let mut parent_block = vec![0u8; bs as usize];
                if let Some(parent_existing) = parent_bat
                    .payload_entry(blk)
                    .filter(|e| {
                        matches!(
                            PayloadState::from_raw(e.state()),
                            PayloadState::FullyPresent
                        )
                    })
                    .map(|e| e.file_offset())
                {
                    if parent_existing == off {
                        read_exact_at(&mut parent_file, off, &mut parent_block)?;
                    }
                }
                for s in 0..sectors_per_block {
                    if bit_is_set(&bitmap, s) {
                        let lo = s * logical_sector_size as usize;
                        let hi = lo + logical_sector_size as usize;
                        parent_block[lo..hi].copy_from_slice(&payload[lo..hi]);
                        report.partial_sectors_copied += 1;
                    }
                }
                write_all_at(&mut parent_file, off, &parent_block)?;
                parent_bat.set_payload_entry(blk, BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, off));
                report.blocks_copied += 1;
            }
        }

        on_event(MergeEvent::BlockDone {
            index: blk,
            bytes: bs,
            wrote_bytes,
        });

        // Periodically tell the kernel it can drop the cached pages we've
        // touched on both files. Without this, every byte of the child
        // and every byte we've appended to the parent would stay resident
        // for the duration of the merge; on memory-constrained hosts that
        // squeezes out the rest of the system's working set and the user
        // has no recourse short of `echo 3 > /proc/sys/vm/drop_caches`
        // (which requires root).
        if (blk + 1) % FADV_BATCH == 0 || blk + 1 == block_count {
            drop_page_cache(&child_file, 0, 0);
            drop_page_cache(&parent_file, 0, 0);
        }
    }

    // Persist parent BAT.
    let mut new_bat_buf = vec![0u8; parent_bat_region.length as usize];
    let raw = parent_bat.encode();
    new_bat_buf[..raw.len()].copy_from_slice(&raw);
    write_all_at(
        &mut parent_file,
        parent_bat_region.file_offset,
        &new_bat_buf,
    )?;

    // Bump parent DataWriteGuid (spec requirement: a parent that has been
    // mutated must advertise a new DataWriteGuid). And FileWriteGuid as well,
    // since the file content changed.
    let new_data_guid = Uuid::new_v4();
    let new_file_guid = Uuid::new_v4();
    HeaderPair::write_new(&mut parent_file, parent_header_pair.active(), |h| {
        h.data_write_guid = new_data_guid;
        h.file_write_guid = new_file_guid;
        h.log_guid = Uuid::nil();
    })?;
    HeaderPair::write_new(&mut parent_file, parent_header_pair.active(), |h| {
        h.data_write_guid = new_data_guid;
        h.file_write_guid = new_file_guid;
        h.log_guid = Uuid::nil();
    })?;
    parent_file.sync_all()?;

    drop(parent_file);
    drop(child_file);

    if !opts.keep_child {
        std::fs::remove_file(&child_path)?;
        report.deleted_child = true;
    }

    let _ = Chain::discover(&parent_path)?;

    on_event(MergeEvent::Finished);

    Ok(report)
}
