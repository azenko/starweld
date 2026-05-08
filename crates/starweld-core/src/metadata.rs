//! Metadata region: parse and re-emit the well-known metadata items used by
//! VHDX/AVHDX (file parameters, virtual disk size, sector sizes, page-83 data
//! and parent locator). Other items are preserved as opaque blobs so we can
//! round-trip a file without losing data.

use byteorder::{ByteOrder, LittleEndian};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::format::{
    FILE_PARAMETERS_FLAG_HAS_PARENT, FILE_PARAMETERS_FLAG_LEAVE_BLOCKS_ALLOCATED,
    METADATA_ENTRY_SIZE, METADATA_FLAG_IS_REQUIRED, METADATA_FLAG_IS_VIRTUAL_DISK,
    METADATA_GUID_FILE_PARAMETERS, METADATA_GUID_LOGICAL_SECTOR_SIZE, METADATA_GUID_PAGE_83_DATA,
    METADATA_GUID_PARENT_LOCATOR, METADATA_GUID_PHYSICAL_SECTOR_SIZE,
    METADATA_GUID_VIRTUAL_DISK_SIZE, METADATA_HEADER_SIZE, METADATA_SIGNATURE,
};
use crate::guid::{read_guid_at, write_guid_at};
use crate::parent_locator::ParentLocator;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileParameters {
    pub block_size: u32,
    pub leave_blocks_allocated: bool,
    pub has_parent: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Metadata {
    pub file_parameters: FileParameters,
    pub virtual_disk_size: u64,
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    pub page_83_data: Uuid,
    pub parent_locator: Option<ParentLocator>,
    /// Opaque preservation of any other metadata items.
    pub extras: Vec<MetadataItem>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MetadataItem {
    pub guid: Uuid,
    pub data: Vec<u8>,
    pub is_user: bool,
    pub is_virtual_disk: bool,
    pub is_required: bool,
}

impl Metadata {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < METADATA_HEADER_SIZE {
            return Err(Error::InvalidStructure("metadata region too small".into()));
        }
        if &buf[0..8] != METADATA_SIGNATURE {
            return Err(Error::InvalidStructure("missing metadata signature".into()));
        }
        let entry_count = LittleEndian::read_u16(&buf[10..12]) as usize;

        let mut file_parameters: Option<FileParameters> = None;
        let mut virtual_disk_size: Option<u64> = None;
        let mut logical_sector_size: Option<u32> = None;
        let mut physical_sector_size: Option<u32> = None;
        let mut page_83_data: Option<Uuid> = None;
        let mut parent_locator: Option<ParentLocator> = None;
        let mut extras = Vec::new();

        for i in 0..entry_count {
            let off = METADATA_HEADER_SIZE + i * METADATA_ENTRY_SIZE;
            if off + METADATA_ENTRY_SIZE > buf.len() {
                break;
            }
            let guid = read_guid_at(buf, off);
            let item_off = LittleEndian::read_u32(&buf[off + 16..off + 20]) as usize;
            let length = LittleEndian::read_u32(&buf[off + 20..off + 24]) as usize;
            let flags = LittleEndian::read_u32(&buf[off + 24..off + 28]);
            if item_off + length > buf.len() {
                continue;
            }
            let data = buf[item_off..item_off + length].to_vec();

            if guid == METADATA_GUID_FILE_PARAMETERS && data.len() >= 8 {
                let bs = LittleEndian::read_u32(&data[0..4]);
                let f = LittleEndian::read_u32(&data[4..8]);
                file_parameters = Some(FileParameters {
                    block_size: bs,
                    leave_blocks_allocated: f & FILE_PARAMETERS_FLAG_LEAVE_BLOCKS_ALLOCATED != 0,
                    has_parent: f & FILE_PARAMETERS_FLAG_HAS_PARENT != 0,
                });
            } else if guid == METADATA_GUID_VIRTUAL_DISK_SIZE && data.len() >= 8 {
                virtual_disk_size = Some(LittleEndian::read_u64(&data[0..8]));
            } else if guid == METADATA_GUID_LOGICAL_SECTOR_SIZE && data.len() >= 4 {
                logical_sector_size = Some(LittleEndian::read_u32(&data[0..4]));
            } else if guid == METADATA_GUID_PHYSICAL_SECTOR_SIZE && data.len() >= 4 {
                physical_sector_size = Some(LittleEndian::read_u32(&data[0..4]));
            } else if guid == METADATA_GUID_PAGE_83_DATA && data.len() >= 16 {
                page_83_data = Some(read_guid_at(&data, 0));
            } else if guid == METADATA_GUID_PARENT_LOCATOR {
                parent_locator = Some(ParentLocator::parse(&data)?);
            } else {
                extras.push(MetadataItem {
                    guid,
                    data,
                    is_user: flags & crate::format::METADATA_FLAG_IS_USER != 0,
                    is_virtual_disk: flags & METADATA_FLAG_IS_VIRTUAL_DISK != 0,
                    is_required: flags & METADATA_FLAG_IS_REQUIRED != 0,
                });
            }
        }

        Ok(Metadata {
            file_parameters: file_parameters.ok_or(Error::MissingMetadata("file_parameters"))?,
            virtual_disk_size: virtual_disk_size
                .ok_or(Error::MissingMetadata("virtual_disk_size"))?,
            logical_sector_size: logical_sector_size
                .ok_or(Error::MissingMetadata("logical_sector_size"))?,
            physical_sector_size: physical_sector_size
                .ok_or(Error::MissingMetadata("physical_sector_size"))?,
            page_83_data: page_83_data.ok_or(Error::MissingMetadata("page_83_data"))?,
            parent_locator,
            extras,
        })
    }

    /// Number of bytes per payload block on disk.
    pub fn block_size(&self) -> u32 {
        self.file_parameters.block_size
    }

    /// Total number of payload blocks needed to cover the virtual disk size.
    pub fn block_count(&self) -> u64 {
        let bs = self.block_size() as u64;
        self.virtual_disk_size.div_ceil(bs)
    }

    /// chunk_ratio = (2^23 * logical_sector_size) / block_size
    pub fn chunk_ratio(&self) -> u64 {
        ((1u64 << 23) * self.logical_sector_size as u64) / self.block_size() as u64
    }

    pub fn encode(&self, region_size: u64) -> Vec<u8> {
        let mut buf = vec![0u8; region_size as usize];
        buf[0..8].copy_from_slice(METADATA_SIGNATURE);
        let mut entries: Vec<(Uuid, Vec<u8>, u32)> = Vec::new();

        // file parameters
        let mut fp = vec![0u8; 8];
        LittleEndian::write_u32(&mut fp[0..4], self.file_parameters.block_size);
        let mut flags = 0u32;
        if self.file_parameters.leave_blocks_allocated {
            flags |= FILE_PARAMETERS_FLAG_LEAVE_BLOCKS_ALLOCATED;
        }
        if self.file_parameters.has_parent {
            flags |= FILE_PARAMETERS_FLAG_HAS_PARENT;
        }
        LittleEndian::write_u32(&mut fp[4..8], flags);
        entries.push((METADATA_GUID_FILE_PARAMETERS, fp, METADATA_FLAG_IS_REQUIRED));

        // virtual disk size
        let mut vds = vec![0u8; 8];
        LittleEndian::write_u64(&mut vds[..], self.virtual_disk_size);
        entries.push((
            METADATA_GUID_VIRTUAL_DISK_SIZE,
            vds,
            METADATA_FLAG_IS_VIRTUAL_DISK | METADATA_FLAG_IS_REQUIRED,
        ));

        // page 83 data
        let mut p83 = vec![0u8; 16];
        write_guid_at(&mut p83, 0, self.page_83_data);
        entries.push((
            METADATA_GUID_PAGE_83_DATA,
            p83,
            METADATA_FLAG_IS_VIRTUAL_DISK | METADATA_FLAG_IS_REQUIRED,
        ));

        // logical sector size
        let mut ls = vec![0u8; 4];
        LittleEndian::write_u32(&mut ls[..], self.logical_sector_size);
        entries.push((
            METADATA_GUID_LOGICAL_SECTOR_SIZE,
            ls,
            METADATA_FLAG_IS_VIRTUAL_DISK | METADATA_FLAG_IS_REQUIRED,
        ));

        // physical sector size
        let mut ps = vec![0u8; 4];
        LittleEndian::write_u32(&mut ps[..], self.physical_sector_size);
        entries.push((
            METADATA_GUID_PHYSICAL_SECTOR_SIZE,
            ps,
            METADATA_FLAG_IS_VIRTUAL_DISK | METADATA_FLAG_IS_REQUIRED,
        ));

        if let Some(pl) = &self.parent_locator {
            entries.push((
                METADATA_GUID_PARENT_LOCATOR,
                pl.encode(),
                METADATA_FLAG_IS_REQUIRED,
            ));
        }

        for extra in &self.extras {
            let mut f = 0u32;
            if extra.is_user {
                f |= crate::format::METADATA_FLAG_IS_USER;
            }
            if extra.is_virtual_disk {
                f |= METADATA_FLAG_IS_VIRTUAL_DISK;
            }
            if extra.is_required {
                f |= METADATA_FLAG_IS_REQUIRED;
            }
            entries.push((extra.guid, extra.data.clone(), f));
        }

        LittleEndian::write_u16(&mut buf[10..12], entries.len() as u16);

        // Place item data starting at 64 KiB (well past the table). Aligned to
        // 8 bytes to satisfy spec.
        let mut data_off: usize = 64 * 1024;
        for (i, (guid, data, flags)) in entries.iter().enumerate() {
            let entry_off = METADATA_HEADER_SIZE + i * METADATA_ENTRY_SIZE;
            write_guid_at(&mut buf, entry_off, *guid);
            LittleEndian::write_u32(&mut buf[entry_off + 16..entry_off + 20], data_off as u32);
            LittleEndian::write_u32(&mut buf[entry_off + 20..entry_off + 24], data.len() as u32);
            LittleEndian::write_u32(&mut buf[entry_off + 24..entry_off + 28], *flags);
            buf[data_off..data_off + data.len()].copy_from_slice(data);
            // 8-byte align next item
            data_off += data.len();
            data_off = (data_off + 7) & !7usize;
        }
        buf
    }
}
