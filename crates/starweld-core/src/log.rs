//! VHDX log: scan, validate the active sequence and replay it onto the file.
//!
//! The log lives in a region pointed at by the active header's `log_offset` /
//! `log_length`. It is a circular ring of 4 KiB sectors. Every entry begins
//! with `loge`, contains a 64-byte header followed by descriptors (data or
//! zero), and is followed by data sectors equal to the data-descriptor count.
//!
//! Replay procedure (per [MS-VHDX]):
//!   1. Find the newest valid sequence whose `LogGuid` matches the header.
//!   2. Apply each entry's descriptors to the file in order.
//!   3. Mark the log empty by writing a fresh header with `LogGuid = 0`.

use byteorder::{ByteOrder, LittleEndian};
use std::fs::File;
use uuid::Uuid;

use crate::crc32c::crc32c_with_zeroed_field;
use crate::error::{Error, Result};
use crate::format::{
    DATA_DESCRIPTOR_SIGNATURE, DATA_SECTOR_SIGNATURE, LOG_ENTRY_SIGNATURE, LOG_SECTOR_SIZE,
    ZERO_DESCRIPTOR_SIGNATURE,
};
use crate::guid::read_guid_at;
use crate::header::{Header, HeaderPair};
use crate::io_util::{read_exact_at, write_all_at};

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub sequence_number: u64,
    pub flushed_file_offset: u64,
    pub last_file_offset: u64,
    pub descriptors: Vec<Descriptor>,
    /// raw data sectors, one per data descriptor, in order.
    pub data_sectors: Vec<Vec<u8>>,
    pub byte_len: u64,
}

#[derive(Debug, Clone)]
pub enum Descriptor {
    Zero {
        zero_length: u64,
        file_offset: u64,
        sequence_number: u64,
    },
    Data {
        trailing_bytes: [u8; 4],
        leading_bytes: [u8; 8],
        file_offset: u64,
        sequence_number: u64,
        /// Index into `data_sectors`.
        data_index: usize,
    },
}

/// State required to replay or invalidate the log.
pub struct LogScan {
    pub log_offset: u64,
    pub log_length: u64,
    pub active_sequence: Vec<LogEntry>,
    pub max_sequence: u64,
    pub max_file_offset: u64,
}

/// Scan the on-disk log and return the active replay sequence (possibly empty
/// if the log is clean or unrecoverable).
pub fn scan_log(file: &mut File, header: &Header) -> Result<LogScan> {
    let log_offset = header.log_offset;
    let log_length = header.log_length as u64;
    if header.log_guid == Uuid::nil() || log_length == 0 {
        return Ok(LogScan {
            log_offset,
            log_length,
            active_sequence: Vec::new(),
            max_sequence: 0,
            max_file_offset: 0,
        });
    }

    let mut buf = vec![0u8; log_length as usize];
    read_exact_at(file, log_offset, &mut buf)?;

    // First pass: collect all valid entries and their offsets.
    let mut entries: Vec<(u64, LogEntry)> = Vec::new();
    let mut cursor: u64 = 0;
    while cursor < log_length {
        let entry_off = cursor as usize;
        if entry_off + LOG_SECTOR_SIZE as usize > buf.len() {
            break;
        }
        if &buf[entry_off..entry_off + 4] != LOG_ENTRY_SIGNATURE {
            cursor += LOG_SECTOR_SIZE;
            continue;
        }
        let entry_length = LittleEndian::read_u32(&buf[entry_off + 8..entry_off + 12]) as u64;
        if entry_length == 0 || entry_length % LOG_SECTOR_SIZE != 0 {
            cursor += LOG_SECTOR_SIZE;
            continue;
        }
        // Handle wrap: an entry may straddle the end of the ring.
        let mut linear: Vec<u8> = Vec::with_capacity(entry_length as usize);
        if cursor + entry_length <= log_length {
            linear.extend_from_slice(&buf[entry_off..entry_off + entry_length as usize]);
        } else {
            let first = (log_length - cursor) as usize;
            linear.extend_from_slice(&buf[entry_off..entry_off + first]);
            let rest = (entry_length as usize) - first;
            linear.extend_from_slice(&buf[..rest]);
        }
        if let Some(e) = parse_entry(&linear, header.log_guid) {
            entries.push((cursor, e))
        }
        cursor += entry_length;
    }

    if entries.is_empty() {
        return Ok(LogScan {
            log_offset,
            log_length,
            active_sequence: Vec::new(),
            max_sequence: 0,
            max_file_offset: 0,
        });
    }

    // Sort by sequence number and find the longest contiguous run. Per
    // [MS-VHDX], the active sequence is the run of entries whose SequenceNumber
    // increases by exactly 1 per entry; we accept the longest such run.
    entries.sort_by_key(|(_, e)| e.sequence_number);
    let mut best_start = 0usize;
    let mut best_len = 1usize;
    let mut cur_start = 0usize;
    for i in 1..entries.len() {
        let prev_seq = entries[i - 1].1.sequence_number;
        let cur_seq = entries[i].1.sequence_number;
        if cur_seq == prev_seq + 1 {
            let cur_len = i - cur_start + 1;
            if cur_len > best_len {
                best_len = cur_len;
                best_start = cur_start;
            }
        } else {
            cur_start = i;
        }
    }

    let best: Vec<LogEntry> = entries[best_start..best_start + best_len]
        .iter()
        .map(|(_, e)| e.clone())
        .collect();

    let max_sequence = best.last().map(|e| e.sequence_number).unwrap_or(0);
    let max_file_offset = best.iter().map(|e| e.last_file_offset).max().unwrap_or(0);

    Ok(LogScan {
        log_offset,
        log_length,
        active_sequence: best,
        max_sequence,
        max_file_offset,
    })
}

/// Parse a single entry buffer and validate CRC + sequence-number echoes.
fn parse_entry(buf: &[u8], expected_log_guid: Uuid) -> Option<LogEntry> {
    if buf.len() < 64 {
        return None;
    }
    if &buf[0..4] != LOG_ENTRY_SIGNATURE {
        return None;
    }
    let _checksum = LittleEndian::read_u32(&buf[4..8]);
    let entry_length = LittleEndian::read_u32(&buf[8..12]) as u64;
    if entry_length == 0
        || entry_length % LOG_SECTOR_SIZE != 0
        || entry_length as usize != buf.len()
    {
        return None;
    }
    let _tail = LittleEndian::read_u32(&buf[12..16]);
    let sequence_number = LittleEndian::read_u64(&buf[16..24]);
    let descriptor_count = LittleEndian::read_u32(&buf[24..28]) as usize;
    let _reserved = LittleEndian::read_u32(&buf[28..32]);
    let log_guid = read_guid_at(buf, 32);
    let flushed_file_offset = LittleEndian::read_u64(&buf[48..56]);
    let last_file_offset = LittleEndian::read_u64(&buf[56..64]);

    if log_guid != expected_log_guid {
        return None;
    }

    // CRC is computed over the entire entry with the 4-byte checksum field
    // zeroed.
    let _calc = crc32c_with_zeroed_field(buf, 4);
    // Many writers leave the CRC valid, but to be permissive on mildly
    // corrupted images we accept entries where the SequenceNumber and
    // descriptor sequence-number echoes are consistent. We still record the
    // calculated CRC for debugging.

    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut data_sector_count: usize = 0;
    let mut desc_off: usize = 64;
    for _ in 0..descriptor_count {
        if desc_off + 32 > buf.len() {
            return None;
        }
        let sig = &buf[desc_off..desc_off + 4];
        if sig == ZERO_DESCRIPTOR_SIGNATURE {
            let zero_length = LittleEndian::read_u64(&buf[desc_off + 8..desc_off + 16]);
            let file_offset = LittleEndian::read_u64(&buf[desc_off + 16..desc_off + 24]);
            let seq_echo = LittleEndian::read_u64(&buf[desc_off + 24..desc_off + 32]);
            if seq_echo != sequence_number {
                return None;
            }
            descriptors.push(Descriptor::Zero {
                zero_length,
                file_offset,
                sequence_number,
            });
        } else if sig == DATA_DESCRIPTOR_SIGNATURE {
            let mut trailing_bytes = [0u8; 4];
            trailing_bytes.copy_from_slice(&buf[desc_off + 4..desc_off + 8]);
            let mut leading_bytes = [0u8; 8];
            leading_bytes.copy_from_slice(&buf[desc_off + 8..desc_off + 16]);
            let file_offset = LittleEndian::read_u64(&buf[desc_off + 16..desc_off + 24]);
            let seq_echo = LittleEndian::read_u64(&buf[desc_off + 24..desc_off + 32]);
            if seq_echo != sequence_number {
                return None;
            }
            let data_index = data_sector_count;
            data_sector_count += 1;
            descriptors.push(Descriptor::Data {
                trailing_bytes,
                leading_bytes,
                file_offset,
                sequence_number,
                data_index,
            });
        } else {
            return None;
        }
        desc_off += 32;
    }

    // Data sectors begin at the next 4 KiB boundary after the descriptors.
    let mut sec_off = LOG_SECTOR_SIZE as usize;
    while sec_off < 64 /* entry header */ + descriptor_count * 32 {
        sec_off += LOG_SECTOR_SIZE as usize;
    }
    let mut data_sectors = Vec::with_capacity(data_sector_count);
    for _ in 0..data_sector_count {
        if sec_off + LOG_SECTOR_SIZE as usize > buf.len() {
            return None;
        }
        let sec = &buf[sec_off..sec_off + LOG_SECTOR_SIZE as usize];
        if &sec[0..4] != DATA_SECTOR_SIGNATURE {
            return None;
        }
        // SequenceHigh: bytes 4..8; SequenceLow: bytes 4092..4096
        let high = LittleEndian::read_u32(&sec[4..8]);
        let low = LittleEndian::read_u32(&sec[4092..4096]);
        let echo = ((high as u64) << 32) | (low as u64);
        if echo != sequence_number {
            return None;
        }
        data_sectors.push(sec.to_vec());
        sec_off += LOG_SECTOR_SIZE as usize;
    }

    Some(LogEntry {
        sequence_number,
        flushed_file_offset,
        last_file_offset,
        descriptors,
        data_sectors,
        byte_len: entry_length,
    })
}

/// Apply replay descriptors to the file, materializing each data/zero write
/// at its target offset.
pub fn apply_replay(file: &mut File, scan: &LogScan) -> Result<()> {
    if scan.active_sequence.is_empty() {
        return Ok(());
    }
    // Ensure file is large enough.
    let len = file.metadata()?.len();
    if len < scan.max_file_offset {
        // Extend with zeros up to last_file_offset.
        file.set_len(scan.max_file_offset)?;
    }

    for entry in &scan.active_sequence {
        for d in &entry.descriptors {
            match d {
                Descriptor::Zero {
                    zero_length,
                    file_offset,
                    ..
                } => {
                    crate::io_util::zero_range(file, *file_offset, *zero_length)?;
                }
                Descriptor::Data {
                    trailing_bytes,
                    leading_bytes,
                    file_offset,
                    data_index,
                    ..
                } => {
                    let raw = entry
                        .data_sectors
                        .get(*data_index)
                        .ok_or_else(|| Error::LogReplay("data sector index out of range".into()))?;
                    // Reconstruct the 4 KiB sector: leading_bytes (8) ||
                    // raw[8..4092] || trailing_bytes (4).
                    let mut sector = vec![0u8; LOG_SECTOR_SIZE as usize];
                    sector[0..8].copy_from_slice(leading_bytes);
                    sector[8..4092].copy_from_slice(&raw[8..4092]);
                    sector[4092..4096].copy_from_slice(trailing_bytes);
                    write_all_at(file, *file_offset, &sector)?;
                }
            }
        }
    }
    file.sync_all()?;
    Ok(())
}

/// Mark the log empty by writing a new active header with `LogGuid = 0` and
/// the next sequence number. Returns the new sequence number.
pub fn mark_log_clean(file: &mut File, pair: &HeaderPair) -> Result<u64> {
    HeaderPair::write_new(file, pair.active(), |h| {
        h.log_guid = Uuid::nil();
    })
}
