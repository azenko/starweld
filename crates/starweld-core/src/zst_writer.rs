//! Forward-only VHDX writer: emits a complete VHDX file into any [`Write`]
//! sink in strict ascending offset order, with no seeks and no rewrites.
//!
//! This is the writer used when the destination is a `zstd::Encoder`, where
//! random-access [`pwrite`][std::os::unix::fs::FileExt::write_at] is not
//! available because the encoded stream is not seekable.
//!
//! To make the layout streamable the BAT region is placed **after** the
//! payload region (the in-tree seekable writer in [`crate::writer`] uses the
//! more conventional BAT-before-payload layout). Per [MS-VHDX] §2.1.3 the
//! region table simply lists `(GUID, file_offset, length)` tuples and does
//! not constrain region ordering, so any spec-compliant reader (Hyper-V,
//! qemu-img, this crate's own [`crate::disk::VhdxDisk`]) accepts it.
//!
//! On-disk layout produced:
//! ```text
//! 0       .. 64K   : file type identifier (`vhdxfile` + zero pad)
//! 64K     ..128K   : header 1
//! 128K    ..192K   : header 2
//! 192K    ..256K   : region table 1   (points at metadata + BAT)
//! 256K    ..320K   : region table 2   (identical)
//! 320K    ..1MiB   : reserved zero pad
//! 1MiB    ..+log   : log region (zeros, never replayed by us)
//! +0      ..+1MiB  : metadata region
//! +0      ..+pld   : payload region (sequential non-zero blocks only)
//! +0      ..+bat   : BAT region (1 MiB-aligned, sized for total_entries)
//! ```

use std::io::Write;

use uuid::Uuid;

use crate::bat::{total_bat_entries, Bat, BatEntry};
use crate::error::{Error, Result};
use crate::format::{
    FILE_TYPE_IDENTIFIER_SIZE, FILE_TYPE_SIGNATURE, HEADER_1_OFFSET, HEADER_2_OFFSET, MIB,
    PAYLOAD_BLOCK_FULLY_PRESENT, PAYLOAD_BLOCK_ZERO, REGION_GUID_BAT, REGION_GUID_METADATA,
    REGION_TABLE_1_OFFSET, REGION_TABLE_2_OFFSET, REGION_TABLE_SIZE,
};
use crate::header::Header;
use crate::io_util::align_up;
use crate::metadata::{FileParameters, Metadata};
use crate::region::{RegionEntry, RegionTable};
use crate::writer::WriterParams;

/// Pre-computed file offsets of every region. Stable for the lifetime of the
/// writer.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub block_size: u64,
    pub log_offset: u64,
    pub log_size: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub payload_start: u64,
    pub bat_offset: u64,
    pub bat_length: u64,
    pub non_zero_blocks: u64,
    pub total_size: u64,
}

impl Layout {
    /// Compute the layout from writer params plus the number of non-zero
    /// payload blocks the caller plans to emit. `bat_entries` is the total
    /// number of BAT slots required for the chain (driven by `block_count`,
    /// `chunk_ratio` and `has_parent`); the writer needs it to size the BAT
    /// region.
    pub fn compute(params: &WriterParams, non_zero_blocks: u64, bat_entries: usize) -> Self {
        let block_size = params.block_size as u64;
        let log_offset = MIB;
        let log_size = align_up(params.log_size, MIB);
        let metadata_offset = log_offset + log_size;
        let metadata_length = MIB;
        let payload_start = metadata_offset + metadata_length;
        let payload_end = payload_start + non_zero_blocks * block_size;
        let bat_offset = align_up(payload_end, MIB);
        let bat_bytes_needed = (bat_entries as u64) * 8;
        let bat_length = align_up(bat_bytes_needed.max(MIB), MIB);
        let total_size = bat_offset + bat_length;
        Layout {
            block_size,
            log_offset,
            log_size,
            metadata_offset,
            metadata_length,
            payload_start,
            bat_offset,
            bat_length,
            non_zero_blocks,
            total_size,
        }
    }
}

/// Forward-only VHDX writer driven by a precomputed [`Layout`] and a fully
/// populated [`Bat`].
///
/// Usage:
/// 1. [`ZstStreamWriter::begin`] writes the file type identifier, both
///    headers, both region tables, padding up to the log, the empty log and
///    the metadata region. After it returns, the cursor is parked at
///    [`Layout::payload_start`].
/// 2. The caller invokes [`Self::write_payload_block`] exactly once per
///    *non-zero* leaf block, in ascending leaf-block-index order, with the
///    full `block_size` bytes of payload. Zero blocks are not passed in
///    (their BAT entry is already `PAYLOAD_BLOCK_ZERO`).
/// 3. [`Self::finish`] pads to [`Layout::bat_offset`] (no-op when the
///    payload run was non-empty), writes the encoded BAT padded to
///    [`Layout::bat_length`], and returns the inner writer for the caller
///    to flush/close.
pub struct ZstStreamWriter<W: Write> {
    out: W,
    cursor: u64,
    layout: Layout,
    bat: Bat,
    expected_payload_blocks: u64,
    written_payload_blocks: u64,
}

impl<W: Write> ZstStreamWriter<W> {
    pub fn begin(mut out: W, params: &WriterParams, bat: Bat, layout: Layout) -> Result<Self> {
        let mut cursor: u64 = 0;

        // 1. File type identifier at offset 0 (64 KiB; first 8 bytes = "vhdxfile").
        let mut fti = vec![0u8; FILE_TYPE_IDENTIFIER_SIZE as usize];
        fti[0..8].copy_from_slice(FILE_TYPE_SIGNATURE);
        write_block(&mut out, &mut cursor, &fti)?;
        debug_assert_eq!(cursor, FILE_TYPE_IDENTIFIER_SIZE);

        // 2. Header 1 + Header 2 (each 64 KiB; the encoded body is 4 KiB and
        //    the rest of the slot is zero).
        let h1 = Header {
            signature_ok: true,
            checksum_ok: true,
            checksum: 0,
            sequence_number: 1,
            file_write_guid: params.file_write_guid,
            data_write_guid: params.data_write_guid,
            log_guid: Uuid::nil(),
            log_version: 0,
            version: 1,
            log_length: params.log_size as u32,
            log_offset: layout.log_offset,
        };
        let h2 = Header {
            sequence_number: 2,
            ..h1
        };
        debug_assert_eq!(cursor, HEADER_1_OFFSET);
        write_block(&mut out, &mut cursor, &h1.encode())?;
        debug_assert_eq!(cursor, HEADER_2_OFFSET);
        write_block(&mut out, &mut cursor, &h2.encode())?;
        debug_assert_eq!(cursor, REGION_TABLE_1_OFFSET);

        // 3. Region tables 1 and 2 (each 64 KiB; reference Metadata and BAT
        //    at their precomputed offsets — including BAT-after-payload).
        let region = RegionTable {
            checksum_ok: true,
            entries: vec![
                RegionEntry {
                    guid: REGION_GUID_METADATA,
                    file_offset: layout.metadata_offset,
                    length: layout.metadata_length as u32,
                    required: true,
                },
                RegionEntry {
                    guid: REGION_GUID_BAT,
                    file_offset: layout.bat_offset,
                    length: layout.bat_length as u32,
                    required: true,
                },
            ],
        };
        let region_buf = region.encode();
        write_block(&mut out, &mut cursor, &region_buf)?;
        debug_assert_eq!(cursor, REGION_TABLE_2_OFFSET);
        write_block(&mut out, &mut cursor, &region_buf)?;
        debug_assert_eq!(cursor, REGION_TABLE_2_OFFSET + REGION_TABLE_SIZE);

        // 4. Reserved zero pad up to the log region at 1 MiB.
        pad_to(&mut out, &mut cursor, layout.log_offset)?;
        debug_assert_eq!(cursor, layout.log_offset);

        // 5. Empty log region (all zeros). VHDX accepts an absent/zero log so
        //    long as the active header's log_guid is nil — which we set above.
        write_zeros(&mut out, &mut cursor, layout.log_size)?;
        debug_assert_eq!(cursor, layout.metadata_offset);

        // 6. Metadata region (sized to layout.metadata_length).
        let metadata = Metadata {
            file_parameters: FileParameters {
                block_size: params.block_size,
                leave_blocks_allocated: params.fixed,
                has_parent: params.parent_locator.is_some(),
            },
            virtual_disk_size: params.virtual_disk_size,
            logical_sector_size: params.logical_sector_size,
            physical_sector_size: params.physical_sector_size,
            page_83_data: params.page_83_data,
            parent_locator: params.parent_locator.clone(),
            extras: Vec::new(),
        };
        let metadata_buf = metadata.encode(layout.metadata_length);
        debug_assert_eq!(metadata_buf.len() as u64, layout.metadata_length);
        write_block(&mut out, &mut cursor, &metadata_buf)?;
        debug_assert_eq!(cursor, layout.payload_start);

        Ok(ZstStreamWriter {
            out,
            cursor,
            layout,
            bat,
            expected_payload_blocks: layout.non_zero_blocks,
            written_payload_blocks: 0,
        })
    }

    /// Append one full payload block. Must be called exactly
    /// [`Layout::non_zero_blocks`] times, in ascending leaf-block-index
    /// order. The buffer must be exactly `block_size` bytes long.
    pub fn write_payload_block(&mut self, buf: &[u8]) -> Result<()> {
        debug_assert_eq!(buf.len() as u64, self.layout.block_size);
        if self.written_payload_blocks >= self.expected_payload_blocks {
            return Err(Error::InvalidStructure(format!(
                "ZstStreamWriter received {} payload blocks but layout was sized for {}",
                self.written_payload_blocks + 1,
                self.expected_payload_blocks
            )));
        }
        write_block(&mut self.out, &mut self.cursor, buf)?;
        self.written_payload_blocks += 1;
        Ok(())
    }

    /// Pad to the BAT region, write the encoded BAT (padded to
    /// [`Layout::bat_length`]) and return the inner writer for the caller
    /// to flush/finish.
    pub fn finish(mut self) -> Result<W> {
        if self.written_payload_blocks != self.expected_payload_blocks {
            return Err(Error::InvalidStructure(format!(
                "ZstStreamWriter expected {} payload blocks, got {}",
                self.expected_payload_blocks, self.written_payload_blocks
            )));
        }
        pad_to(&mut self.out, &mut self.cursor, self.layout.bat_offset)?;
        debug_assert_eq!(self.cursor, self.layout.bat_offset);

        let raw = self.bat.encode();
        let mut bat_buf = vec![0u8; self.layout.bat_length as usize];
        bat_buf[..raw.len()].copy_from_slice(&raw);
        write_block(&mut self.out, &mut self.cursor, &bat_buf)?;
        debug_assert_eq!(self.cursor, self.layout.total_size);

        Ok(self.out)
    }

    /// Bytes written so far (uncompressed).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

fn write_block<W: Write>(out: &mut W, cursor: &mut u64, buf: &[u8]) -> Result<()> {
    out.write_all(buf)?;
    *cursor += buf.len() as u64;
    Ok(())
}

fn write_zeros<W: Write>(out: &mut W, cursor: &mut u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let chunk = vec![0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        out.write_all(&chunk[..n])?;
        remaining -= n as u64;
    }
    *cursor += len;
    Ok(())
}

fn pad_to<W: Write>(out: &mut W, cursor: &mut u64, target: u64) -> Result<()> {
    debug_assert!(
        *cursor <= target,
        "pad_to is forward-only ({} > {})",
        cursor,
        target
    );
    if *cursor == target {
        return Ok(());
    }
    let n = target - *cursor;
    write_zeros(out, cursor, n)
}

/// Build a fully populated BAT for the streaming writer.
///
/// `is_zero[i] == true` marks block `i` as `PAYLOAD_BLOCK_ZERO` (no payload
/// slot is allocated); every other block gets a sequential
/// `PAYLOAD_BLOCK_FULLY_PRESENT` entry pointing into the payload region.
///
/// Returns the BAT plus the number of non-zero blocks (= the number of
/// `write_payload_block` calls the caller must make).
pub fn build_bat_for_layout<I: IntoIterator<Item = bool>>(
    block_count: u64,
    chunk_ratio: u64,
    has_parent: bool,
    payload_start: u64,
    block_size: u64,
    is_zero_iter: I,
) -> (Bat, u64) {
    let total_entries = total_bat_entries(block_count, chunk_ratio, has_parent);
    let mut bat = Bat {
        entries: vec![BatEntry::empty(); total_entries],
        chunk_ratio,
        block_count,
    };
    let mut seq: u64 = 0;
    for (i, is_zero) in is_zero_iter
        .into_iter()
        .enumerate()
        .take(block_count as usize)
    {
        let blk = i as u64;
        if is_zero {
            bat.set_payload_entry(blk, BatEntry::new(PAYLOAD_BLOCK_ZERO, 0));
        } else {
            let off = payload_start + seq * block_size;
            bat.set_payload_entry(blk, BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, off));
            seq += 1;
        }
    }
    (bat, seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_params() -> WriterParams {
        WriterParams {
            virtual_disk_size: 64 * MIB,
            block_size: 32 * 1024 * 1024,
            logical_sector_size: 512,
            physical_sector_size: 4096,
            fixed: false,
            parent_locator: None,
            log_size: MIB,
            data_write_guid: Uuid::new_v4(),
            file_write_guid: Uuid::new_v4(),
            page_83_data: Uuid::new_v4(),
        }
    }

    #[test]
    fn layout_offsets_are_mib_aligned() {
        let p = dummy_params();
        let bat_entries = total_bat_entries(2, 2048, false);
        let l = Layout::compute(&p, 1, bat_entries);
        assert_eq!(l.log_offset % MIB, 0);
        assert_eq!(l.metadata_offset % MIB, 0);
        assert_eq!(l.payload_start % MIB, 0);
        assert_eq!(l.bat_offset % MIB, 0);
        assert_eq!(l.bat_length % MIB, 0);
        assert_eq!(l.total_size, l.bat_offset + l.bat_length);
    }

    #[test]
    fn build_bat_for_layout_assigns_sequential_offsets() {
        let bs: u64 = 32 * 1024 * 1024;
        let payload_start: u64 = 3 * MIB;
        let (bat, non_zero) = build_bat_for_layout(
            4,
            2048,
            false,
            payload_start,
            bs,
            vec![false, true, false, true],
        );
        assert_eq!(non_zero, 2);
        // block 0: non-zero -> payload_start + 0*bs
        let e0 = bat.payload_entry(0).unwrap();
        assert_eq!(e0.state(), PAYLOAD_BLOCK_FULLY_PRESENT);
        assert_eq!(e0.file_offset(), payload_start);
        // block 1: zero
        let e1 = bat.payload_entry(1).unwrap();
        assert_eq!(e1.state(), PAYLOAD_BLOCK_ZERO);
        assert_eq!(e1.file_offset(), 0);
        // block 2: non-zero -> payload_start + 1*bs
        let e2 = bat.payload_entry(2).unwrap();
        assert_eq!(e2.state(), PAYLOAD_BLOCK_FULLY_PRESENT);
        assert_eq!(e2.file_offset(), payload_start + bs);
        // block 3: zero
        let e3 = bat.payload_entry(3).unwrap();
        assert_eq!(e3.state(), PAYLOAD_BLOCK_ZERO);
    }

    #[test]
    fn stream_writer_round_trip_in_memory_zero_blocks() {
        let p = dummy_params();
        let block_count = p.virtual_disk_size / p.block_size as u64;
        let chunk_ratio = ((1u64 << 23) * p.logical_sector_size as u64) / p.block_size as u64;
        let total_entries = total_bat_entries(block_count, chunk_ratio, false);
        // Mark every block as zero so we don't have to feed payload bytes.
        let (bat, non_zero) = build_bat_for_layout(
            block_count,
            chunk_ratio,
            false,
            Layout::compute(&p, 0, total_entries).payload_start,
            p.block_size as u64,
            std::iter::repeat(true).take(block_count as usize),
        );
        assert_eq!(non_zero, 0);
        let layout = Layout::compute(&p, non_zero, total_entries);

        let buf: Vec<u8> = Vec::new();
        let writer = ZstStreamWriter::begin(buf, &p, bat, layout).unwrap();
        let inner = writer.finish().unwrap();
        assert_eq!(inner.len() as u64, layout.total_size);
        // First 8 bytes must be the file type signature.
        assert_eq!(&inner[0..8], FILE_TYPE_SIGNATURE);
    }
}
