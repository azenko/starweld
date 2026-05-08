//! Resilience tests: truncated tail, both-headers-corrupt-but-recoverable, etc.

use starweld_core::disk::VhdxDisk;
use starweld_core::repair::{self, FsckOptions};
use starweld_core::writer::{BlockSource, VhdxWriter, WriterParams};
use uuid::Uuid;

fn make_image(path: &std::path::Path) {
    let mut params = WriterParams::dynamic(4 * 1024 * 1024);
    params.block_size = 1024 * 1024;
    params.data_write_guid = Uuid::new_v4();
    let mut w = VhdxWriter::create(path, params).unwrap();
    let mut b = vec![0u8; 1024 * 1024];
    b[0] = 0xAA;
    b[100] = 0xBB;
    w.write_block(0, BlockSource::Buf(&b)).unwrap();
    w.finish().unwrap();
}

#[test]
fn fsck_dry_run_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    make_image(&path);

    let original = std::fs::read(&path).unwrap();

    let report = repair::fsck(
        &path,
        FsckOptions {
            fix: false,
            dry_run: true,
            allow_log_discard: true,
        },
    )
    .unwrap();
    assert!(report.bat_issues.is_empty());

    let after = std::fs::read(&path).unwrap();
    assert_eq!(original, after, "dry-run must not mutate the file");
}

#[test]
fn fake_dirty_log_is_discarded_on_fix() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    make_image(&path);

    // Open the file and patch the active header to advertise a non-zero
    // LogGuid (the log region is empty, so replay won't find anything; --fix
    // should discard the log and re-stamp LogGuid=0).
    {
        use starweld_core::format::{header_offset, HEADER_SIZE};
        use starweld_core::header::{Header, HeaderPair};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let pair = HeaderPair::read(&mut f).unwrap();
        let active = *pair.active();
        // Build a corrupted header with a fake LogGuid.
        let mut new_h = active;
        new_h.log_guid = Uuid::new_v4();
        new_h.sequence_number = active.sequence_number.wrapping_add(1);
        let raw = new_h.encode();
        // Overwrite both header slots so the dirty header is unambiguously
        // active.
        use std::io::{Seek, SeekFrom, Write};
        f.seek(SeekFrom::Start(header_offset(0))).unwrap();
        f.write_all(&raw).unwrap();
        let mut new_h2 = active;
        new_h2.log_guid = new_h.log_guid;
        new_h2.sequence_number = new_h.sequence_number.wrapping_sub(1);
        let raw2 = new_h2.encode();
        f.seek(SeekFrom::Start(header_offset(1))).unwrap();
        f.write_all(&raw2).unwrap();
        let _ = HEADER_SIZE;
        let _ = Header { ..active };
    }

    // Without --fix, opening for write should fail with DirtyLog.
    let err = VhdxDisk::open(&path, false).err().unwrap();
    assert!(matches!(err, starweld_core::Error::DirtyLog), "got {err:?}");

    // With --fix and allow_log_discard, the log is invalidated and the file
    // becomes openable for write.
    let report = repair::fsck(
        &path,
        FsckOptions {
            fix: true,
            dry_run: false,
            allow_log_discard: true,
        },
    )
    .unwrap();
    assert!(report.fixed);
    let _ = VhdxDisk::open(&path, false).unwrap();
}

#[test]
fn fsck_detects_truncated_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    make_image(&path);

    // Truncate the file so the payload block falls past EOF.
    let len = std::fs::metadata(&path).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(len - 1024 * 1024).unwrap();

    let report = repair::fsck(
        &path,
        FsckOptions {
            fix: false,
            dry_run: true,
            allow_log_discard: false,
        },
    )
    .unwrap();
    assert!(
        !report.bat_issues.is_empty(),
        "expected BAT issues for truncated payload, got {report:?}"
    );
}
