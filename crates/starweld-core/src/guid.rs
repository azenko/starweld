//! Microsoft "mixed-endian" GUID helpers.
//!
//! On disk a GUID is stored as:
//!   - Data1: u32 little-endian
//!   - Data2: u16 little-endian
//!   - Data3: u16 little-endian
//!   - Data4: 8 bytes big-endian
//!
//! The [`uuid::Uuid`] crate's `from_bytes_le` / `to_bytes_le` perform exactly
//! this conversion against a string-form UUID, which is the canonical
//! representation used in [MS-VHDX].

use uuid::Uuid;

/// Parse a Microsoft mixed-endian 16-byte GUID into a `Uuid`.
pub fn read_guid(bytes: [u8; 16]) -> Uuid {
    Uuid::from_bytes_le(bytes)
}

/// Serialize a `Uuid` to 16 bytes in Microsoft mixed-endian form.
pub fn write_guid(uuid: Uuid) -> [u8; 16] {
    uuid.to_bytes_le()
}

/// Convenience: read a GUID at the given offset of `buf`.
pub fn read_guid_at(buf: &[u8], off: usize) -> Uuid {
    let mut g = [0u8; 16];
    g.copy_from_slice(&buf[off..off + 16]);
    read_guid(g)
}

/// Convenience: write a GUID at the given offset of `buf`.
pub fn write_guid_at(buf: &mut [u8], off: usize, uuid: Uuid) {
    buf[off..off + 16].copy_from_slice(&write_guid(uuid));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let u = Uuid::parse_str("2DC27766-F623-4200-9D64-115E9BFD4A08").unwrap();
        let bytes = write_guid(u);
        let back = read_guid(bytes);
        assert_eq!(back, u);
    }

    #[test]
    fn known_layout() {
        let u = Uuid::parse_str("00112233-4455-6677-8899-AABBCCDDEEFF").unwrap();
        let bytes = write_guid(u);
        assert_eq!(
            bytes,
            [
                0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF
            ]
        );
    }
}
