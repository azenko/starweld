//! VHDX region table parse + arbitration.

use byteorder::{ByteOrder, LittleEndian};
use std::fs::File;
use uuid::Uuid;

use crate::crc32c::{crc32c, crc32c_with_zeroed_field};
use crate::error::{Error, Result};
use crate::format::{
    region_table_offset, REGION_FLAG_REQUIRED, REGION_GUID_BAT, REGION_GUID_METADATA,
    REGION_SIGNATURE, REGION_TABLE_ENTRY_SIZE, REGION_TABLE_HEADER_SIZE, REGION_TABLE_MAX_ENTRIES,
    REGION_TABLE_SIZE,
};
use crate::guid::{read_guid_at, write_guid_at};
use crate::io_util::{read_exact_at, write_all_at};

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RegionEntry {
    pub guid: Uuid,
    pub file_offset: u64,
    pub length: u32,
    pub required: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RegionTable {
    pub checksum_ok: bool,
    pub entries: Vec<RegionEntry>,
}

impl RegionTable {
    pub fn parse(buf: &[u8]) -> RegionTable {
        let mut rt = RegionTable {
            checksum_ok: false,
            entries: Vec::new(),
        };
        if buf.len() < REGION_TABLE_HEADER_SIZE {
            return rt;
        }
        if &buf[0..4] != REGION_SIGNATURE {
            return rt;
        }
        let checksum = LittleEndian::read_u32(&buf[4..8]);
        let entry_count = LittleEndian::read_u32(&buf[8..12]) as usize;
        if entry_count > REGION_TABLE_MAX_ENTRIES {
            return rt;
        }
        let need = REGION_TABLE_HEADER_SIZE + entry_count * REGION_TABLE_ENTRY_SIZE;
        if buf.len() < need {
            return rt;
        }
        let calc = crc32c_with_zeroed_field(buf, 4);
        rt.checksum_ok = calc == checksum;

        for i in 0..entry_count {
            let off = REGION_TABLE_HEADER_SIZE + i * REGION_TABLE_ENTRY_SIZE;
            let guid = read_guid_at(buf, off);
            let file_offset = LittleEndian::read_u64(&buf[off + 16..off + 24]);
            let length = LittleEndian::read_u32(&buf[off + 24..off + 28]);
            let flags = LittleEndian::read_u32(&buf[off + 28..off + 32]);
            rt.entries.push(RegionEntry {
                guid,
                file_offset,
                length,
                required: flags & REGION_FLAG_REQUIRED != 0,
            });
        }
        rt
    }

    pub fn find(&self, guid: Uuid) -> Option<&RegionEntry> {
        self.entries.iter().find(|e| e.guid == guid)
    }

    pub fn bat(&self) -> Option<&RegionEntry> {
        self.find(REGION_GUID_BAT)
    }

    pub fn metadata(&self) -> Option<&RegionEntry> {
        self.find(REGION_GUID_METADATA)
    }

    pub fn validate_layout(&self) -> Result<()> {
        let mut intervals: Vec<(u64, u64)> = self
            .entries
            .iter()
            .map(|e| (e.file_offset, e.file_offset + e.length as u64))
            .collect();
        intervals.sort();
        for w in intervals.windows(2) {
            if w[0].1 > w[1].0 {
                return Err(Error::InvalidStructure(format!(
                    "regions overlap: {:#x}..{:#x} and {:#x}..{:#x}",
                    w[0].0, w[0].1, w[1].0, w[1].1
                )));
            }
        }
        for e in &self.entries {
            if e.file_offset % (1024 * 1024) != 0 {
                return Err(Error::InvalidStructure(format!(
                    "region offset {:#x} not 1 MiB aligned",
                    e.file_offset
                )));
            }
            if (e.length as u64) % (1024 * 1024) != 0 {
                return Err(Error::InvalidStructure(format!(
                    "region length {:#x} not 1 MiB aligned",
                    e.length
                )));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; REGION_TABLE_SIZE as usize];
        buf[0..4].copy_from_slice(REGION_SIGNATURE);
        LittleEndian::write_u32(&mut buf[8..12], self.entries.len() as u32);
        for (i, e) in self.entries.iter().enumerate() {
            let off = REGION_TABLE_HEADER_SIZE + i * REGION_TABLE_ENTRY_SIZE;
            write_guid_at(&mut buf, off, e.guid);
            LittleEndian::write_u64(&mut buf[off + 16..off + 24], e.file_offset);
            LittleEndian::write_u32(&mut buf[off + 24..off + 28], e.length);
            let flags = if e.required { REGION_FLAG_REQUIRED } else { 0 };
            LittleEndian::write_u32(&mut buf[off + 28..off + 32], flags);
        }
        let cs = crc32c(&buf);
        LittleEndian::write_u32(&mut buf[4..8], cs);
        buf
    }
}

#[derive(Debug, Clone)]
pub struct RegionTablePair {
    pub tables: [Option<RegionTable>; 2],
    pub active_index: usize,
}

impl RegionTablePair {
    pub fn read(file: &mut File) -> Result<Self> {
        let mut buf0 = vec![0u8; REGION_TABLE_SIZE as usize];
        let mut buf1 = vec![0u8; REGION_TABLE_SIZE as usize];
        read_exact_at(file, region_table_offset(0), &mut buf0)?;
        read_exact_at(file, region_table_offset(1), &mut buf1)?;
        let r0 = RegionTable::parse(&buf0);
        let r1 = RegionTable::parse(&buf1);
        let r0v = r0.checksum_ok;
        let r1v = r1.checksum_ok;
        let active = match (r0v, r1v) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => return Err(Error::NoValidRegionTable),
        };
        Ok(RegionTablePair {
            tables: [
                if r0v { Some(r0) } else { None },
                if r1v { Some(r1) } else { None },
            ],
            active_index: active,
        })
    }

    pub fn active(&self) -> &RegionTable {
        self.tables[self.active_index]
            .as_ref()
            .expect("active region table must be Some")
    }

    pub fn write_both(file: &mut File, table: &RegionTable) -> Result<()> {
        let raw = table.encode();
        write_all_at(file, region_table_offset(0), &raw)?;
        write_all_at(file, region_table_offset(1), &raw)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_roundtrip() {
        let rt = RegionTable {
            checksum_ok: true,
            entries: vec![
                RegionEntry {
                    guid: REGION_GUID_BAT,
                    file_offset: 3 * 1024 * 1024,
                    length: 1024 * 1024,
                    required: true,
                },
                RegionEntry {
                    guid: REGION_GUID_METADATA,
                    file_offset: 2 * 1024 * 1024,
                    length: 1024 * 1024,
                    required: true,
                },
            ],
        };
        let buf = rt.encode();
        let parsed = RegionTable::parse(&buf);
        assert!(parsed.checksum_ok);
        assert_eq!(parsed.entries.len(), 2);
        assert!(parsed.bat().is_some());
        assert!(parsed.metadata().is_some());
        parsed.validate_layout().unwrap();
    }
}
