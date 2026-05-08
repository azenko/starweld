// Tiny utility used by maintainers to drop a parent+child VHDX chain into
// a directory so the CLI can be exercised manually:
//
//     cargo run -p starweld-core --example make_chain -- /tmp/vhdx-test
//     ./target/release/starweld flatten /tmp/vhdx-test/child.avhdx \
//         -o /tmp/vhdx-test/flat.vhdx --zstd
//
// Not part of the public API; deliberately kept very small.

use starweld_core::parent_locator::ParentLocator;
use starweld_core::writer::{BlockSource, VhdxWriter, WriterParams};
use uuid::Uuid;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: make_chain <dir>");
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let parent = dir.join("parent.vhdx");
    let child = dir.join("child.avhdx");

    let parent_guid = Uuid::new_v4();
    let mut params = WriterParams::dynamic(64 * 1024 * 1024); // 64 MiB virtual disk
    params.block_size = 1024 * 1024; // 1 MiB blocks
    params.data_write_guid = parent_guid;
    params.file_write_guid = parent_guid;
    params.page_83_data = Uuid::new_v4();

    let mut writer = VhdxWriter::create(&parent, params).unwrap();
    let mut filler = vec![0u8; 1024 * 1024];
    for (i, b) in filler.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    // Three non-zero blocks at indices 0, 2, 40; the rest are sparse.
    writer.write_block(0, BlockSource::Buf(&filler)).unwrap();
    writer.write_block(2, BlockSource::Buf(&filler)).unwrap();
    writer.write_block(40, BlockSource::Buf(&filler)).unwrap();
    writer.finish().unwrap();

    let pl = ParentLocator::new_vhdx_relative("parent.vhdx", parent_guid);
    let mut params = WriterParams::dynamic(64 * 1024 * 1024);
    params.block_size = 1024 * 1024;
    params.parent_locator = Some(pl);
    let mut writer = VhdxWriter::create(&child, params).unwrap();
    let mut child_block = vec![0u8; 1024 * 1024];
    child_block[0..5].copy_from_slice(b"CHILD");
    writer
        .write_block(1, BlockSource::Buf(&child_block))
        .unwrap();
    writer.finish().unwrap();

    println!("created chain in {}", dir.display());
    println!("  parent: {}", parent.display());
    println!("  child:  {}", child.display());
}
