//! Pure-Rust VHDX *writer*. Produces dynamic VHDX files from scratch (the
//! optional `fixed` subformat is also supported via `WriterParams::fixed`).
//!
//! Layout produced:
//!     0 ..    64 KiB  : file type identifier (`vhdxfile`)
//!    64 ..   192 KiB  : Header 1 / Header 2
//!   192 ..   320 KiB  : Region table 1 / Region table 2
//!   320 KiB ..  1 MiB : reserved (zero)
//!     1 MiB           : log region (default 1 MiB; empty)
//!  +log_len           : metadata region (1 MiB)
//!  +1 MiB             : BAT region (1 MiB aligned, sized to required entries)
//!  + ...              : payload blocks (1 MiB aligned), one per data block.

use std::fs::File;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::bat::{Bat, BatEntry};
use crate::error::Result;
use crate::format::{
    FILE_TYPE_IDENTIFIER_OFFSET, FILE_TYPE_SIGNATURE, MIB, PAYLOAD_BLOCK_FULLY_PRESENT,
    PAYLOAD_BLOCK_NOT_PRESENT, PAYLOAD_BLOCK_ZERO, REGION_GUID_BAT, REGION_GUID_METADATA,
};
use crate::header::Header;
use crate::io_util::{align_up, write_all_at};
use crate::metadata::{FileParameters, Metadata};
use crate::parent_locator::ParentLocator;
use crate::region::{RegionEntry, RegionTable};

#[derive(Debug, Clone)]
pub struct WriterParams {
    pub virtual_disk_size: u64,
    pub block_size: u32,
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    pub fixed: bool,
    pub parent_locator: Option<ParentLocator>,
    pub log_size: u64,
    pub data_write_guid: Uuid,
    pub file_write_guid: Uuid,
    pub page_83_data: Uuid,
}

impl WriterParams {
    pub fn dynamic(virtual_disk_size: u64) -> Self {
        WriterParams {
            virtual_disk_size,
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
}

/// A block scheduled to be written to the output file.
pub enum BlockSource<'a> {
    Zero,
    Buf(&'a [u8]),
}

pub struct VhdxWriter {
    pub params: WriterParams,
    pub path: PathBuf,
    file: File,
    bat: Bat,
    log_offset: u64,
    metadata_offset: u64,
    bat_offset: u64,
    bat_length: u64,
    payload_cursor: u64,
    /// When > 0, payload block writes are streamed through `WriteFile` /
    /// `pwrite` in chunks of this many bytes, with `sync_data` called
    /// between each chunk. Bounds the per-write transient cluster
    /// reservation on filesystems that pre-allocate uncompressed-equivalent
    /// space — notably NTFS with `FILE_ATTRIBUTE_COMPRESSED`, where a
    /// 32 MiB `WriteFile` would otherwise force the volume to have 32 MiB
    /// of physically free clusters even though the eventual compressed
    /// footprint is much smaller. Set via [`Self::set_chunked_write_size`].
    chunked_write_size: u32,
}

impl VhdxWriter {
    /// Create a fresh, empty VHDX file at `path`. The BAT is initialized with
    /// all-zero entries (`PAYLOAD_BLOCK_NOT_PRESENT`).
    pub fn create<P: AsRef<Path>>(path: P, params: WriterParams) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;

        let block_size = params.block_size as u64;
        let block_count = params.virtual_disk_size.div_ceil(block_size);
        let chunk_ratio = ((1u64 << 23) * params.logical_sector_size as u64) / block_size;
        let has_parent = params.parent_locator.is_some();
        let total_entries = crate::bat::total_bat_entries(block_count, chunk_ratio, has_parent);
        let bat_bytes_needed = (total_entries as u64) * 8;
        let bat_length = align_up(bat_bytes_needed.max(MIB), MIB);

        let log_offset = MIB;
        let metadata_offset = log_offset + align_up(params.log_size, MIB);
        let bat_offset = metadata_offset + MIB;
        let payload_start = bat_offset + bat_length;

        // Write file type identifier.
        let mut fti = vec![0u8; 64 * 1024];
        fti[0..8].copy_from_slice(FILE_TYPE_SIGNATURE);
        write_all_at(&mut file, FILE_TYPE_IDENTIFIER_OFFSET, &fti)?;

        // Reserve the header section (1 MiB) by writing a zero pad.
        crate::io_util::zero_range(&mut file, 64 * 1024, MIB - 64 * 1024)?;

        // Write the empty log (zeroed).
        crate::io_util::zero_range(&mut file, log_offset, params.log_size)?;

        // Write metadata.
        let metadata = Metadata {
            file_parameters: FileParameters {
                block_size: params.block_size,
                leave_blocks_allocated: params.fixed,
                has_parent,
            },
            virtual_disk_size: params.virtual_disk_size,
            logical_sector_size: params.logical_sector_size,
            physical_sector_size: params.physical_sector_size,
            page_83_data: params.page_83_data,
            parent_locator: params.parent_locator.clone(),
            extras: Vec::new(),
        };
        let metadata_buf = metadata.encode(MIB);
        write_all_at(&mut file, metadata_offset, &metadata_buf)?;

        // Initialize BAT with all-zero entries.
        let mut bat = Bat {
            entries: vec![BatEntry::empty(); total_entries],
            chunk_ratio,
            block_count,
        };
        // Mark every entry as NOT_PRESENT (state 0, offset 0) — this is the
        // already-zeroed default.
        let mut bat_buf = vec![0u8; bat_length as usize];
        let raw = bat.encode();
        bat_buf[..raw.len()].copy_from_slice(&raw);
        write_all_at(&mut file, bat_offset, &bat_buf)?;

        // Write region table.
        let region = RegionTable {
            checksum_ok: true,
            entries: vec![
                RegionEntry {
                    guid: REGION_GUID_METADATA,
                    file_offset: metadata_offset,
                    length: MIB as u32,
                    required: true,
                },
                RegionEntry {
                    guid: REGION_GUID_BAT,
                    file_offset: bat_offset,
                    length: bat_length as u32,
                    required: true,
                },
            ],
        };
        let region_buf = region.encode();
        write_all_at(
            &mut file,
            crate::format::region_table_offset(0),
            &region_buf,
        )?;
        write_all_at(
            &mut file,
            crate::format::region_table_offset(1),
            &region_buf,
        )?;

        // Write headers (sequence numbers 1 and 2; the higher one wins).
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
            log_offset,
        };
        let h2 = Header {
            sequence_number: 2,
            ..h1
        };
        write_all_at(&mut file, crate::format::header_offset(0), &h1.encode())?;
        write_all_at(&mut file, crate::format::header_offset(1), &h2.encode())?;

        // For fixed images: pre-allocate every payload block.
        if params.fixed {
            let mut cur = payload_start;
            for i in 0..block_count {
                bat.set_payload_entry(i, BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, cur));
                cur += block_size;
            }
            // Re-flush BAT.
            let mut bat_buf = vec![0u8; bat_length as usize];
            let raw = bat.encode();
            bat_buf[..raw.len()].copy_from_slice(&raw);
            write_all_at(&mut file, bat_offset, &bat_buf)?;
            file.set_len(cur)?;
        } else {
            file.set_len(payload_start)?;
        }
        file.flush()?;

        Ok(VhdxWriter {
            params,
            path,
            file,
            bat,
            log_offset,
            metadata_offset,
            bat_offset,
            bat_length,
            payload_cursor: payload_start,
            chunked_write_size: 0,
        })
    }

    /// Configure payload block writes to be issued in `chunk_size`-byte
    /// chunks with a `sync_data` call between each. Set to `0` to restore
    /// the default behaviour (one `WriteFile` per block).
    ///
    /// Useful when the destination is an NTFS-compressed file: by writing
    /// in small chunks and waiting for the filesystem to compress and
    /// commit each one, the volume's transient free-space reservation never
    /// exceeds `chunk_size` bytes, even though each VHDX payload block can
    /// be up to 256 MiB. A typical good value is 1 MiB.
    pub fn set_chunked_write_size(&mut self, chunk_size: u32) {
        self.chunked_write_size = chunk_size;
    }

    /// Append a payload block at the end of the file. `block_index` is the
    /// virtual block index (0-based). The BAT entry is updated in memory; call
    /// [`Self::finish`] to flush BAT and headers.
    pub fn write_block(&mut self, block_index: u64, source: BlockSource<'_>) -> Result<()> {
        let bs = self.params.block_size as u64;
        match source {
            BlockSource::Zero => {
                self.bat
                    .set_payload_entry(block_index, BatEntry::new(PAYLOAD_BLOCK_ZERO, 0));
            }
            BlockSource::Buf(buf) => {
                debug_assert_eq!(buf.len() as u64, bs);
                let off = if self.params.fixed {
                    // The block is already allocated in `create`; just write
                    // to the slot recorded in the BAT.
                    self.bat.payload_entry(block_index).unwrap().file_offset()
                } else {
                    self.payload_cursor
                };
                self.write_payload_buf(off, buf)?;
                if !self.params.fixed {
                    self.bat.set_payload_entry(
                        block_index,
                        BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, off),
                    );
                    self.payload_cursor += bs;
                }
            }
        }
        Ok(())
    }

    /// Write a payload buffer at `offset`, honouring the
    /// [`chunked_write_size`](Self::set_chunked_write_size) setting.
    ///
    /// In chunked mode every sub-chunk is followed by a `sync_data` call so
    /// that the filesystem (notably NTFS with compression) commits the just
    /// written bytes before the next chunk is queued.
    fn write_payload_buf(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let chunk_size = self.chunked_write_size as usize;
        if chunk_size == 0 || buf.len() <= chunk_size {
            write_all_at(&mut self.file, offset, buf)?;
            return Ok(());
        }
        let mut cursor = offset;
        let mut i = 0;
        while i < buf.len() {
            let end = (i + chunk_size).min(buf.len());
            write_all_at(&mut self.file, cursor, &buf[i..end])?;
            // Force the filesystem to compress and commit this chunk before
            // we issue the next WriteFile, so the OS write cache cannot grow
            // past one chunk's worth of uncompressed data and the volume's
            // transient cluster reservation never exceeds `chunk_size`.
            self.file.sync_data()?;
            cursor += (end - i) as u64;
            i = end;
        }
        Ok(())
    }

    /// Flush every byte already written through the writer to the underlying
    /// storage (`fdatasync` on Unix, `FlushFileBuffers` on Windows).
    ///
    /// On Windows, when the destination file carries `FILE_ATTRIBUTE_COMPRESSED`
    /// (NTFS LZNT1), this call blocks until the filesystem has finished
    /// compressing every dirty cluster. Calling it between block writes during
    /// a flatten therefore caps the size of the OS write cache and prevents
    /// the compressor from falling arbitrarily far behind on a large run.
    pub fn sync_data(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Close the underlying file handle and re-open it.
    ///
    /// On Windows, dropping a `File` invokes `CloseHandle`, which is the
    /// trigger NTFS uses to run its "on close" maintenance pass on a
    /// compressed file: it finalises the compressed-extent metadata and
    /// updates the volume's allocation accounting so subsequent writes are
    /// reserved against the file's real (compressed) on-disk footprint
    /// instead of its uncompressed-equivalent peak. Without this, long
    /// runs of extending writes through a single handle can keep the
    /// volume's free-space accounting on the pessimistic (uncompressed)
    /// estimate even after every chunk has been `FlushFileBuffers`'d, and
    /// the next `WriteFile` may fail with `ENOSPC` while the file's actual
    /// size on disk leaves plenty of headroom.
    ///
    /// **Caller contract:** every byte that must survive the reopen has
    /// to be `sync_data`'d first. `CloseHandle` is *not* documented to
    /// imply `FlushFileBuffers` and we do not assume otherwise.
    ///
    /// On Unix this is just an `fclose` + `fopen` round-trip on the same
    /// inode; correctness is preserved but no extra benefit is gained.
    pub fn reopen(&mut self) -> Result<()> {
        // Open the new handle first so `self.file` always holds a valid
        // `File`; the old handle is then dropped, which is what
        // synchronously runs `CloseHandle` on Windows.
        let new = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        let _old = std::mem::replace(&mut self.file, new);
        Ok(())
    }

    /// Mark a block as `PAYLOAD_BLOCK_NOT_PRESENT` so that reads fall through
    /// to the parent. Only meaningful for differencing files.
    pub fn mark_not_present(&mut self, block_index: u64) -> Result<()> {
        self.bat
            .set_payload_entry(block_index, BatEntry::new(PAYLOAD_BLOCK_NOT_PRESENT, 0));
        Ok(())
    }

    /// Flush BAT, fsync, and close.
    pub fn finish(mut self) -> Result<PathBuf> {
        let mut bat_buf = vec![0u8; self.bat_length as usize];
        let raw = self.bat.encode();
        bat_buf[..raw.len()].copy_from_slice(&raw);
        write_all_at(&mut self.file, self.bat_offset, &bat_buf)?;
        // Truncate to the last allocated payload offset (already aligned).
        if !self.params.fixed {
            self.file.set_len(self.payload_cursor)?;
        }
        self.file.flush()?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.sync_all()?;
        let _ = self.metadata_offset;
        let _ = self.log_offset;
        Ok(self.path)
    }
}
