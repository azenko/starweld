use starweld_core::compact::{self, CompactOptions};
use starweld_core::disk::{BlockRead, VhdxDisk};
use starweld_core::flatten::{self, FlattenEvent, FlattenOptions, FlattenSubformat};
use starweld_core::merge::{self, MergeOptions};
use starweld_core::parent_locator::ParentLocator;
use starweld_core::repair::{self, FsckOptions};
use starweld_core::writer::{BlockSource, VhdxWriter, WriterParams};
use starweld_core::zst_compress::ZstdParams;
use std::path::Path;
use uuid::Uuid;

const VIRTUAL_DISK_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB
const BLOCK_SIZE: u32 = 1024 * 1024; // 1 MiB blocks (4 blocks total)

fn create_parent(path: &Path, parent_data_guid: Uuid) -> WriterParams {
    let mut params = WriterParams::dynamic(VIRTUAL_DISK_SIZE);
    params.block_size = BLOCK_SIZE;
    params.data_write_guid = parent_data_guid;
    params.file_write_guid = parent_data_guid;
    params.page_83_data = Uuid::new_v4();

    let mut writer = VhdxWriter::create(path, params.clone()).unwrap();
    let mut block0 = vec![0u8; BLOCK_SIZE as usize];
    for (i, b) in block0.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let mut block2 = vec![0u8; BLOCK_SIZE as usize];
    block2[0..6].copy_from_slice(b"PARENT");

    writer.write_block(0, BlockSource::Buf(&block0)).unwrap();
    writer.write_block(2, BlockSource::Buf(&block2)).unwrap();
    writer.finish().unwrap();
    params
}

fn create_child(path: &Path, parent_path: &Path, parent_data_guid: Uuid) {
    let pl = ParentLocator::new_vhdx_relative(
        parent_path.file_name().unwrap().to_str().unwrap(),
        parent_data_guid,
    );
    let mut params = WriterParams::dynamic(VIRTUAL_DISK_SIZE);
    params.block_size = BLOCK_SIZE;
    params.parent_locator = Some(pl);

    let mut writer = VhdxWriter::create(path, params).unwrap();
    let mut block1 = vec![0u8; BLOCK_SIZE as usize];
    block1[0..5].copy_from_slice(b"CHILD");

    writer.write_block(1, BlockSource::Buf(&block1)).unwrap();
    writer.finish().unwrap();
}

#[test]
fn open_dynamic_image_built_with_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    create_parent(&path, Uuid::new_v4());

    let (disk, mut file) = VhdxDisk::open(&path, true).unwrap();
    assert_eq!(disk.virtual_disk_size(), VIRTUAL_DISK_SIZE);
    assert_eq!(disk.block_size(), BLOCK_SIZE);
    assert_eq!(disk.block_count(), 4);
    assert!(!disk.has_parent());

    let r0 = disk.read_block(&mut file, 0).unwrap();
    match r0 {
        BlockRead::Full(b) => {
            assert_eq!(b.len(), BLOCK_SIZE as usize);
            assert_eq!(b[0], 0);
            assert_eq!(b[1], 1);
            assert_eq!(b[250], 250);
            assert_eq!(b[251], 0);
            assert_eq!(b[252], 1);
        }
        _ => panic!("expected fully present block 0"),
    }

    let r2 = disk.read_block(&mut file, 2).unwrap();
    match r2 {
        BlockRead::Full(b) => {
            assert_eq!(&b[0..6], b"PARENT");
        }
        _ => panic!("expected fully present block 2"),
    }

    // Block 1 was never written; should be Absent (NotPresent).
    let r1 = disk.read_block(&mut file, 1).unwrap();
    assert!(matches!(r1, BlockRead::Absent));
}

#[test]
fn fsck_reports_clean_image() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    create_parent(&path, Uuid::new_v4());

    let report = repair::fsck(
        &path,
        FsckOptions {
            fix: false,
            dry_run: true,
            allow_log_discard: false,
        },
    )
    .unwrap();
    assert!(report.bat_issues.is_empty());
    assert!(matches!(
        report.header_status,
        repair::HeaderStatus::BothValid
    ));
    assert!(matches!(
        report.region_status,
        repair::RegionStatus::BothValid
    ));
}

#[test]
fn flatten_chain_to_standalone() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let report = flatten::flatten(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: None,
        },
    )
    .unwrap();
    assert_eq!(report.blocks_total, 4);

    let (disk, mut file) = VhdxDisk::open(&out, true).unwrap();
    assert!(!disk.has_parent());

    let r0 = disk.read_block(&mut file, 0).unwrap();
    match r0 {
        BlockRead::Full(b) => assert_eq!(b[1], 1),
        _ => panic!("expected fully present block 0 (from parent)"),
    }
    let r1 = disk.read_block(&mut file, 1).unwrap();
    match r1 {
        BlockRead::Full(b) => assert_eq!(&b[0..5], b"CHILD"),
        _ => panic!("expected fully present block 1 (from child)"),
    }
    let r2 = disk.read_block(&mut file, 2).unwrap();
    match r2 {
        BlockRead::Full(b) => assert_eq!(&b[0..6], b"PARENT"),
        _ => panic!("expected fully present block 2 (from parent)"),
    }
    // Block 3 was never written by anyone → should be Absent (zero-elided).
    let r3 = disk.read_block(&mut file, 3).unwrap();
    assert!(matches!(r3, BlockRead::Absent | BlockRead::Full(_)));
}

#[test]
fn flatten_emits_progress_events() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let mut events: Vec<FlattenEvent> = Vec::new();
    let report = flatten::flatten_with_progress(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: None,
        },
        |ev| events.push(ev),
    )
    .unwrap();
    assert_eq!(report.blocks_total, 4);

    // Exactly one Started, one BlockDone per block, exactly one Finished, in
    // that order.
    assert!(events.len() >= 6);
    match events.first().unwrap() {
        FlattenEvent::Started {
            total_blocks,
            block_size,
            total_bytes,
        } => {
            assert_eq!(*total_blocks, report.blocks_total);
            assert_eq!(*block_size, BLOCK_SIZE);
            assert_eq!(*total_bytes, report.blocks_total * BLOCK_SIZE as u64);
        }
        other => panic!("expected Started first, got {other:?}"),
    }
    assert!(matches!(events.last().unwrap(), FlattenEvent::Finished));

    let block_dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            FlattenEvent::BlockDone {
                index,
                bytes,
                wrote_bytes,
            } => Some((*index, *bytes, *wrote_bytes)),
            _ => None,
        })
        .collect();
    assert_eq!(block_dones.len(), report.blocks_total as usize);
    for (i, (idx, bytes, _wrote)) in block_dones.iter().enumerate() {
        assert_eq!(*idx, i as u64);
        assert_eq!(*bytes, BLOCK_SIZE as u64);
    }
    let total_written = block_dones.iter().filter(|(_, _, w)| *w).count() as u64;
    let total_zero = block_dones.iter().filter(|(_, _, w)| !*w).count() as u64;
    assert_eq!(total_written, report.blocks_written);
    assert_eq!(total_zero, report.blocks_zero);
}

#[test]
fn flatten_chain_with_mismatched_block_sizes() {
    // Hyper-V occasionally produces chains where the differencing AVHDX uses
    // a smaller block size than its VHDX parent (e.g. 2 MiB leaf vs 32 MiB
    // parent). Flatten must still produce a correct standalone image.
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx");

    let virtual_size: u64 = 8 * 1024 * 1024;
    let parent_block_size: u32 = 4 * 1024 * 1024;
    let child_block_size: u32 = 1024 * 1024;
    let parent_guid = Uuid::new_v4();

    {
        let mut params = WriterParams::dynamic(virtual_size);
        params.block_size = parent_block_size;
        params.data_write_guid = parent_guid;
        params.file_write_guid = parent_guid;
        params.page_83_data = Uuid::new_v4();
        let mut writer = VhdxWriter::create(&parent, params).unwrap();
        let mut p_block0 = vec![0u8; parent_block_size as usize];
        for (i, b) in p_block0.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut p_block1 = vec![0u8; parent_block_size as usize];
        p_block1[0..6].copy_from_slice(b"PARENT");
        writer.write_block(0, BlockSource::Buf(&p_block0)).unwrap();
        writer.write_block(1, BlockSource::Buf(&p_block1)).unwrap();
        writer.finish().unwrap();
    }

    {
        let pl = ParentLocator::new_vhdx_relative(
            parent.file_name().unwrap().to_str().unwrap(),
            parent_guid,
        );
        let mut params = WriterParams::dynamic(virtual_size);
        params.block_size = child_block_size;
        params.parent_locator = Some(pl);
        let mut writer = VhdxWriter::create(&child, params).unwrap();
        // Overwrite child block index 5 (= virtual offset 5 MiB), which falls
        // inside the parent's 4 MiB block 1.
        let mut c_block = vec![0u8; child_block_size as usize];
        c_block[0..5].copy_from_slice(b"CHILD");
        writer.write_block(5, BlockSource::Buf(&c_block)).unwrap();
        writer.finish().unwrap();
    }

    let report = flatten::flatten(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: None,
        },
    )
    .unwrap();
    assert_eq!(report.blocks_total, 8);

    let (disk, mut file) = VhdxDisk::open(&out, true).unwrap();
    assert!(!disk.has_parent());
    assert_eq!(disk.block_size(), child_block_size);
    assert_eq!(disk.block_count(), 8);

    // Output block 0 (virtual 0..1 MiB) is the first 1 MiB of parent's block 0.
    let r0 = disk.read_block(&mut file, 0).unwrap();
    match r0 {
        BlockRead::Full(b) => {
            assert_eq!(b[0], 0);
            assert_eq!(b[1], 1);
            assert_eq!(b[250], 250);
            assert_eq!(b[251], 0);
        }
        _ => panic!("expected fully present block 0 (slice of parent block 0)"),
    }

    // Output block 4 (virtual 4..5 MiB) is the first 1 MiB of parent's block 1.
    let r4 = disk.read_block(&mut file, 4).unwrap();
    match r4 {
        BlockRead::Full(b) => assert_eq!(&b[0..6], b"PARENT"),
        _ => panic!("expected fully present block 4 (slice of parent block 1)"),
    }

    // Output block 5 (virtual 5..6 MiB) is the child's overwrite.
    let r5 = disk.read_block(&mut file, 5).unwrap();
    match r5 {
        BlockRead::Full(b) => assert_eq!(&b[0..5], b"CHILD"),
        _ => panic!("expected fully present block 5 (from child)"),
    }
}

#[test]
fn merge_child_into_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let report = merge::merge(
        &child,
        MergeOptions {
            keep_child: false,
            skip_fsck: false,
        },
    )
    .unwrap();
    assert!(report.deleted_child);
    assert_eq!(report.parent, parent);
    assert!(!child.exists());

    // Now the parent should have block 1 fully present.
    let (disk, mut file) = VhdxDisk::open(&parent, true).unwrap();
    let r1 = disk.read_block(&mut file, 1).unwrap();
    match r1 {
        BlockRead::Full(b) => assert_eq!(&b[0..5], b"CHILD"),
        _ => panic!("expected fully present block 1 in merged parent"),
    }
    // Block 0 (from original parent) should still be intact.
    let r0 = disk.read_block(&mut file, 0).unwrap();
    match r0 {
        BlockRead::Full(b) => assert_eq!(b[1], 1),
        _ => panic!("expected fully present block 0"),
    }
}

#[test]
fn writer_survives_mid_stream_reopen() {
    // Under `--compress-ntfs` the flatten loop closes and re-opens the
    // file handle after every payload block to force NTFS's on-close
    // maintenance pass. The writer's internal state (payload cursor,
    // BAT entries already populated, file offset for the next write)
    // must survive that close/reopen, otherwise we'd silently overwrite
    // a previously written block or skip ahead.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");

    let mut params = WriterParams::dynamic(VIRTUAL_DISK_SIZE);
    params.block_size = BLOCK_SIZE;
    let guid = Uuid::new_v4();
    params.data_write_guid = guid;
    params.file_write_guid = guid;
    params.page_83_data = Uuid::new_v4();

    let mut block0 = vec![0u8; BLOCK_SIZE as usize];
    for (i, b) in block0.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let mut block2 = vec![0u8; BLOCK_SIZE as usize];
    block2[0..6].copy_from_slice(b"PARENT");
    let mut block3 = vec![0u8; BLOCK_SIZE as usize];
    block3[0..5].copy_from_slice(b"AFTER");

    let mut writer = VhdxWriter::create(&path, params).unwrap();
    writer.set_chunked_write_size(64 * 1024);

    writer.write_block(0, BlockSource::Buf(&block0)).unwrap();
    writer.sync_data().unwrap();
    writer.reopen().unwrap();

    writer.write_block(2, BlockSource::Buf(&block2)).unwrap();
    writer.sync_data().unwrap();
    writer.reopen().unwrap();

    writer.write_block(3, BlockSource::Buf(&block3)).unwrap();
    writer.finish().unwrap();

    let (disk, mut file) = VhdxDisk::open(&path, true).unwrap();
    for (blk, expected_prefix) in [(0u64, &block0[..16]), (2, &block2[..6]), (3, &block3[..5])] {
        match disk.read_block(&mut file, blk).unwrap() {
            BlockRead::Full(b) => {
                assert_eq!(
                    &b[..expected_prefix.len()],
                    expected_prefix,
                    "block {blk} content survived reopen mismatch"
                );
            }
            other => panic!("expected fully present block {blk} after reopen, got {other:?}"),
        }
    }
    // Block 1 was never written and must remain absent (the reopens did
    // not accidentally fill it).
    assert!(matches!(
        disk.read_block(&mut file, 1).unwrap(),
        BlockRead::Absent
    ));
}

#[test]
fn writer_chunked_writes_are_byte_identical() {
    // The chunked-write path used under `--compress-ntfs` splits each block
    // into smaller WriteFile calls + sync_data. The on-disk bytes must end
    // up identical to the single-WriteFile path, otherwise we'd silently
    // corrupt every flatten run.
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain.vhdx");
    let chunked = tmp.path().join("chunked.vhdx");

    let mut params = WriterParams::dynamic(VIRTUAL_DISK_SIZE);
    params.block_size = BLOCK_SIZE;
    let plain_guid = Uuid::new_v4();
    params.data_write_guid = plain_guid;
    params.file_write_guid = plain_guid;
    params.page_83_data = Uuid::new_v4();

    let mut block0 = vec![0u8; BLOCK_SIZE as usize];
    for (i, b) in block0.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let mut block2 = vec![0u8; BLOCK_SIZE as usize];
    block2[0..6].copy_from_slice(b"PARENT");

    {
        let mut writer = VhdxWriter::create(&plain, params.clone()).unwrap();
        writer.write_block(0, BlockSource::Buf(&block0)).unwrap();
        writer.write_block(2, BlockSource::Buf(&block2)).unwrap();
        writer.finish().unwrap();
    }
    {
        let mut writer = VhdxWriter::create(&chunked, params).unwrap();
        // 64 KiB chunk size — much smaller than the 1 MiB block size used in
        // these tests, so every block triggers the chunked code path.
        writer.set_chunked_write_size(64 * 1024);
        writer.write_block(0, BlockSource::Buf(&block0)).unwrap();
        writer.write_block(2, BlockSource::Buf(&block2)).unwrap();
        writer.finish().unwrap();
    }

    // Payload regions must match byte-for-byte. (We don't compare the whole
    // file because headers/BAT carry per-run GUIDs and timestamps.)
    let (disk_p, mut fp) = VhdxDisk::open(&plain, true).unwrap();
    let (disk_c, mut fc) = VhdxDisk::open(&chunked, true).unwrap();
    for blk in [0u64, 2] {
        match (
            disk_p.read_block(&mut fp, blk).unwrap(),
            disk_c.read_block(&mut fc, blk).unwrap(),
        ) {
            (BlockRead::Full(a), BlockRead::Full(b)) => {
                assert_eq!(
                    a, b,
                    "block {blk} mismatch between plain and chunked writers"
                );
            }
            other => panic!("expected fully present blocks, got {other:?}"),
        }
    }
}

#[test]
fn compact_elides_all_zero_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");

    let mut params = WriterParams::dynamic(VIRTUAL_DISK_SIZE);
    params.block_size = BLOCK_SIZE;
    let mut writer = VhdxWriter::create(&path, params).unwrap();
    // Write an all-zero block 0 explicitly (so it shows up as FULLY_PRESENT).
    let zero_block = vec![0u8; BLOCK_SIZE as usize];
    writer
        .write_block(0, BlockSource::Buf(&zero_block))
        .unwrap();
    let mut nonzero_block = vec![0u8; BLOCK_SIZE as usize];
    nonzero_block[42] = 0x5A;
    writer
        .write_block(1, BlockSource::Buf(&nonzero_block))
        .unwrap();
    writer.finish().unwrap();

    let bytes_before = std::fs::metadata(&path).unwrap().len();

    let report = compact::compact(
        &path,
        CompactOptions {
            sparse_size: 4096,
            in_place: true,
            skip_fsck: false,
        },
    )
    .unwrap();
    assert!(
        report.blocks_zero >= 1,
        "expected at least one elided zero block, got {report:?}"
    );
    assert!(
        report.bytes_out < bytes_before,
        "compacted file should be smaller: before={bytes_before}, after={}",
        report.bytes_out
    );
}

#[test]
fn fsck_recovers_torn_secondary_header() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("disk.vhdx");
    create_parent(&path, Uuid::new_v4());

    // Corrupt header 2 by zeroing its first 4 KiB.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(128 * 1024)).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
    }

    // fsck without --fix should still open it via header 1.
    let report = repair::fsck(
        &path,
        FsckOptions {
            fix: false,
            dry_run: true,
            allow_log_discard: false,
        },
    )
    .unwrap();
    assert!(matches!(
        report.header_status,
        repair::HeaderStatus::Header1Only
    ));

    // With --fix, header 2 is rewritten and we end up "BothValid".
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

    // Re-open should now succeed cleanly.
    let (_disk, _f) = VhdxDisk::open(&path, true).unwrap();
}

/// Minimal POSIX tar reader for the round-trip test. We deliberately do
/// **not** pull a tar crate into the dev-dependency set: the round-trip
/// is more meaningful when our own code on both sides is exercised end
/// to end. The reader assumes one regular-file entry followed by the
/// usual two-block zero EOF marker, which is exactly what the streaming
/// flatten path emits.
fn parse_single_file_tar(buf: &[u8]) -> (String, Vec<u8>) {
    assert!(
        buf.len() >= 1024,
        "tar archive must contain at least a header + EOF"
    );
    let header = &buf[..512];

    // ustar magic / version sanity check.
    assert_eq!(&header[257..263], b"ustar\0", "missing ustar magic");
    assert_eq!(&header[263..265], b"00", "wrong ustar version");

    // Filename: NUL-terminated within the first 100 bytes.
    let name_end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
    let name = std::str::from_utf8(&header[..name_end])
        .unwrap()
        .to_string();

    // Size: octal NUL-terminated *or* GNU base-256 (high bit of byte 0
    // set, value in the remaining bytes big-endian).
    let size_field = &header[124..136];
    let size: u64 = if size_field[0] & 0x80 != 0 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&size_field[4..12]);
        u64::from_be_bytes(bytes)
    } else {
        let s_end = size_field.iter().position(|&b| b == 0).unwrap_or(11);
        u64::from_str_radix(std::str::from_utf8(&size_field[..s_end]).unwrap().trim(), 8).unwrap()
    };

    // Verify the header checksum: sum of every byte with the chksum
    // field treated as eight ASCII spaces matches the value stored in
    // the chksum field.
    let stored = u32::from_str_radix(std::str::from_utf8(&header[148..154]).unwrap(), 8).unwrap();
    let mut h = header.to_vec();
    h[148..156].copy_from_slice(b"        ");
    let computed: u32 = h.iter().map(|&b| b as u32).sum();
    assert_eq!(stored, computed, "tar header checksum mismatch");

    let payload = buf[512..512 + size as usize].to_vec();

    // EOF marker: two consecutive zero blocks immediately after the
    // 512-byte-aligned end of the payload.
    let padded = (size + 511) & !511u64;
    let eof_off = 512 + padded as usize;
    assert!(buf.len() >= eof_off + 1024, "tar EOF marker missing");
    assert!(
        buf[eof_off..eof_off + 1024].iter().all(|&b| b == 0),
        "tar EOF marker is not two zero blocks"
    );

    (name, payload)
}

#[test]
fn flatten_zst_round_trips_through_decompression() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let tar_zst_out = tmp.path().join("flat.vhdx.tar.zst");
    let decomp = tmp.path().join("flat.vhdx");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let mut events: Vec<FlattenEvent> = Vec::new();
    let report = flatten::flatten_with_progress(
        &child,
        &tar_zst_out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: Some(ZstdParams::new(3, 0)),
        },
        |ev| events.push(ev),
    )
    .unwrap();

    assert_eq!(report.blocks_total, 4);
    assert_eq!(
        report.compressed_output.as_deref(),
        Some(tar_zst_out.as_path())
    );
    assert!(report.bytes_compressed > 0);
    assert!(report.bytes_uncompressed > report.bytes_compressed);
    assert!(tar_zst_out.exists());

    // The pre-scan phase must emit exactly one ScanStarted, one
    // ScanFinished, and at least one ScanProgress.
    assert!(matches!(
        events.first().unwrap(),
        FlattenEvent::ScanStarted { .. }
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, FlattenEvent::ScanFinished { .. })));
    assert!(matches!(events.last().unwrap(), FlattenEvent::Finished));

    // Decompress, then untar: the result is a plain standalone VHDX.
    let bytes = std::fs::read(&tar_zst_out).unwrap();
    let tar_bytes = zstd::stream::decode_all(&bytes[..]).unwrap();
    let (entry_name, vhdx_bytes) = parse_single_file_tar(&tar_bytes);

    // Inner archive entry must be named after the outer file with the
    // tar.zst suffix stripped — this is what `tar -tvf` will show and
    // what extracts to disk on `tar -xf`.
    assert_eq!(entry_name, "flat.vhdx");

    std::fs::write(&decomp, &vhdx_bytes).unwrap();

    let (disk, mut file) = VhdxDisk::open(&decomp, true).unwrap();
    assert!(!disk.has_parent());
    assert_eq!(disk.virtual_disk_size(), VIRTUAL_DISK_SIZE);
    assert_eq!(disk.block_size(), BLOCK_SIZE);

    let r0 = disk.read_block(&mut file, 0).unwrap();
    match r0 {
        BlockRead::Full(b) => assert_eq!(b[1], 1),
        _ => panic!("expected fully present block 0 (from parent)"),
    }
    let r1 = disk.read_block(&mut file, 1).unwrap();
    match r1 {
        BlockRead::Full(b) => assert_eq!(&b[0..5], b"CHILD"),
        _ => panic!("expected fully present block 1 (from child)"),
    }
    let r2 = disk.read_block(&mut file, 2).unwrap();
    match r2 {
        BlockRead::Full(b) => assert_eq!(&b[0..6], b"PARENT"),
        _ => panic!("expected fully present block 2 (from parent)"),
    }
    // Block 3 was never written → must be zero-elided.
    let r3 = disk.read_block(&mut file, 3).unwrap();
    assert!(matches!(r3, BlockRead::Absent | BlockRead::Full(_)));
}

/// Once decompressed, the leading bytes of the artefact must look like
/// a tar archive to any third-party tool — which means the ustar magic
/// at offset 257 of the *first* 512-byte block. This is a cheap check
/// that the wrapping is in place at the right offset, independently of
/// the round-trip test above.
#[test]
fn flatten_zst_emits_ustar_magic_first() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx.tar.zst");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    flatten::flatten(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: Some(ZstdParams::new(3, 0)),
        },
    )
    .unwrap();

    let bytes = std::fs::read(&out).unwrap();
    let tar_bytes = zstd::stream::decode_all(&bytes[..]).unwrap();
    assert!(
        tar_bytes.len() >= 512,
        "decompressed output too short to be a tar header"
    );
    assert_eq!(
        &tar_bytes[257..263],
        b"ustar\0",
        "missing ustar magic at offset 257; tar/Explorer will not recognise the archive"
    );
}

#[test]
fn flatten_zst_rejects_compress_ntfs() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx.zst");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let err = flatten::flatten(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Dynamic,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: true,
            compress_zstd: Some(ZstdParams::new(3, 0)),
        },
    )
    .unwrap_err();
    assert!(matches!(err, starweld_core::error::Error::Unsupported(_)));
}

#[test]
fn flatten_zst_rejects_fixed_subformat() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent.vhdx");
    let child = tmp.path().join("child.avhdx");
    let out = tmp.path().join("flat.vhdx.zst");

    let parent_guid = Uuid::new_v4();
    create_parent(&parent, parent_guid);
    create_child(&child, &parent, parent_guid);

    let err = flatten::flatten(
        &child,
        &out,
        FlattenOptions {
            subformat: FlattenSubformat::Fixed,
            skip_fsck: false,
            elide_zero_blocks: true,
            compress_ntfs: false,
            compress_zstd: Some(ZstdParams::new(3, 0)),
        },
    )
    .unwrap_err();
    assert!(matches!(err, starweld_core::error::Error::Unsupported(_)));
}
