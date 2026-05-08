//! Minimal POSIX ustar tar header writer.
//!
//! Used by the streaming-zstd flatten path to wrap the inner VHDX in a
//! single-file tar archive *before* zstd-compresses it, so the resulting
//! `.vhdx.tar.zst` is recognised as an archive by:
//!
//! - GNU `tar` (any version) and BSD / libarchive `tar`.
//! - Windows 11 Explorer (libarchive integration, 24H2+).
//! - 7-Zip / PeaZip / NanaZip on Windows 10.
//! - macOS Finder (BSD `tar`).
//!
//! The implementation supports two encodings of the file-size field:
//!
//! 1. **Standard ustar octal** — used when the inner VHDX is at most
//!    `8 GiB - 1` byte. Eleven octal digits + NUL fit into the 12-byte
//!    field.
//! 2. **GNU base-256 binary** — kicks in for larger files. Byte 0 is
//!    `0x80` (positive infinity flag), bytes 1..12 are the size as a
//!    big-endian unsigned integer. Supported by GNU tar, libarchive,
//!    7-Zip and BSD tar; effectively universal in 2026.
//!
//! Modes / uid / gid / mtime are not security-sensitive for our use case
//! (the artefact is a single VHDX produced by a CLI run, not a Unix
//! permission dump), so we hard-code mode `0644`, uid/gid `0`, and let
//! the caller supply an mtime (the CLI passes the current Unix time).

use std::io::Write;

use crate::error::Result;

/// Length of one tar block.
pub const TAR_BLOCK_SIZE: usize = 512;
/// One tar header is exactly one block.
pub const TAR_HEADER_SIZE: usize = TAR_BLOCK_SIZE;
/// End-of-archive marker: two consecutive zero blocks.
pub const TAR_EOF_SIZE: usize = 2 * TAR_BLOCK_SIZE;

/// Maximum filename length that fits into the basic `name` field of a
/// ustar header. Anything larger would require the `prefix` field or a
/// PAX extended header; for our use case the inner filename is always a
/// short `flat.vhdx`-style basename, so we just enforce the limit.
const NAME_LEN: usize = 100;

/// Largest inner-file size representable as 11 octal digits in the
/// ustar `size` field (`0o7_777_777_777_7` — 8 GiB - 1).
const MAX_OCTAL_SIZE: u64 = 0o77_777_777_777;

/// Write a 512-byte ustar header for a single regular file named `name`
/// containing `size` bytes.
///
/// `mtime` is a Unix timestamp (seconds since the epoch); pass the
/// current time to have `tar -tvf` show today's date for the entry.
///
/// Returns an [`Error::Unsupported`](crate::error::Error::Unsupported) if
/// the filename exceeds 100 bytes — callers should always pass a basename.
pub fn write_ustar_header<W: Write>(out: &mut W, name: &str, size: u64, mtime: u64) -> Result<()> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > NAME_LEN {
        return Err(crate::error::Error::Unsupported(format!(
            "tar entry filename longer than {NAME_LEN} bytes is not supported: {name:?}"
        )));
    }

    let mut h = [0u8; TAR_HEADER_SIZE];

    // name (0..100)
    h[..name_bytes.len()].copy_from_slice(name_bytes);

    // mode (100..108): "0000644\0"
    write_octal_field(&mut h[100..108], 0o644);
    // uid (108..116) and gid (116..124): root
    write_octal_field(&mut h[108..116], 0);
    write_octal_field(&mut h[116..124], 0);

    // size (124..136): octal if it fits, else GNU base-256
    write_size_field(&mut h[124..136], size);

    // mtime (136..148): 11 octal digits + NUL
    write_octal_field(&mut h[136..148], mtime);

    // chksum (148..156): seed with eight spaces for the checksum sum,
    // then overwrite with the formatted result below.
    h[148..156].copy_from_slice(b"        ");

    // typeflag (156..157): '0' = regular file
    h[156] = b'0';

    // linkname (157..257): zero-filled (regular files have no link target)

    // magic (257..263): "ustar\0", version (263..265): "00"
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");

    // uname (265..297) / gname (297..329): "root\0..."
    h[265..269].copy_from_slice(b"root");
    h[297..301].copy_from_slice(b"root");

    // devmajor (329..337) / devminor (337..345)
    write_octal_field(&mut h[329..337], 0);
    write_octal_field(&mut h[337..345], 0);

    // prefix (345..500) and pad (500..512) stay zero.

    // Now compute the header checksum: unsigned sum of every byte with the
    // chksum field treated as eight ASCII spaces (which we already wrote
    // above, so we can just sum the buffer as-is).
    let chksum: u32 = h.iter().map(|&b| b as u32).sum();
    // chksum format per POSIX.1-1988: 6 octal digits, NUL, space.
    let chk_str = format!("{:06o}", chksum);
    let chk_bytes = chk_str.as_bytes();
    debug_assert_eq!(chk_bytes.len(), 6);
    h[148..154].copy_from_slice(chk_bytes);
    h[154] = 0;
    h[155] = b' ';

    out.write_all(&h)?;
    Ok(())
}

/// Write the 1024-byte tar end-of-archive marker (two consecutive zero
/// blocks). All standard tar implementations stop reading here.
pub fn write_ustar_eof<W: Write>(out: &mut W) -> Result<()> {
    let zeros = [0u8; TAR_EOF_SIZE];
    out.write_all(&zeros)?;
    Ok(())
}

/// Number of zero bytes required to round `size` up to a 512-byte
/// boundary (tar pads every file's content with zeros to the next block).
pub fn padding_to_block(size: u64) -> u64 {
    let r = size % TAR_BLOCK_SIZE as u64;
    if r == 0 {
        0
    } else {
        TAR_BLOCK_SIZE as u64 - r
    }
}

// ---------------------------------------------------------------------------

/// Write `value` as `(buf.len() - 1)` octal digits followed by a NUL byte
/// into `buf`. Used for the `mode`, `uid`, `gid`, `mtime`, `devmajor` and
/// `devminor` fields, all of which use this NUL-terminated octal format.
fn write_octal_field(buf: &mut [u8], value: u64) {
    debug_assert!(buf.len() >= 2);
    let width = buf.len() - 1;
    let s = format!("{:0width$o}", value, width = width);
    let bytes = s.as_bytes();
    let n = bytes.len().min(width);
    // If value overflows the field, we truncate to the low-order digits;
    // this only happens for values that don't fit in `width` octal digits.
    // None of the fields we use this for should ever overflow.
    let off = width - n;
    buf[..off].fill(b'0');
    buf[off..off + n].copy_from_slice(&bytes[bytes.len() - n..]);
    buf[buf.len() - 1] = 0;
}

/// Write the 12-byte `size` field. Falls back from octal to GNU base-256
/// for files that exceed 8 GiB - 1 byte.
fn write_size_field(buf: &mut [u8], size: u64) {
    debug_assert_eq!(buf.len(), 12);
    if size <= MAX_OCTAL_SIZE {
        let s = format!("{:011o}", size);
        let bytes = s.as_bytes();
        debug_assert_eq!(bytes.len(), 11);
        buf[..11].copy_from_slice(bytes);
        buf[11] = 0;
    } else {
        // GNU base-256: high bit of byte 0 set, remaining bytes are the
        // value as a big-endian unsigned integer. The 12-byte field gives
        // us 11 value bytes (88 bits) — far more than any conceivable
        // VHDX can need.
        buf[0] = 0x80;
        // u64 fits in the low 8 bytes; bytes 1..4 stay zero (high padding).
        buf[1..4].fill(0);
        buf[4..12].copy_from_slice(&size.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the header round-trips through GNU tar's checksum logic.
    fn parse_chksum(h: &[u8]) -> u32 {
        // The chksum field is at offset 148, length 8; format "%06o\0 ".
        let s = std::str::from_utf8(&h[148..154]).unwrap();
        u32::from_str_radix(s, 8).unwrap()
    }

    fn computed_chksum(h: &[u8]) -> u32 {
        let mut h = h.to_vec();
        // Replace chksum field with spaces to recompute.
        h[148..156].copy_from_slice(b"        ");
        h.iter().map(|&b| b as u32).sum()
    }

    #[test]
    fn small_header_octal_size() {
        let mut buf: Vec<u8> = Vec::new();
        write_ustar_header(&mut buf, "flat.vhdx", 1024, 1_700_000_000).unwrap();
        assert_eq!(buf.len(), TAR_HEADER_SIZE);
        // Magic/version present.
        assert_eq!(&buf[257..263], b"ustar\0");
        assert_eq!(&buf[263..265], b"00");
        // typeflag = '0' (regular file).
        assert_eq!(buf[156], b'0');
        // Filename round-trip.
        let name_end = buf[..100].iter().position(|&b| b == 0).unwrap();
        assert_eq!(&buf[..name_end], b"flat.vhdx");
        // Size field is "00000002000\0" (octal of 1024).
        assert_eq!(&buf[124..136], b"00000002000\0");
        // Checksum self-consistent.
        assert_eq!(parse_chksum(&buf), computed_chksum(&buf));
    }

    #[test]
    fn large_header_uses_base256() {
        // 16 GiB — exceeds the octal limit, must use base-256.
        let size: u64 = 16 * 1024 * 1024 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        write_ustar_header(&mut buf, "big.vhdx", size, 0).unwrap();
        // Sentinel byte for base-256.
        assert_eq!(buf[124], 0x80);
        // Last 8 bytes of the size field decode to `size` big-endian.
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[128..136]);
        assert_eq!(u64::from_be_bytes(bytes), size);
        // Checksum still self-consistent under the base-256 encoding.
        assert_eq!(parse_chksum(&buf), computed_chksum(&buf));
    }

    #[test]
    fn rejects_overlong_filename() {
        let too_long = "a".repeat(101);
        let mut buf: Vec<u8> = Vec::new();
        let err = write_ustar_header(&mut buf, &too_long, 0, 0).unwrap_err();
        match err {
            crate::error::Error::Unsupported(msg) => {
                assert!(msg.contains("100"), "unexpected message: {msg}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn padding_helper() {
        assert_eq!(padding_to_block(0), 0);
        assert_eq!(padding_to_block(1), 511);
        assert_eq!(padding_to_block(511), 1);
        assert_eq!(padding_to_block(512), 0);
        assert_eq!(padding_to_block(1024), 0);
        assert_eq!(padding_to_block(1025), 511);
    }

    #[test]
    fn eof_marker_is_two_zero_blocks() {
        let mut buf: Vec<u8> = Vec::new();
        write_ustar_eof(&mut buf).unwrap();
        assert_eq!(buf.len(), 1024);
        assert!(buf.iter().all(|&b| b == 0));
    }
}
