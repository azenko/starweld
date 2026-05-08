//! Tiny example: creates a small VHDX at the path passed as arg[1].
//! Used for ad-hoc interop checks against `qemu-img info`.

use starweld_core::writer::{BlockSource, VhdxWriter, WriterParams};

fn main() {
    let path = std::env::args().nth(1).expect("usage: mkvhdx <path.vhdx>");

    let mut params = WriterParams::dynamic(8 * 1024 * 1024);
    params.block_size = 1024 * 1024;
    let mut w = VhdxWriter::create(&path, params).unwrap();

    let mut blk = vec![0u8; 1024 * 1024];
    blk[0..5].copy_from_slice(b"HELLO");
    w.write_block(0, BlockSource::Buf(&blk)).unwrap();
    w.finish().unwrap();
    eprintln!("wrote {path}");
}
