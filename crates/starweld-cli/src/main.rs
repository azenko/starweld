// starweld — pure-Rust VHDX/AVHDX inspect, repair, merge, flatten and compact.
//
// Author : Asuka Zenko <contact@gungnirnet.eu>
// Version: 1.0.0
// License: MIT OR Apache-2.0
//
// Made with love using Cursor (https://cursor.com).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use starweld_core::chain::Chain;
use starweld_core::compact::{self, CompactOptions};
use starweld_core::disk::VhdxDisk;
use starweld_core::flatten::{self, FlattenEvent, FlattenOptions, FlattenSubformat};
use starweld_core::merge::{self, MergeEvent, MergeOptions};
use starweld_core::repair::{self, FsckOptions};
use starweld_core::zst_compress::ZstdParams;

const LONG_ABOUT: &str = "\
Pure-Rust VHDX/AVHDX inspect, repair, merge, flatten and compact.

Author : Asuka Zenko <contact@gungnirnet.eu>
Version: 1.0.0
License: MIT OR Apache-2.0 (your choice)

Made with love using Cursor.";

#[derive(Debug, Parser)]
#[command(
    name = "starweld",
    author = "Asuka Zenko <contact@gungnirnet.eu>",
    about = "Pure-Rust VHDX/AVHDX inspect, repair, merge, flatten and compact.",
    long_about = LONG_ABOUT,
    version
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,

    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show information about a VHDX/AVHDX file (and its parent chain).
    Info {
        path: PathBuf,
        #[arg(long)]
        no_follow: bool,
    },

    /// Print the parent>child chain.
    Chain { path: PathBuf },

    /// Verify (and optionally repair) a VHDX/AVHDX file.
    Fsck {
        path: PathBuf,
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_log_discard: bool,
    },

    /// Merge a leaf AVHDX into its immediate parent (one chain step).
    Merge {
        child: PathBuf,
        #[arg(long)]
        keep: bool,
        #[arg(long)]
        no_fsck: bool,
        /// Disable the interactive progress bar. Implied by `--json` and
        /// when stderr is not a terminal.
        #[arg(long)]
        no_progress: bool,
    },

    /// Collapse an entire chain into a single standalone VHDX.
    Flatten {
        leaf: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long, value_enum, default_value_t = SubformatArg::Dynamic)]
        ty: SubformatArg,
        #[arg(long)]
        no_fsck: bool,
        #[arg(long)]
        keep_zero_blocks: bool,
        /// Enable transparent NTFS LZNT1 compression on the output file
        /// before writing payload blocks, and wait for the filesystem to
        /// finish compressing each block before queuing the next one
        /// (FlushFileBuffers per block, so the OS write cache stays
        /// bounded on multi-GiB flattens). Windows-only and requires the
        /// destination to live on an NTFS volume; the command fails with
        /// a clear error otherwise.
        #[arg(long)]
        compress_ntfs: bool,
        /// Stream the flatten output into a one-file POSIX tar archive
        /// compressed with zstd, producing a single `<out>.tar.zst`
        /// file. The artefact is recognised as an archive by `tar`,
        /// libarchive, Windows Explorer (24H2+), 7-Zip and PeaZip — so
        /// extracting the inner VHDX is a one-step operation with the
        /// tools shipped on every modern OS. No intermediate
        /// uncompressed VHDX is ever written to the destination
        /// filesystem; everything goes directly through the zstd
        /// encoder. Requires a one-time pre-scan of the source chain to
        /// keep the inner VHDX sparse, so the source data is read twice
        /// (once for analysis, once for compression). Incompatible with
        /// `--compress-ntfs` and with `--ty fixed`.
        #[arg(long)]
        zstd: bool,
        /// zstd compression level. 1 is fastest and largest, 22 is
        /// slowest and smallest. Default 3 matches the `zstd` CLI.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(i32).range(1..=22))]
        zstd_level: i32,
        /// Number of zstd worker threads. `0` (the default) auto-detects
        /// the host's logical CPU count; pass `1` to keep the encoder
        /// single-threaded.
        #[arg(long, default_value_t = 0)]
        zstd_threads: u32,
        /// Disable the interactive progress bar. Implied by `--json` and
        /// when stderr is not a terminal.
        #[arg(long)]
        no_progress: bool,
    },

    /// qemu-img-style compact (zero-block elision and repack).
    Compact {
        path: PathBuf,
        #[arg(short = 'S', long, default_value_t = 4096)]
        sparse_size: u64,
        #[arg(long)]
        in_place: bool,
        #[arg(long)]
        no_fsck: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SubformatArg {
    Dynamic,
    Fixed,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Cmd::Info { path, no_follow } => cmd_info(&path, no_follow, cli.json),
        Cmd::Chain { path } => cmd_chain(&path, cli.json),
        Cmd::Fsck {
            path,
            fix,
            dry_run,
            no_log_discard,
        } => cmd_fsck(&path, fix, dry_run, !no_log_discard, cli.json),
        Cmd::Merge {
            child,
            keep,
            no_fsck,
            no_progress,
        } => cmd_merge(&child, keep, no_fsck, no_progress, cli.json),
        Cmd::Flatten {
            leaf,
            out,
            ty,
            no_fsck,
            keep_zero_blocks,
            compress_ntfs,
            zstd,
            zstd_level,
            zstd_threads,
            no_progress,
        } => cmd_flatten(
            &leaf,
            &out,
            ty,
            no_fsck,
            keep_zero_blocks,
            compress_ntfs,
            zstd,
            zstd_level,
            zstd_threads,
            no_progress,
            cli.json,
        ),
        Cmd::Compact {
            path,
            sparse_size,
            in_place,
            no_fsck,
        } => cmd_compact(&path, sparse_size, in_place, no_fsck, cli.json),
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .try_init();
}

fn cmd_info(path: &std::path::Path, no_follow: bool, json: bool) -> Result<()> {
    let (disk, _f) = VhdxDisk::open(path, true).context("opening VHDX")?;
    let chain = if no_follow {
        None
    } else {
        Some(Chain::discover(path).context("walking parent chain")?)
    };

    if json {
        let info = serde_json::json!({
            "path": path,
            "header": {
                "data_write_guid": disk.active_header().data_write_guid,
                "file_write_guid": disk.active_header().file_write_guid,
                "log_guid": disk.active_header().log_guid,
                "sequence_number": disk.active_header().sequence_number,
                "version": disk.active_header().version,
                "log_offset": disk.active_header().log_offset,
                "log_length": disk.active_header().log_length,
            },
            "metadata": {
                "virtual_disk_size": disk.virtual_disk_size(),
                "block_size": disk.block_size(),
                "block_count": disk.block_count(),
                "logical_sector_size": disk.metadata.logical_sector_size,
                "physical_sector_size": disk.metadata.physical_sector_size,
                "page_83_data": disk.metadata.page_83_data,
                "has_parent": disk.has_parent(),
            },
            "chain": chain,
        });
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!(
            "starweld {} by {} ({})",
            starweld_core::VERSION,
            starweld_core::AUTHOR,
            starweld_core::TAGLINE
        );
        println!();
        println!("file:                {}", path.display());
        println!(
            "virtual disk size:   {} bytes ({:.2} GiB)",
            disk.virtual_disk_size(),
            disk.virtual_disk_size() as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        println!("block size:          {} bytes", disk.block_size());
        println!("block count:         {}", disk.block_count());
        println!("logical sector size: {}", disk.metadata.logical_sector_size);
        println!(
            "physical sector size:{}",
            disk.metadata.physical_sector_size
        );
        println!("page 83 data:        {}", disk.metadata.page_83_data);
        println!("has parent:          {}", disk.has_parent());
        let h = disk.active_header();
        println!("data write guid:     {}", h.data_write_guid);
        println!("file write guid:     {}", h.file_write_guid);
        println!("log guid:            {}", h.log_guid);
        println!("sequence number:     {}", h.sequence_number);
        println!(
            "log offset/length:   {:#x} / {} bytes",
            h.log_offset, h.log_length
        );
        if let Some(c) = chain {
            println!("\nchain:");
            print!("{}", c.pretty());
        }
    }
    Ok(())
}

fn cmd_chain(path: &std::path::Path, json: bool) -> Result<()> {
    let chain = Chain::discover(path).context("walking parent chain")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&chain)?);
    } else {
        print!("{}", chain.pretty());
    }
    Ok(())
}

fn cmd_fsck(
    path: &std::path::Path,
    fix: bool,
    dry_run: bool,
    allow_log_discard: bool,
    json: bool,
) -> Result<()> {
    let report = repair::fsck(
        path,
        FsckOptions {
            fix,
            dry_run,
            allow_log_discard,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{report:#?}");
    }
    Ok(())
}

fn cmd_merge(
    child: &std::path::Path,
    keep: bool,
    no_fsck: bool,
    no_progress: bool,
    json: bool,
) -> Result<()> {
    // Progress is disabled in JSON mode (we don't want bar frames mixing
    // into machine-readable output) and when the user opts out
    // explicitly. In every other case we rely on indicatif to detect a
    // non-TTY stderr and silently no-op there.
    let progress = ProgressGuard::new(!no_progress && !json);

    let report = merge::merge_with_progress(
        child,
        MergeOptions {
            keep_child: keep,
            skip_fsck: no_fsck,
        },
        |ev| match ev {
            MergeEvent::Started {
                total_blocks,
                total_bytes,
                ..
            } => {
                progress.start_merge(total_blocks, total_bytes);
            }
            MergeEvent::BlockDone {
                bytes, wrote_bytes, ..
            } => {
                progress.advance_merge(bytes, wrote_bytes);
            }
            MergeEvent::Finished => {
                progress.finish();
            }
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "merged {} -> {} ({} blocks, {} partial sectors, child {})",
            report.child.display(),
            report.parent.display(),
            report.blocks_copied,
            report.partial_sectors_copied,
            if report.deleted_child {
                "deleted"
            } else {
                "kept"
            }
        );
    }
    Ok(())
}

// `cmd_flatten` has many arguments because it directly mirrors the union of
// every flatten-related CLI flag exposed by `Cmd::Flatten`. Bundling these
// into a struct buys nothing — the call site already names them, and the
// extra indirection just adds visual noise.
#[allow(clippy::too_many_arguments)]
fn cmd_flatten(
    leaf: &std::path::Path,
    out: &std::path::Path,
    ty: SubformatArg,
    no_fsck: bool,
    keep_zero_blocks: bool,
    compress_ntfs: bool,
    zstd: bool,
    zstd_level: i32,
    zstd_threads: u32,
    no_progress: bool,
    json: bool,
) -> Result<()> {
    // Resolve the final destination. When --zstd is set we wrap the
    // inner VHDX in a one-file POSIX tar archive *before* zstd
    // compression — that way the resulting artefact is recognised as an
    // archive by `tar`, libarchive, Windows Explorer (24H2+), 7-Zip,
    // PeaZip and friends instead of looking like raw bytes to them.
    //
    // To give the user a predictable mental model — "the path I pass is
    // the path that ends up on disk" — we transparently rewrite their
    // --out so it always ends in `.tar.zst` when --zstd is set:
    //
    //   foo.vhdx          -> foo.vhdx.tar.zst   (most common case)
    //   foo.vhdx.tar.zst  -> foo.vhdx.tar.zst   (already correct)
    //   foo.vhdx.zst      -> foo.vhdx.tar.zst   (replace bare .zst)
    //   foo.tar           -> foo.tar.zst        (tar but uncompressed)
    //   foo               -> foo.tar.zst        (no extension at all)
    let final_out: std::path::PathBuf = if zstd {
        rewrite_to_tar_zst(out)
    } else {
        out.to_path_buf()
    };

    // 0 = auto = "use every logical core". 1 still spins up one worker
    // thread (separate from the application thread) which is what the
    // user typically wants when they say "single-threaded zstd"; that
    // matches the behaviour of the `zstd` CLI's `-T1`.
    let resolved_threads: u32 = if zstd_threads == 0 {
        num_cpus::get() as u32
    } else {
        zstd_threads
    };

    let opts = FlattenOptions {
        subformat: match ty {
            SubformatArg::Dynamic => FlattenSubformat::Dynamic,
            SubformatArg::Fixed => FlattenSubformat::Fixed,
        },
        skip_fsck: no_fsck,
        elide_zero_blocks: !keep_zero_blocks,
        compress_ntfs,
        compress_zstd: zstd.then(|| ZstdParams::new(zstd_level, resolved_threads)),
    };

    // Progress is disabled in JSON mode (we don't want bar frames mixing into
    // machine-readable output) and when the user opts out explicitly. In
    // every other case we rely on indicatif to detect a non-TTY stderr and
    // silently no-op there.
    let progress = ProgressGuard::new(!no_progress && !json);

    let report = flatten::flatten_with_progress(leaf, &final_out, opts, |ev| match ev {
        FlattenEvent::ScanStarted { total_bytes } => {
            progress.start_scan(total_bytes);
        }
        FlattenEvent::ScanProgress { bytes_scanned } => {
            progress.set_scan_position(bytes_scanned);
        }
        FlattenEvent::ScanFinished { .. } => {
            // The bar is reset by the next Started event, which carries the
            // total payload size; nothing to do here other than mark the
            // scan phase as logically done.
        }
        FlattenEvent::Started {
            total_blocks,
            block_size,
            total_bytes,
        } => {
            // On the zstd path Started fires *after* ScanFinished and bounds
            // the bar to the *full* virtual size — that way "wrote bytes"
            // and "zero block" advances both contribute to the same
            // monotone bar, which is what the user expects for "we are
            // processing the chain".
            progress.start_write(
                total_blocks,
                block_size,
                total_bytes,
                zstd,
                zstd_level,
                resolved_threads,
            );
        }
        FlattenEvent::BlockDone {
            bytes, wrote_bytes, ..
        } => {
            progress.advance(bytes, wrote_bytes);
        }
        FlattenEvent::Finished => {
            progress.finish();
        }
    })?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if zstd {
        let ratio = if report.bytes_uncompressed > 0 {
            (report.bytes_compressed as f64 / report.bytes_uncompressed as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "flattened {} -> {} ({} blocks: {} written, {} zero) [zstd L{} x{}, inner {:.2} GiB -> .tar.zst {:.2} GiB, {:.1}%]",
            report.leaf.display(),
            report
                .compressed_output
                .as_deref()
                .unwrap_or(report.output.as_path())
                .display(),
            report.blocks_total,
            report.blocks_written,
            report.blocks_zero,
            zstd_level,
            resolved_threads,
            report.bytes_uncompressed as f64 / (1024.0 * 1024.0 * 1024.0),
            report.bytes_compressed as f64 / (1024.0 * 1024.0 * 1024.0),
            ratio,
        );
    } else {
        println!(
            "flattened {} -> {} ({} blocks: {} written, {} zero){}",
            report.leaf.display(),
            report.output.display(),
            report.blocks_total,
            report.blocks_written,
            report.blocks_zero,
            if compress_ntfs {
                " [ntfs-compressed]"
            } else {
                ""
            },
        );
    }
    Ok(())
}

/// Rewrite an arbitrary user-provided output path so it always ends in
/// `.tar.zst`. Idempotent: a path that already ends in `.tar.zst`
/// (case-insensitively) is returned unchanged. See the call site for the
/// full table of cases this is intended to cover.
fn rewrite_to_tar_zst(p: &std::path::Path) -> std::path::PathBuf {
    let raw = p.as_os_str().to_string_lossy().into_owned();
    let lower = raw.to_ascii_lowercase();

    if lower.ends_with(".tar.zst") {
        return p.to_path_buf();
    }
    if lower.ends_with(".zst") {
        // Drop the bare `.zst`; the result might already end in `.tar`,
        // in which case we just add `.zst` back below.
        let trimmed = &raw[..raw.len() - ".zst".len()];
        let lower_trim = trimmed.to_ascii_lowercase();
        if lower_trim.ends_with(".tar") {
            return std::path::PathBuf::from(format!("{trimmed}.zst"));
        }
        return std::path::PathBuf::from(format!("{trimmed}.tar.zst"));
    }
    if lower.ends_with(".tar") {
        return std::path::PathBuf::from(format!("{raw}.zst"));
    }
    std::path::PathBuf::from(format!("{raw}.tar.zst"))
}

/// RAII wrapper around an [`indicatif::ProgressBar`] that always clears the
/// bar on drop, including on the error path of `flatten_with_progress`.
struct ProgressGuard {
    bar: ProgressBar,
    enabled: bool,
}

impl ProgressGuard {
    fn new(enabled: bool) -> Self {
        let bar = if enabled {
            let bar = ProgressBar::new(0);
            bar.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {wide_bar:.cyan/blue} \
                     {bytes:>10}/{total_bytes:<10} ({bytes_per_sec}, ETA {eta}) {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
            );
            bar
        } else {
            let bar = ProgressBar::with_draw_target(Some(0), ProgressDrawTarget::hidden());
            bar.set_style(ProgressStyle::default_bar());
            bar
        };
        ProgressGuard { bar, enabled }
    }

    /// Begin the "scanning chain" phase used by the streaming-zstd path.
    /// `total_bytes` is the virtual disk size; the bar advances as the
    /// pre-scan walks the chain.
    fn start_scan(&self, total_bytes: u64) {
        if !self.enabled {
            return;
        }
        self.bar.set_length(total_bytes);
        self.bar.set_position(0);
        self.bar.set_message("scanning chain for zero blocks");
        self.bar.tick();
    }

    /// Set the absolute position of the scan-phase bar. Called once per
    /// pre-scanned block with the running total of bytes processed so far.
    fn set_scan_position(&self, bytes_scanned: u64) {
        if !self.enabled {
            return;
        }
        self.bar.set_position(bytes_scanned);
    }

    /// Begin the "writing" phase. On the non-zstd path this is the only
    /// phase; on the zstd path it follows the scan phase and the bar is
    /// reset for it.
    fn start_write(
        &self,
        total_blocks: u64,
        _block_size: u32,
        total_bytes: u64,
        zstd: bool,
        zstd_level: i32,
        zstd_threads: u32,
    ) {
        if !self.enabled {
            return;
        }
        self.bar.set_length(total_bytes);
        self.bar.set_position(0);
        if zstd {
            self.bar.set_message(format!(
                "flattening + zstd L{zstd_level} x{zstd_threads} ({total_blocks} blocks)"
            ));
        } else {
            self.bar
                .set_message(format!("flattening {total_blocks} blocks"));
        }
        self.bar.tick();
    }

    fn advance(&self, bytes: u64, wrote_bytes: bool) {
        if !self.enabled {
            return;
        }
        self.bar.inc(bytes);
        if !wrote_bytes {
            // Cheap, non-allocating cue that the last block was sparse-elided
            // — useful on long zero runs at the head/tail of an image.
            self.bar.set_message("flattening (sparse)");
        } else {
            self.bar.set_message("flattening");
        }
    }

    /// Begin the single-phase progress bar used by `merge`. The bar is
    /// bounded by the child's full virtual size; advances driven by
    /// [`Self::advance_merge`] cover both copied blocks (where bytes
    /// landed in the parent) and absent blocks (where nothing changed).
    fn start_merge(&self, total_blocks: u64, total_bytes: u64) {
        if !self.enabled {
            return;
        }
        self.bar.set_length(total_bytes);
        self.bar.set_position(0);
        self.bar
            .set_message(format!("merging {total_blocks} blocks"));
        self.bar.tick();
    }

    fn advance_merge(&self, bytes: u64, wrote_bytes: bool) {
        if !self.enabled {
            return;
        }
        self.bar.inc(bytes);
        if !wrote_bytes {
            // The child block was Absent; nothing was copied for it.
            self.bar.set_message("merging (absent)");
        } else {
            self.bar.set_message("merging");
        }
    }

    fn finish(&self) {
        if !self.enabled {
            return;
        }
        self.bar.finish_and_clear();
    }
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

fn cmd_compact(
    path: &std::path::Path,
    sparse_size: u64,
    in_place: bool,
    no_fsck: bool,
    json: bool,
) -> Result<()> {
    let report = compact::compact(
        path,
        CompactOptions {
            sparse_size,
            in_place,
            skip_fsck: no_fsck,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "compacted {} -> {} ({} blocks: {} written, {} zero) {} -> {} bytes",
            report.input.display(),
            report.output.display(),
            report.blocks_total,
            report.blocks_written,
            report.blocks_zero,
            report.bytes_in,
            report.bytes_out
        );
    }
    Ok(())
}

#[cfg(test)]
mod rewrite_tests {
    use super::rewrite_to_tar_zst;
    use std::path::{Path, PathBuf};

    fn rw(s: &str) -> String {
        rewrite_to_tar_zst(Path::new(s))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn appends_when_no_extension() {
        assert_eq!(rw("backup"), "backup.tar.zst");
    }

    #[test]
    fn appends_to_plain_vhdx() {
        assert_eq!(rw("flat.vhdx"), "flat.vhdx.tar.zst");
    }

    #[test]
    fn replaces_bare_zst() {
        assert_eq!(rw("flat.vhdx.zst"), "flat.vhdx.tar.zst");
    }

    #[test]
    fn idempotent_on_tar_zst() {
        assert_eq!(rw("flat.vhdx.tar.zst"), "flat.vhdx.tar.zst");
    }

    #[test]
    fn extends_tar_to_tar_zst() {
        assert_eq!(rw("flat.tar"), "flat.tar.zst");
    }

    #[test]
    fn case_insensitive_tar_zst_check() {
        // Already-correct path keeps its casing; we only normalise the
        // "should we append?" decision case-insensitively.
        let got: PathBuf = rewrite_to_tar_zst(Path::new("Foo.Vhdx.TAR.ZST"));
        assert_eq!(got, PathBuf::from("Foo.Vhdx.TAR.ZST"));
    }

    #[test]
    fn handles_dotted_basename() {
        // A leading dot in the filename is just part of the name, not
        // an extension, so the rewrite still appends `.tar.zst`.
        assert_eq!(rw(".hidden"), ".hidden.tar.zst");
    }
}
