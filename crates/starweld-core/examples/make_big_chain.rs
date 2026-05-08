// Like make_chain, but produces a multi-hundred-MiB chain so the page-cache
// fix is easy to observe with `fincore`. Allocates ~200 MiB of non-zero
// payload split between parent and child.
//
//     cargo run --release -p starweld-core --example make_big_chain -- /tmp/vhdx-cache-test
//     fincore /tmp/vhdx-cache-test/{parent,child}.vhdx
//     # then run the flatten and re-check fincore

use starweld_core::parent_locator::ParentLocator;
use starweld_core::writer::{BlockSource, VhdxWriter, WriterParams};
use uuid::Uuid;

const VIRTUAL_SIZE: u64 = 1024 * 1024 * 1024; // 1 GiB virtual disk
const BLOCK_SIZE: u32 = 32 * 1024 * 1024; // 32 MiB blocks (32 blocks total)

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: make_big_chain <dir>");
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let parent = dir.join("parent.vhdx");
    let child = dir.join("child.avhdx");

    let parent_guid = Uuid::new_v4();
    let mut params = WriterParams::dynamic(VIRTUAL_SIZE);
    params.block_size = BLOCK_SIZE;
    params.data_write_guid = parent_guid;
    params.file_write_guid = parent_guid;
    params.page_83_data = Uuid::new_v4();

    let mut writer = VhdxWriter::create(&parent, params).unwrap();
    let mut filler = vec![0u8; BLOCK_SIZE as usize];
    for (i, b) in filler.iter_mut().enumerate() {
        *b = ((i.wrapping_mul(2654435761)) & 0xff) as u8;
    }
    // Five non-zero parent blocks scattered through the disk = 160 MiB of data.
    for &idx in &[0u64, 5, 10, 20, 30] {
        writer.write_block(idx, BlockSource::Buf(&filler)).unwrap();
    }
    writer.finish().unwrap();

    let pl = ParentLocator::new_vhdx_relative("parent.vhdx", parent_guid);
    let mut params = WriterParams::dynamic(VIRTUAL_SIZE);
    params.block_size = BLOCK_SIZE;
    params.parent_locator = Some(pl);
    let mut writer = VhdxWriter::create(&child, params).unwrap();
    // One child block that overrides position 1 = 32 MiB.
    let mut child_block = vec![1u8; BLOCK_SIZE as usize];
    child_block[0..5].copy_from_slice(b"CHILD");
    writer
        .write_block(1, BlockSource::Buf(&child_block))
        .unwrap();
    writer.finish().unwrap();

    println!("created big chain in {}", dir.display());
    println!(
        "  parent: {} ({} bytes)",
        parent.display(),
        std::fs::metadata(&parent).unwrap().len()
    );
    println!(
        "  child:  {} ({} bytes)",
        child.display(),
        std::fs::metadata(&child).unwrap().len()
    );
}
