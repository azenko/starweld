//! Compact a VHDX in qemu-img-style: detect zero blocks (with a configurable
//! sparse-size threshold matching qemu-img's `-S`) and repack into a fresh
//! contiguous dynamic VHDX.

use std::path::{Path, PathBuf};

use tempfile::Builder;

use crate::disk::{BlockRead, VhdxDisk};
use crate::error::Result;
use crate::repair::{fsck, FsckOptions};
use crate::writer::{BlockSource, VhdxWriter, WriterParams};

#[derive(Debug, Clone, Copy)]
pub struct CompactOptions {
    /// Mirror of `qemu-img convert -S`: minimum zero-byte run (in bytes) to
    /// treat as sparse. Default: 4 KiB. Set to 0 to disable zero detection
    /// entirely (i.e. preserve every block as-is).
    pub sparse_size: u64,
    pub in_place: bool,
    pub skip_fsck: bool,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            sparse_size: 4096,
            in_place: false,
            skip_fsck: false,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactReport {
    pub input: PathBuf,
    pub output: PathBuf,
    pub blocks_total: u64,
    pub blocks_written: u64,
    pub blocks_zero: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

pub fn compact<P: AsRef<Path>>(path: P, opts: CompactOptions) -> Result<CompactReport> {
    let in_path = path.as_ref().to_path_buf();
    if !opts.skip_fsck {
        fsck(
            &in_path,
            FsckOptions {
                fix: true,
                dry_run: false,
                allow_log_discard: true,
            },
        )?;
    }

    let (disk, mut file) = VhdxDisk::open(&in_path, true)?;
    if disk.has_parent() {
        return Err(crate::error::Error::Unsupported(
            "compact does not operate on differencing disks; flatten first".into(),
        ));
    }

    let block_size = disk.block_size();
    let block_count = disk.block_count();
    let bytes_in = file.metadata()?.len();

    // Build temp output beside the source.
    let dir = in_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let tmp = Builder::new()
        .prefix(".starweld-compact-")
        .suffix(".vhdx")
        .tempfile_in(&dir)?;
    let tmp_path = tmp.path().to_path_buf();
    drop(tmp);

    let params = WriterParams {
        virtual_disk_size: disk.virtual_disk_size(),
        block_size,
        logical_sector_size: disk.metadata.logical_sector_size,
        physical_sector_size: disk.metadata.physical_sector_size,
        fixed: false,
        parent_locator: None,
        log_size: 1024 * 1024,
        data_write_guid: uuid::Uuid::new_v4(),
        file_write_guid: uuid::Uuid::new_v4(),
        page_83_data: disk.metadata.page_83_data,
    };
    let mut writer = VhdxWriter::create(&tmp_path, params)?;

    let mut report = CompactReport {
        input: in_path.clone(),
        output: in_path.clone(),
        blocks_total: block_count,
        blocks_written: 0,
        blocks_zero: 0,
        bytes_in,
        bytes_out: 0,
    };

    for blk in 0..block_count {
        let read = disk.read_block(&mut file, blk)?;
        let buf = match read {
            BlockRead::Absent => {
                writer.write_block(blk, BlockSource::Zero)?;
                report.blocks_zero += 1;
                continue;
            }
            BlockRead::Full(b) => b,
            BlockRead::Partial { payload, .. } => payload,
        };
        if opts.sparse_size > 0 && is_all_zero_aligned(&buf, opts.sparse_size as usize) {
            writer.write_block(blk, BlockSource::Zero)?;
            report.blocks_zero += 1;
        } else {
            writer.write_block(blk, BlockSource::Buf(&buf))?;
            report.blocks_written += 1;
        }
    }

    writer.finish()?;

    let bytes_out = std::fs::metadata(&tmp_path)?.len();
    report.bytes_out = bytes_out;

    if opts.in_place {
        // Atomic replace.
        std::fs::rename(&tmp_path, &in_path)?;
        report.output = in_path.clone();
    } else {
        // Place next to original with `.compacted.vhdx` suffix.
        let mut out = in_path.clone();
        let stem = in_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "image".into());
        out.set_file_name(format!("{stem}.compacted.vhdx"));
        std::fs::rename(&tmp_path, &out)?;
        report.output = out;
    }

    Ok(report)
}

/// `qemu-img -S` semantics: scan in `granularity`-byte chunks; if every chunk
/// is all-zero, treat the buffer as sparse. With granularity = 0 we never
/// declare a buffer sparse.
pub fn is_all_zero_aligned(buf: &[u8], granularity: usize) -> bool {
    if granularity == 0 {
        return false;
    }
    if buf.is_empty() {
        return true;
    }
    let g = granularity.max(1);
    let mut i = 0;
    while i < buf.len() {
        let end = (i + g).min(buf.len());
        if !buf[i..end].iter().all(|&b| b == 0) {
            return false;
        }
        i += g;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_check() {
        assert!(is_all_zero_aligned(&[0u8; 8192], 4096));
        let mut b = vec![0u8; 8192];
        b[5000] = 1;
        assert!(!is_all_zero_aligned(&b, 4096));
    }

    #[test]
    fn granularity_zero_disables() {
        assert!(!is_all_zero_aligned(&[0u8; 16], 0));
    }
}
