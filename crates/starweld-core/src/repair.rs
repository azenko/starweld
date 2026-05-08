//! `fsck`-style integrity check + optional repair.

use std::fs::{File, OpenOptions};
use std::path::Path;

use uuid::Uuid;

use crate::bat::PayloadState;
use crate::error::{Error, Result};
use crate::format::REGION_GUID_BAT;
use crate::header::{verify_file_type, HeaderPair};
use crate::io_util::write_all_at;
use crate::log::{apply_replay, mark_log_clean, scan_log};
use crate::metadata::Metadata;
use crate::region::RegionTablePair;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FsckReport {
    pub path: std::path::PathBuf,
    pub header_status: HeaderStatus,
    pub region_status: RegionStatus,
    pub log_replay: LogStatus,
    pub bat_issues: Vec<String>,
    pub fixed: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum HeaderStatus {
    BothValid,
    Header1Only,
    Header2Only,
    BothInvalid,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RegionStatus {
    BothValid,
    Table1Only,
    Table2Only,
    BothInvalid,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LogStatus {
    Empty,
    Replayed { entries: usize, max_seq: u64 },
    Discarded { entries_seen: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct FsckOptions {
    pub fix: bool,
    pub dry_run: bool,
    /// Allow discarding a corrupt log if it cannot form a valid sequence.
    pub allow_log_discard: bool,
}

impl Default for FsckOptions {
    fn default() -> Self {
        FsckOptions {
            fix: false,
            dry_run: false,
            allow_log_discard: true,
        }
    }
}

pub fn fsck<P: AsRef<Path>>(path: P, opts: FsckOptions) -> Result<FsckReport> {
    let mut report = FsckReport {
        path: path.as_ref().to_path_buf(),
        header_status: HeaderStatus::BothInvalid,
        region_status: RegionStatus::BothInvalid,
        log_replay: LogStatus::Empty,
        bat_issues: Vec::new(),
        fixed: false,
        warnings: Vec::new(),
    };

    let mut file = OpenOptions::new()
        .read(true)
        .write(opts.fix && !opts.dry_run)
        .open(path.as_ref())?;
    verify_file_type(&mut file)?;

    let pair = HeaderPair::read(&mut file)?;
    report.header_status = match (pair.headers[0].is_valid(), pair.headers[1].is_valid()) {
        (true, true) => HeaderStatus::BothValid,
        (true, false) => HeaderStatus::Header1Only,
        (false, true) => HeaderStatus::Header2Only,
        (false, false) => HeaderStatus::BothInvalid,
    };

    let region_pair = RegionTablePair::read(&mut file)?;
    let region = region_pair.active().clone();
    region.validate_layout()?;
    report.region_status = match (
        region_pair.tables[0].is_some(),
        region_pair.tables[1].is_some(),
    ) {
        (true, true) => RegionStatus::BothValid,
        (true, false) => RegionStatus::Table1Only,
        (false, true) => RegionStatus::Table2Only,
        (false, false) => RegionStatus::BothInvalid,
    };

    // Repair the inactive header / mismatched region tables.
    if opts.fix && !opts.dry_run {
        if !matches!(report.header_status, HeaderStatus::BothValid) {
            // Rewrite both headers from the active one (with bumped sequence).
            let active = pair.active();
            HeaderPair::write_new(&mut file, active, |_| {})?;
            HeaderPair::write_new(&mut file, active, |_| {})?;
            report.fixed = true;
        }
        if !matches!(report.region_status, RegionStatus::BothValid) {
            RegionTablePair::write_both(&mut file, &region)?;
            report.fixed = true;
        }
    }

    // Log replay.
    let active = *pair.active();
    if active.log_guid != Uuid::nil() && active.log_length > 0 {
        let scan = scan_log(&mut file, &active)?;
        let count = scan.active_sequence.len();
        if count > 0 {
            if opts.fix && !opts.dry_run {
                apply_replay(&mut file, &scan)?;
                mark_log_clean(&mut file, &pair)?;
                report.log_replay = LogStatus::Replayed {
                    entries: count,
                    max_seq: scan.max_sequence,
                };
                report.fixed = true;
            } else {
                report.warnings.push(format!(
                    "log has {count} replayable entries; rerun with --fix to apply"
                ));
                report.log_replay = LogStatus::Replayed {
                    entries: count,
                    max_seq: scan.max_sequence,
                };
            }
        } else if opts.fix && !opts.dry_run && opts.allow_log_discard {
            report
                .warnings
                .push("log was non-empty but no valid sequence was found; discarding log".into());
            mark_log_clean(&mut file, &pair)?;
            report.log_replay = LogStatus::Discarded { entries_seen: 0 };
            report.fixed = true;
        } else if !opts.fix {
            report.warnings.push(
                "log appears corrupt and cannot be replayed; rerun with --fix to discard it".into(),
            );
        }
    }

    // BAT sanity.
    let bat_region = region.bat().ok_or(Error::MissingRegion("BAT"))?;
    let metadata_region = region.metadata().ok_or(Error::MissingRegion("Metadata"))?;
    let mut metadata_buf = vec![0u8; metadata_region.length as usize];
    crate::io_util::read_exact_at(&mut file, metadata_region.file_offset, &mut metadata_buf)?;
    let metadata = Metadata::parse(&metadata_buf)?;

    let mut bat_buf = vec![0u8; bat_region.length as usize];
    crate::io_util::read_exact_at(&mut file, bat_region.file_offset, &mut bat_buf)?;
    let bat = crate::bat::Bat::parse(
        &bat_buf,
        metadata.block_count(),
        metadata.chunk_ratio(),
        metadata.file_parameters.has_parent,
    );
    let file_len = file.metadata()?.len();
    let bs = metadata.block_size() as u64;
    let mut chunk_payload_count = 0u64;
    let mut payload_index = 0u64;
    for entry in &bat.entries {
        let is_sb = chunk_payload_count == metadata.chunk_ratio();
        if is_sb {
            // sector bitmap entry; skip for payload validation.
            chunk_payload_count = 0;
            continue;
        }
        chunk_payload_count += 1;
        let state = PayloadState::from_raw(entry.state());
        match state {
            PayloadState::FullyPresent | PayloadState::PartiallyPresent => {
                let off = entry.file_offset();
                if off == 0 {
                    report
                        .bat_issues
                        .push(format!("block {payload_index}: state present but offset 0"));
                } else if off + bs > file_len {
                    report.bat_issues.push(format!(
                        "block {payload_index}: payload offset {off:#x}+{bs:#x} exceeds file length {file_len:#x}"
                    ));
                }
            }
            PayloadState::Other(s) => {
                report
                    .bat_issues
                    .push(format!("block {payload_index}: unknown state {s}"));
            }
            _ => {}
        }
        payload_index += 1;
    }
    let _ = REGION_GUID_BAT;
    let _ = file;

    Ok(report)
}

/// Convenience: open a file mutably for the engines below; runs `fsck --fix`
/// first (allowing log discard).
pub fn open_repaired(path: &Path) -> Result<File> {
    let report = fsck(
        path,
        FsckOptions {
            fix: true,
            dry_run: false,
            allow_log_discard: true,
        },
    )?;
    if !report.bat_issues.is_empty() {
        return Err(Error::Integrity(format!(
            "BAT issues found: {}",
            report.bat_issues.join(", ")
        )));
    }
    Ok(OpenOptions::new().read(true).write(true).open(path)?)
}

/// Helper to physically remove the active log if even header-arbitration is
/// wrong; used by aggressive recovery paths.
pub fn force_clean_log(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    verify_file_type(&mut file)?;
    let pair = HeaderPair::read(&mut file)?;
    let active = pair.active();
    let mut new_header = *active;
    new_header.log_guid = Uuid::nil();
    new_header.sequence_number = active.sequence_number.wrapping_add(1);
    let raw = new_header.encode();
    write_all_at(&mut file, crate::format::header_offset(0), &raw)?;
    write_all_at(&mut file, crate::format::header_offset(1), &raw)?;
    file.sync_all()?;
    Ok(())
}
