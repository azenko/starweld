//! Small helpers for positional I/O against `std::fs::File` without using
//! platform-specific APIs.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::Result;

/// Read exactly `buf.len()` bytes from `file` starting at `offset`.
pub fn read_exact_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)?;
    Ok(())
}

/// Write exactly `buf.len()` bytes into `file` starting at `offset`.
pub fn write_all_at(file: &mut File, offset: u64, buf: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buf)?;
    Ok(())
}

/// Read up to `buf.len()` bytes; returns the number actually read (only short
/// at EOF).
pub fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<usize> {
    file.seek(SeekFrom::Start(offset))?;
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// Round `n` up to the nearest multiple of `align` (`align` must be > 0).
pub fn align_up(n: u64, align: u64) -> u64 {
    debug_assert!(align > 0);
    let r = n % align;
    if r == 0 {
        n
    } else {
        n + (align - r)
    }
}

pub fn is_all_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

/// Fully zero a buffer of `len` bytes starting at `offset` in the file by
/// writing in 64 KiB chunks.
pub fn zero_range(file: &mut File, offset: u64, len: u64) -> Result<()> {
    let mut remaining = len;
    let mut cursor = offset;
    let chunk = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        write_all_at(file, cursor, &chunk[..n])?;
        cursor += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}

pub fn fsync(file: &File) -> Result<()> {
    file.sync_all()?;
    Ok(())
}

/// Tell the OS that the cached pages backing the byte range `[offset,
/// offset + len)` of `file` will not be needed again, so it is free to
/// drop them from the page cache.
///
/// On Linux this issues `posix_fadvise(POSIX_FADV_DONTNEED)`, which is the
/// same hint that `dd iflag=nocache` and `tar --no-fscache` use to keep
/// streaming reads from squeezing out the rest of the system's working
/// set on memory-constrained hosts. The kernel may comply immediately or
/// lazily; in practice on Linux the pages are dropped synchronously when
/// they are clean.
///
/// `len == 0` advises from `offset` to the end of the file, matching
/// `posix_fadvise`'s own convention.
///
/// On non-Unix targets (Windows, WASI without `fadvise`) this is a silent
/// no-op: `read()` on those platforms still goes through the OS cache,
/// but there is no portable way for an unprivileged process to evict it
/// from the cache after the fact.
pub fn drop_page_cache(file: &File, offset: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{fadvise, Advice};
        use std::num::NonZeroU64;
        let len_arg: Option<NonZeroU64> = if len == 0 { None } else { NonZeroU64::new(len) };
        let _ = fadvise(file, offset, len_arg, Advice::DontNeed);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, offset, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_examples() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }

    #[test]
    fn detect_zeros() {
        assert!(is_all_zero(&[0u8; 16]));
        assert!(!is_all_zero(&[0u8, 0, 0, 1]));
    }
}
