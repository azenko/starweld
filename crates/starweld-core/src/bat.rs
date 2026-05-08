//! Block Allocation Table.

use byteorder::{ByteOrder, LittleEndian};

use crate::format::{
    PAYLOAD_BLOCK_FULLY_PRESENT, PAYLOAD_BLOCK_NOT_PRESENT, PAYLOAD_BLOCK_PARTIALLY_PRESENT,
    PAYLOAD_BLOCK_UNDEFINED, PAYLOAD_BLOCK_UNMAPPED, PAYLOAD_BLOCK_ZERO, SB_BLOCK_NOT_PRESENT,
    SB_BLOCK_PRESENT,
};

/// One entry in the BAT, encoded as a 64-bit little-endian word:
///   - bits 0..3:   state (3 bits)
///   - bits 3..20:  reserved (must be zero)
///   - bits 20..64: file offset in MiB units
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatEntry(pub u64);

impl BatEntry {
    pub fn empty() -> Self {
        BatEntry(0)
    }

    pub fn new(state: u8, file_offset: u64) -> Self {
        debug_assert!(file_offset % (1024 * 1024) == 0);
        let mb = file_offset >> 20;
        BatEntry((state as u64 & 0x7) | (mb << 20))
    }

    pub fn state(&self) -> u8 {
        (self.0 & 0x7) as u8
    }

    pub fn file_offset(&self) -> u64 {
        (self.0 >> 20) << 20
    }

    pub fn raw(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct Bat {
    pub entries: Vec<BatEntry>,
    pub chunk_ratio: u64,
    pub block_count: u64,
}

impl Bat {
    pub fn parse(buf: &[u8], block_count: u64, chunk_ratio: u64, has_parent: bool) -> Self {
        let total_entries = total_bat_entries(block_count, chunk_ratio, has_parent);
        let max_from_buf = buf.len() / 8;
        let limit = total_entries.min(max_from_buf);
        let mut entries = Vec::with_capacity(limit);
        for i in 0..limit {
            let off = i * 8;
            entries.push(BatEntry(LittleEndian::read_u64(&buf[off..off + 8])));
        }
        Bat {
            entries,
            chunk_ratio,
            block_count,
        }
    }

    pub fn payload_entry(&self, virt_block: u64) -> Option<BatEntry> {
        let idx = payload_index(virt_block, self.chunk_ratio);
        self.entries.get(idx as usize).copied()
    }

    pub fn set_payload_entry(&mut self, virt_block: u64, e: BatEntry) {
        let idx = payload_index(virt_block, self.chunk_ratio);
        if (idx as usize) < self.entries.len() {
            self.entries[idx as usize] = e;
        }
    }

    /// Sector bitmap entry for the chunk that owns `virt_block`. Only present
    /// in differencing files; for non-differencing files this method still
    /// returns the entry that exists (may be `NOT_PRESENT`).
    pub fn sector_bitmap_entry(&self, virt_block: u64) -> Option<BatEntry> {
        let idx = sector_bitmap_index(virt_block, self.chunk_ratio);
        self.entries.get(idx as usize).copied()
    }

    pub fn set_sector_bitmap_entry(&mut self, virt_block: u64, e: BatEntry) {
        let idx = sector_bitmap_index(virt_block, self.chunk_ratio);
        if (idx as usize) < self.entries.len() {
            self.entries[idx as usize] = e;
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.entries.len() * 8];
        for (i, e) in self.entries.iter().enumerate() {
            LittleEndian::write_u64(&mut buf[i * 8..i * 8 + 8], e.raw());
        }
        buf
    }
}

/// Number of payload BAT entries before the first sector-bitmap entry.
pub fn payload_index(virt_block: u64, chunk_ratio: u64) -> u64 {
    // Within each "chunk", there are `chunk_ratio` payload entries followed by
    // 1 sector-bitmap entry. Differencing files always include the sector-
    // bitmap entries; non-differencing files only allocate payload entries
    // for blocks that actually exist (entries up to `block_count`).
    let chunk = virt_block / chunk_ratio;
    let offset = virt_block % chunk_ratio;
    chunk * (chunk_ratio + 1) + offset
}

pub fn sector_bitmap_index(virt_block: u64, chunk_ratio: u64) -> u64 {
    let chunk = virt_block / chunk_ratio;
    chunk * (chunk_ratio + 1) + chunk_ratio
}

pub fn total_bat_entries(block_count: u64, chunk_ratio: u64, has_parent: bool) -> usize {
    if block_count == 0 {
        return 0;
    }
    let last_block = block_count - 1;
    let last_chunk = last_block / chunk_ratio;
    if has_parent {
        // Up to and including the sector-bitmap entry of the last chunk.
        ((last_chunk + 1) * (chunk_ratio + 1)) as usize
    } else {
        // Up to and including the last payload entry.
        let in_chunk = last_block % chunk_ratio;
        (last_chunk * (chunk_ratio + 1) + in_chunk + 1) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadState {
    NotPresent,
    Undefined,
    Zero,
    Unmapped,
    FullyPresent,
    PartiallyPresent,
    Other(u8),
}

impl PayloadState {
    pub fn from_raw(s: u8) -> PayloadState {
        match s {
            PAYLOAD_BLOCK_NOT_PRESENT => PayloadState::NotPresent,
            PAYLOAD_BLOCK_UNDEFINED => PayloadState::Undefined,
            PAYLOAD_BLOCK_ZERO => PayloadState::Zero,
            PAYLOAD_BLOCK_UNMAPPED => PayloadState::Unmapped,
            PAYLOAD_BLOCK_FULLY_PRESENT => PayloadState::FullyPresent,
            PAYLOAD_BLOCK_PARTIALLY_PRESENT => PayloadState::PartiallyPresent,
            other => PayloadState::Other(other),
        }
    }

    pub fn raw(&self) -> u8 {
        match self {
            PayloadState::NotPresent => PAYLOAD_BLOCK_NOT_PRESENT,
            PayloadState::Undefined => PAYLOAD_BLOCK_UNDEFINED,
            PayloadState::Zero => PAYLOAD_BLOCK_ZERO,
            PayloadState::Unmapped => PAYLOAD_BLOCK_UNMAPPED,
            PayloadState::FullyPresent => PAYLOAD_BLOCK_FULLY_PRESENT,
            PayloadState::PartiallyPresent => PAYLOAD_BLOCK_PARTIALLY_PRESENT,
            PayloadState::Other(o) => *o,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorBitmapState {
    NotPresent,
    Present,
    Other(u8),
}

impl SectorBitmapState {
    pub fn from_raw(s: u8) -> SectorBitmapState {
        match s {
            SB_BLOCK_NOT_PRESENT => SectorBitmapState::NotPresent,
            SB_BLOCK_PRESENT => SectorBitmapState::Present,
            other => SectorBitmapState::Other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_layout() {
        let e = BatEntry::new(PAYLOAD_BLOCK_FULLY_PRESENT, 5 * 1024 * 1024);
        assert_eq!(e.state(), PAYLOAD_BLOCK_FULLY_PRESENT);
        assert_eq!(e.file_offset(), 5 * 1024 * 1024);
    }

    #[test]
    fn indexing() {
        // chunk_ratio = 2048 for 32 MiB block + 512-byte sector
        let cr: u64 = 2048;
        assert_eq!(payload_index(0, cr), 0);
        assert_eq!(payload_index(1, cr), 1);
        assert_eq!(payload_index(2047, cr), 2047);
        assert_eq!(sector_bitmap_index(0, cr), 2048);
        assert_eq!(payload_index(2048, cr), 2049);
    }

    #[test]
    fn total_entries() {
        // 1 block, no parent, chunk_ratio 2048 → 1 payload entry.
        assert_eq!(total_bat_entries(1, 2048, false), 1);
        // 1 block, parent, chunk_ratio 2048 → 2049 entries (chunk + sb).
        assert_eq!(total_bat_entries(1, 2048, true), 2049);
        // 2049 blocks, no parent → 2 chunks, second has 1 payload only.
        assert_eq!(total_bat_entries(2049, 2048, false), 2050);
    }
}
