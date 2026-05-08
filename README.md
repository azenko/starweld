# starweld

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![Version](https://img.shields.io/badge/version-1.0.0-brightgreen.svg)](Cargo.toml)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org/)
[![Unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

A pure-Rust, cross-platform CLI to inspect, repair, merge, flatten and compact
Microsoft VHDX / AVHDX files. No Microsoft components — everything is built
directly against the [MS-VHDX] open specification.

- **Author:** Asuka Zenko `<contact@gungnirnet.eu>`
- **Version:** 1.0.0
- **License:** MIT or Apache-2.0 (your choice)

## Highlights

- `info` — header GUIDs, virtual size, block size, sector sizes, log status,
  parent locator, full parent>child chain.
- `chain` — pretty tree of the differencing chain.
- `fsck` — spec-compliant log replay and structural repair.
- `merge` — leaf AVHDX into its immediate parent (one step).
- `flatten` — collapse an entire chain into a single standalone VHDX,
  optionally streamed straight into a `.vhdx.tar.zst` archive (one-file
  POSIX tar wrapped in zstd; multi-threaded; no intermediate
  uncompressed file ever touches the destination).
- `compact` — qemu-img-style zero-block elision and repack.
- Standalone single-binary on Linux (`x86_64-unknown-linux-musl`) and
  Windows (`x86_64-pc-windows-gnu`).
- `#![forbid(unsafe_code)]` in the engine.

## Build

Native (whatever the host is):

```bash
cargo build --release
./target/release/starweld --help
```

Cross-build the two distribution binaries (Linux musl + Windows MinGW). The
defaults work on a stock Linux box with `mingw-w64` and the `musl` target
installed (`rustup target add x86_64-unknown-linux-musl x86_64-pc-windows-gnu`):

```bash
cargo xtask dist
# Artifacts:
#   target/x86_64-unknown-linux-musl/release/starweld
#   target/x86_64-pc-windows-gnu/release/starweld.exe
```

Pass `--cross` to drive `cross` containers instead of the host toolchain. Both
artifacts are stripped, statically linked and have no runtime dependencies.

## Quick reference

```bash
starweld info disk.vhdx
starweld chain leaf.avhdx
starweld fsck disk.vhdx --fix
starweld merge child.avhdx
starweld flatten leaf.avhdx -o flat.vhdx
starweld flatten leaf.avhdx -o flat.vhdx --zstd               # writes flat.vhdx.tar.zst, no intermediate
starweld flatten leaf.avhdx -o flat.vhdx --zstd --zstd-level 19
starweld flatten leaf.avhdx -o flat.vhdx --zstd --zstd-threads 1
starweld compact disk.vhdx --in-place
```

### Streaming `--zstd` output (`.vhdx.tar.zst`)

When `--zstd` is set, `flatten` writes **only** a single
`<out>.vhdx.tar.zst` file — a one-file POSIX tar archive whose sole
entry is the inner VHDX, the whole thing compressed with
[`zstd`](https://facebook.github.io/zstd/). No intermediate uncompressed
VHDX ever lands on the destination filesystem; every byte of the inner
VHDX flows directly through the encoder, which by default uses every
available logical core.

The tar wrapping is what makes the artefact a real archive in the eyes
of the tools shipped on every modern OS:

| Tool | Command / behaviour |
|------|---------------------|
| GNU `tar`, BSD `tar`, libarchive `bsdtar` | `tar -xf flat.vhdx.tar.zst` extracts `flat.vhdx` directly. |
| Windows 11 Explorer (24H2+) | Right-click → *Extract All*. |
| 7-Zip ≥ 21.07, PeaZip, NanaZip | Recognised as `tar.zst`; double-click extracts the inner VHDX. |
| macOS Finder / `tar` | `tar -xf flat.vhdx.tar.zst` (BSD tar with libarchive zstd). |
| Plain `zstd` | `zstd -d flat.vhdx.tar.zst` yields `flat.vhdx.tar`, then `tar -xf flat.vhdx.tar`. |

A bare `.zst` would be a stream-compression format (the zstd analogue of
`.gz`, **not** an archive), so neither `tar` nor Windows Explorer would
recognise it; the tar wrapping costs ~1.5 KiB of overhead and removes
that papercut entirely.

To make the inner VHDX streamable into an encoder, it uses a slightly
different region layout (the BAT region sits *after* the payload
region; this is allowed by MS-VHDX and accepted by Hyper-V, qemu-img,
and `starweld` itself). A one-time pre-scan of the source chain
detects zero blocks so the inner VHDX still benefits from sparse-block
elision. The trade-off is that the source data is read twice (once for
the scan, once during encoding).

| flag | default | meaning |
|------|---------|---------|
| `--zstd` | off | enable streaming zstd output (always wrapped in a one-file tar) |
| `--zstd-level <N>` | 3 | compression level, 1–22 (matches the `zstd` CLI) |
| `--zstd-threads <N>` | 0 | worker threads; `0` = all logical cores, `1` = single-threaded |

## License

`starweld` is dual-licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option. SPDX identifier: `MIT OR Apache-2.0`.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.

## Credits

Made with love by **Asuka Zenko** `<contact@gungnirnet.eu>` — crafted using
[Cursor](https://cursor.com), the AI-first code editor.

`starweld` is an independent open-source project and is not affiliated with,
endorsed by, or sponsored by Microsoft. "Hyper-V", "VHDX" and "AVHDX" are
trademarks of Microsoft Corporation, used here only for identification.
