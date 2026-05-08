//! VHDX file-type identifier and Header 1/Header 2 parse + arbitration.

use byteorder::{ByteOrder, LittleEndian};
use std::fs::File;
use uuid::Uuid;

use crate::crc32c::{crc32c, crc32c_with_zeroed_field};
use crate::error::{Error, Result};
use crate::format::{
    header_offset, FILE_TYPE_IDENTIFIER_OFFSET, FILE_TYPE_SIGNATURE, HEADER_FIXED_SIZE,
    HEADER_SIGNATURE, HEADER_SIZE,
};
use crate::guid::{read_guid_at, write_guid_at};
use crate::io_util::{read_exact_at, write_all_at};

/// The first 64 KiB of the file: the file type identifier. Only the first
/// 8 bytes are meaningful (the literal `vhdxfile`); the rest must be zero.
pub fn verify_file_type(file: &mut File) -> Result<()> {
    let mut sig = [0u8; 8];
    read_exact_at(file, FILE_TYPE_IDENTIFIER_OFFSET, &mut sig)?;
    if &sig != FILE_TYPE_SIGNATURE {
        return Err(Error::BadSignature { found: sig });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Header {
    pub signature_ok: bool,
    pub checksum_ok: bool,
    pub checksum: u32,
    pub sequence_number: u64,
    pub file_write_guid: Uuid,
    pub data_write_guid: Uuid,
    pub log_guid: Uuid,
    pub log_version: u16,
    pub version: u16,
    pub log_length: u32,
    pub log_offset: u64,
}

impl Header {
    pub fn parse(buf: &[u8]) -> Header {
        let mut h = Header {
            signature_ok: false,
            checksum_ok: false,
            checksum: 0,
            sequence_number: 0,
            file_write_guid: Uuid::nil(),
            data_write_guid: Uuid::nil(),
            log_guid: Uuid::nil(),
            log_version: 0,
            version: 0,
            log_length: 0,
            log_offset: 0,
        };
        if buf.len() < HEADER_FIXED_SIZE {
            return h;
        }
        if &buf[0..4] != HEADER_SIGNATURE {
            return h;
        }
        h.signature_ok = true;
        h.checksum = LittleEndian::read_u32(&buf[4..8]);
        h.sequence_number = LittleEndian::read_u64(&buf[8..16]);
        h.file_write_guid = read_guid_at(buf, 16);
        h.data_write_guid = read_guid_at(buf, 32);
        h.log_guid = read_guid_at(buf, 48);
        h.log_version = LittleEndian::read_u16(&buf[64..66]);
        h.version = LittleEndian::read_u16(&buf[66..68]);
        h.log_length = LittleEndian::read_u32(&buf[68..72]);
        h.log_offset = LittleEndian::read_u64(&buf[72..80]);

        let calc = crc32c_with_zeroed_field(&buf[..HEADER_FIXED_SIZE], 4);
        h.checksum_ok = calc == h.checksum;
        h
    }

    pub fn is_valid(&self) -> bool {
        self.signature_ok && self.checksum_ok && self.version == 1 && self.log_version == 0
    }

    /// Encode a header into a 64 KiB buffer (4 KiB used, rest zero).
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE as usize];
        buf[0..4].copy_from_slice(HEADER_SIGNATURE);
        // checksum (4..8) zeroed for now; updated below
        LittleEndian::write_u64(&mut buf[8..16], self.sequence_number);
        write_guid_at(&mut buf, 16, self.file_write_guid);
        write_guid_at(&mut buf, 32, self.data_write_guid);
        write_guid_at(&mut buf, 48, self.log_guid);
        LittleEndian::write_u16(&mut buf[64..66], self.log_version);
        LittleEndian::write_u16(&mut buf[66..68], self.version);
        LittleEndian::write_u32(&mut buf[68..72], self.log_length);
        LittleEndian::write_u64(&mut buf[72..80], self.log_offset);

        let cs = crc32c(&buf[..HEADER_FIXED_SIZE]);
        LittleEndian::write_u32(&mut buf[4..8], cs);
        buf
    }
}

/// A pair of headers loaded from disk plus the index of the active one.
#[derive(Debug, Clone)]
pub struct HeaderPair {
    pub headers: [Header; 2],
    pub raw: [Vec<u8>; 2],
    pub active_index: usize,
}

impl HeaderPair {
    pub fn read(file: &mut File) -> Result<Self> {
        let mut raw0 = vec![0u8; HEADER_SIZE as usize];
        let mut raw1 = vec![0u8; HEADER_SIZE as usize];
        read_exact_at(file, header_offset(0), &mut raw0)?;
        read_exact_at(file, header_offset(1), &mut raw1)?;
        let h0 = Header::parse(&raw0);
        let h1 = Header::parse(&raw1);

        let pick = match (h0.is_valid(), h1.is_valid()) {
            (true, true) => {
                if h0.sequence_number >= h1.sequence_number {
                    0
                } else {
                    1
                }
            }
            (true, false) => 0,
            (false, true) => 1,
            (false, false) => return Err(Error::NoValidHeader),
        };

        Ok(HeaderPair {
            headers: [h0, h1],
            raw: [raw0, raw1],
            active_index: pick,
        })
    }

    pub fn active(&self) -> &Header {
        &self.headers[self.active_index]
    }

    /// Write a new header, bumping the sequence number, and replace the OLDER
    /// of the two headers on disk. Returns the new sequence number.
    pub fn write_new(
        file: &mut File,
        current: &Header,
        mutate: impl FnOnce(&mut Header),
    ) -> Result<u64> {
        let mut new_h = *current;
        mutate(&mut new_h);
        new_h.sequence_number = current.sequence_number.wrapping_add(1);

        let raw = new_h.encode();

        // Re-read both headers to learn which has the lower sequence; that's
        // the slot we'll overwrite (so the just-written header becomes active).
        let pair = HeaderPair::read(file).ok();
        let target_idx = match pair {
            Some(p) => match (p.headers[0].is_valid(), p.headers[1].is_valid()) {
                (true, true) => {
                    if p.headers[0].sequence_number <= p.headers[1].sequence_number {
                        0
                    } else {
                        1
                    }
                }
                (true, false) => 1,
                (false, true) => 0,
                (false, false) => 0,
            },
            None => 0,
        };

        write_all_at(file, header_offset(target_idx), &raw)?;
        Ok(new_h.sequence_number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            signature_ok: true,
            checksum_ok: true,
            checksum: 0,
            sequence_number: 7,
            file_write_guid: Uuid::new_v4(),
            data_write_guid: Uuid::new_v4(),
            log_guid: Uuid::nil(),
            log_version: 0,
            version: 1,
            log_length: 1024 * 1024,
            log_offset: 1024 * 1024,
        };
        let buf = h.encode();
        let parsed = Header::parse(&buf);
        assert!(parsed.signature_ok);
        assert!(parsed.checksum_ok);
        assert!(parsed.is_valid());
        assert_eq!(parsed.sequence_number, 7);
        assert_eq!(parsed.file_write_guid, h.file_write_guid);
        assert_eq!(parsed.log_length, h.log_length);
    }
}
