//! Helpers around CRC32C (Castagnoli), the checksum used throughout VHDX.

pub fn crc32c(buf: &[u8]) -> u32 {
    crc32c::crc32c(buf)
}

/// Compute a CRC32C over `buf` after temporarily zeroing the 4-byte field at
/// `field_offset`. This is the standard MS-VHDX scheme: the checksum field is
/// zeroed in-place, the structure is hashed, and the resulting value is then
/// stored back into that field.
pub fn crc32c_with_zeroed_field(buf: &[u8], field_offset: usize) -> u32 {
    let mut tmp = buf.to_vec();
    if field_offset + 4 <= tmp.len() {
        tmp[field_offset..field_offset + 4].copy_from_slice(&[0u8; 4]);
    }
    crc32c::crc32c(&tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(crc32c(&[]), 0);
    }

    #[test]
    fn known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE3069283);
    }

    #[test]
    fn zeroed_field_is_idempotent() {
        let mut buf = vec![0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        let c1 = crc32c_with_zeroed_field(&buf, 4);
        buf[4..8].copy_from_slice(&[0u8; 4]);
        let c2 = crc32c(&buf);
        assert_eq!(c1, c2);
    }
}
