//! Read-side `VhdxDisk`: opens a single VHDX file, parses headers, region
//! tables, BAT and metadata, and exposes a virtual-block read API.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::bat::{Bat, PayloadState, SectorBitmapState};
use crate::error::{Error, Result};
use crate::header::{verify_file_type, Header, HeaderPair};
use crate::io_util::{read_at, read_exact_at, write_all_at};
use crate::metadata::Metadata;
use crate::region::{RegionEntry, RegionTable, RegionTablePair};

#[derive(Debug, Clone)]
pub struct VhdxDisk {
    pub path: PathBuf,
    pub read_only: bool,
    pub header_pair: HeaderPair,
    pub region_pair: RegionTablePair,
    pub region: RegionTable,
    pub bat_region: RegionEntry,
    pub metadata_region: RegionEntry,
    pub metadata: Metadata,
    pub bat: Bat,
    pub file_len: u64,
}

impl VhdxDisk {
    pub fn open<P: AsRef<Path>>(path: P, read_only: bool) -> Result<(Self, File)> {
        let path = path.as_ref().to_path_buf();
        let mut opts = OpenOptions::new();
        opts.read(true);
        if !read_only {
            opts.write(true);
        }
        let mut file = opts.open(&path)?;
        verify_file_type(&mut file)?;

        let header_pair = HeaderPair::read(&mut file)?;
        let region_pair = RegionTablePair::read(&mut file)?;
        let region = region_pair.active().clone();
        region.validate_layout()?;

        let bat_region = *region.bat().ok_or(Error::MissingRegion("BAT"))?;
        let metadata_region = *region.metadata().ok_or(Error::MissingRegion("Metadata"))?;

        let mut metadata_buf = vec![0u8; metadata_region.length as usize];
        read_exact_at(&mut file, metadata_region.file_offset, &mut metadata_buf)?;
        let metadata = Metadata::parse(&metadata_buf)?;

        let mut bat_buf = vec![0u8; bat_region.length as usize];
        read_exact_at(&mut file, bat_region.file_offset, &mut bat_buf)?;
        let bat = Bat::parse(
            &bat_buf,
            metadata.block_count(),
            metadata.chunk_ratio(),
            metadata.file_parameters.has_parent,
        );

        let file_len = file.metadata()?.len();

        // Reject opening a dirty file in write mode without an explicit fsck;
        // however we *do* allow read-only opens of dirty files for `info`.
        let active = header_pair.active();
        if !read_only && active.log_guid != uuid::Uuid::nil() {
            return Err(Error::DirtyLog);
        }

        Ok((
            VhdxDisk {
                path,
                read_only,
                header_pair,
                region_pair,
                region,
                bat_region,
                metadata_region,
                metadata,
                bat,
                file_len,
            },
            file,
        ))
    }

    pub fn block_size(&self) -> u32 {
        self.metadata.block_size()
    }

    pub fn virtual_disk_size(&self) -> u64 {
        self.metadata.virtual_disk_size
    }

    pub fn block_count(&self) -> u64 {
        self.metadata.block_count()
    }

    pub fn has_parent(&self) -> bool {
        self.metadata.file_parameters.has_parent
    }

    pub fn active_header(&self) -> &Header {
        self.header_pair.active()
    }

    /// Read a payload block. Returns `None` if the block is `NotPresent`,
    /// `Unmapped` or otherwise should be served by a parent. For `Zero`
    /// blocks it returns a freshly zeroed buffer.
    pub fn read_block(&self, file: &mut File, block_index: u64) -> Result<BlockRead> {
        if block_index >= self.block_count() {
            return Err(Error::VirtOffsetOutOfRange(
                block_index * self.block_size() as u64,
            ));
        }
        let entry = self.bat.payload_entry(block_index).ok_or_else(|| {
            Error::InvalidStructure(format!("BAT entry missing for block {block_index}"))
        })?;
        let state = PayloadState::from_raw(entry.state());
        match state {
            PayloadState::FullyPresent => {
                let mut buf = vec![0u8; self.block_size() as usize];
                read_exact_at(file, entry.file_offset(), &mut buf)?;
                Ok(BlockRead::Full(buf))
            }
            PayloadState::Zero => Ok(BlockRead::Full(vec![0u8; self.block_size() as usize])),
            PayloadState::PartiallyPresent => {
                let sb_entry = self
                    .bat
                    .sector_bitmap_entry(block_index)
                    .ok_or_else(|| Error::InvalidStructure("missing sector bitmap".into()))?;
                if SectorBitmapState::from_raw(sb_entry.state()) != SectorBitmapState::Present {
                    // Spec: if PartiallyPresent then SB must be Present.
                    return Err(Error::InvalidStructure(
                        "PartiallyPresent block without Present sector bitmap".into(),
                    ));
                }
                let mut payload = vec![0u8; self.block_size() as usize];
                read_exact_at(file, entry.file_offset(), &mut payload)?;

                let bm = self.read_sector_bitmap(file, block_index)?;
                Ok(BlockRead::Partial {
                    payload,
                    bitmap: bm,
                })
            }
            PayloadState::NotPresent | PayloadState::Unmapped | PayloadState::Undefined => {
                Ok(BlockRead::Absent)
            }
            PayloadState::Other(s) => Err(Error::InvalidStructure(format!(
                "unknown payload state {s} for block {block_index}"
            ))),
        }
    }

    /// Read the 1 MiB sector bitmap chunk that owns `block_index` and return
    /// only the slice that corresponds to this block's sectors.
    pub fn read_sector_bitmap(&self, file: &mut File, block_index: u64) -> Result<Vec<u8>> {
        let chunk_ratio = self.metadata.chunk_ratio();
        let chunk_index = block_index / chunk_ratio;
        let block_in_chunk = (block_index % chunk_ratio) as usize;
        let sb_entry = self
            .bat
            .sector_bitmap_entry(block_index)
            .ok_or_else(|| Error::InvalidStructure("no sector bitmap entry".into()))?;
        let sb_offset = sb_entry.file_offset();
        // The sector bitmap chunk is exactly 1 MiB and contains 1 bit per
        // logical sector covering `chunk_ratio` payload blocks.
        let sectors_per_block =
            (self.block_size() as u64) / (self.metadata.logical_sector_size as u64);
        let bits_per_block = sectors_per_block as usize;
        let bytes_per_block = bits_per_block.div_ceil(8);
        let mut buf = vec![0u8; bytes_per_block];

        let bit_offset_in_chunk = block_in_chunk * bits_per_block;
        let byte_offset_in_chunk = bit_offset_in_chunk / 8;
        // For 4 KiB sector + 1 MiB block (default), bits are byte-aligned.
        if bit_offset_in_chunk % 8 != 0 {
            return Err(Error::Unsupported(
                "sub-byte sector bitmap alignment is not yet supported".into(),
            ));
        }
        read_at(file, sb_offset + byte_offset_in_chunk as u64, &mut buf)?;
        let _ = chunk_index;
        Ok(buf)
    }

    /// Re-read both region tables and overwrite them from `self.region`.
    pub fn rewrite_region_tables(&self, file: &mut File) -> Result<()> {
        let raw = self.region.encode();
        write_all_at(file, crate::format::region_table_offset(0), &raw)?;
        write_all_at(file, crate::format::region_table_offset(1), &raw)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum BlockRead {
    Full(Vec<u8>),
    Partial { payload: Vec<u8>, bitmap: Vec<u8> },
    Absent,
}

/// Test whether bit `i` is set in the LSB-first packed bitmap `bm`.
pub fn bit_is_set(bm: &[u8], i: usize) -> bool {
    let byte = i / 8;
    let bit = i % 8;
    if byte >= bm.len() {
        return false;
    }
    (bm[byte] >> bit) & 1 == 1
}
