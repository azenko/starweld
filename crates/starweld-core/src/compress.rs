//! Filesystem-level compression helpers.
//!
//! The only platform that actually exposes a transparent block-level
//! compression knob for ordinary files is Windows / NTFS via the
//! `FILE_ATTRIBUTE_COMPRESSED` flag (LZNT1). When that flag is stamped on a
//! file *before* its data is written, NTFS transparently compresses every
//! cluster the application later writes — no extra read-modify-write pass is
//! ever needed.
//!
//! To keep the engine `#![forbid(unsafe_code)]`-clean we don't call
//! `DeviceIoControl(FSCTL_SET_COMPRESSION)` directly; we shell out to the
//! built-in [`compact.exe`][compact] command, which is shipped with every
//! Windows install since Windows 2000 and is the documented way to do this
//! from a script. On a 0-byte file, `compact /c` is essentially free — it
//! only flips the file attribute.
//!
//! [compact]: https://learn.microsoft.com/windows-server/administration/windows-commands/compact

use std::path::Path;

use crate::error::{Error, Result};

/// Stamp the NTFS `FILE_ATTRIBUTE_COMPRESSED` flag on a file so that every
/// cluster the operating system later writes to it is transparently
/// compressed (LZNT1).
///
/// The file at `path` is created if missing and truncated to 0 bytes before
/// the flag is set, so that the (otherwise expensive) `compact /c` invocation
/// has nothing to actually compress and only updates the file attribute. The
/// attribute survives any subsequent `O_TRUNC`-style reopen, which is exactly
/// what the VHDX writer does next.
///
/// Returns:
/// - [`Error::Unsupported`] on non-Windows hosts.
/// - [`Error::Unsupported`] on Windows when the underlying volume isn't NTFS,
///   when `compact.exe` is missing from `PATH`, or when the OS otherwise
///   refuses to set the flag (with the original message preserved).
/// - [`Error::Io`] for unrelated filesystem errors when truncating the file.
pub fn set_ntfs_compression(path: &Path) -> Result<()> {
    set_ntfs_compression_impl(path)
}

#[cfg(windows)]
fn set_ntfs_compression_impl(path: &Path) -> Result<()> {
    {
        let _empty = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
    }

    let output = match std::process::Command::new("compact")
        .arg("/c")
        .arg("/i")
        .arg("/q")
        .arg(path.as_os_str())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Unsupported(format!(
                "could not enable NTFS compression: `compact.exe` was not found in PATH ({e})"
            )));
        }
        Err(e) => return Err(Error::Io(e)),
    };

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match (stdout.trim(), stderr.trim()) {
            ("", "") => String::new(),
            (so, "") => format!(": {so}"),
            ("", se) => format!(": {se}"),
            (so, se) => format!(": {so} | {se}"),
        };
        return Err(Error::Unsupported(format!(
            "could not enable NTFS compression on {} (compact.exe exited with {}){detail}",
            path.display(),
            output.status,
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_ntfs_compression_impl(_path: &Path) -> Result<()> {
    Err(Error::Unsupported(
        "NTFS compression is only available on Windows hosts".into(),
    ))
}
