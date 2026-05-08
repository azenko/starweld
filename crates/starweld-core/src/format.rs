//! On-disk VHDX format constants. Values come from [MS-VHDX].

use uuid::{uuid, Uuid};

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;

pub const FILE_TYPE_IDENTIFIER_OFFSET: u64 = 0;
pub const FILE_TYPE_IDENTIFIER_SIZE: u64 = 64 * KIB;

pub const HEADER_1_OFFSET: u64 = 64 * KIB;
pub const HEADER_2_OFFSET: u64 = 128 * KIB;
pub const HEADER_SIZE: u64 = 64 * KIB;

pub const REGION_TABLE_1_OFFSET: u64 = 192 * KIB;
pub const REGION_TABLE_2_OFFSET: u64 = 256 * KIB;
pub const REGION_TABLE_SIZE: u64 = 64 * KIB;

pub const HEADER_SECTION_END: u64 = MIB;

pub const FILE_TYPE_SIGNATURE: &[u8; 8] = b"vhdxfile";
pub const HEADER_SIGNATURE: &[u8; 4] = b"head";
pub const REGION_SIGNATURE: &[u8; 4] = b"regi";
pub const METADATA_SIGNATURE: &[u8; 8] = b"metadata";
pub const LOG_ENTRY_SIGNATURE: &[u8; 4] = b"loge";
pub const ZERO_DESCRIPTOR_SIGNATURE: &[u8; 4] = b"zero";
pub const DATA_DESCRIPTOR_SIGNATURE: &[u8; 4] = b"desc";
pub const DATA_SECTOR_SIGNATURE: &[u8; 4] = b"data";

pub const LOG_SECTOR_SIZE: u64 = 4 * KIB;

pub const REGION_TABLE_HEADER_SIZE: usize = 16;
pub const REGION_TABLE_ENTRY_SIZE: usize = 32;
pub const REGION_TABLE_MAX_ENTRIES: usize = 2047;

pub const HEADER_FIXED_SIZE: usize = 4096;

pub const METADATA_HEADER_SIZE: usize = 32;
pub const METADATA_ENTRY_SIZE: usize = 32;

pub const PAYLOAD_BLOCK_NOT_PRESENT: u8 = 0;
pub const PAYLOAD_BLOCK_UNDEFINED: u8 = 1;
pub const PAYLOAD_BLOCK_ZERO: u8 = 2;
pub const PAYLOAD_BLOCK_UNMAPPED: u8 = 3;
pub const PAYLOAD_BLOCK_FULLY_PRESENT: u8 = 6;
pub const PAYLOAD_BLOCK_PARTIALLY_PRESENT: u8 = 7;

pub const SB_BLOCK_NOT_PRESENT: u8 = 0;
pub const SB_BLOCK_PRESENT: u8 = 6;

pub const REGION_GUID_BAT: Uuid = uuid!("2DC27766-F623-4200-9D64-115E9BFD4A08");
pub const REGION_GUID_METADATA: Uuid = uuid!("8B7CA206-4790-4B9A-B8FE-575F050F886E");

pub const METADATA_GUID_FILE_PARAMETERS: Uuid = uuid!("CAA16737-FA36-4D43-B3B6-33F0AA44E76B");
pub const METADATA_GUID_VIRTUAL_DISK_SIZE: Uuid = uuid!("2FA54224-CD1B-4876-B211-5DBED83BF4B8");
pub const METADATA_GUID_PAGE_83_DATA: Uuid = uuid!("BECA12AB-B2E6-4523-93EF-C309E000C746");
pub const METADATA_GUID_LOGICAL_SECTOR_SIZE: Uuid = uuid!("8141BF1D-A96F-4709-BA47-F233A8FAAB5F");
pub const METADATA_GUID_PHYSICAL_SECTOR_SIZE: Uuid = uuid!("CDA348C7-445D-4471-9CC9-E9885251C556");
pub const METADATA_GUID_PARENT_LOCATOR: Uuid = uuid!("A8D35F2D-B30B-454D-ABF7-D3D84834AB0C");

pub const PARENT_LOCATOR_TYPE_VHDX: Uuid = uuid!("B04AEFB7-D19E-4A81-B789-25B8E9445913");

pub const KEY_PARENT_LINKAGE: &str = "parent_linkage";
pub const KEY_PARENT_LINKAGE2: &str = "parent_linkage2";
pub const KEY_RELATIVE_PATH: &str = "relative_path";
pub const KEY_VOLUME_PATH: &str = "volume_path";
pub const KEY_ABSOLUTE_WIN32_PATH: &str = "absolute_win32_path";

pub const FILE_PARAMETERS_FLAG_LEAVE_BLOCKS_ALLOCATED: u32 = 0x0000_0001;
pub const FILE_PARAMETERS_FLAG_HAS_PARENT: u32 = 0x0000_0002;

pub const REGION_FLAG_REQUIRED: u32 = 0x0000_0001;

pub const METADATA_FLAG_IS_USER: u32 = 0x0000_0001;
pub const METADATA_FLAG_IS_VIRTUAL_DISK: u32 = 0x0000_0002;
pub const METADATA_FLAG_IS_REQUIRED: u32 = 0x0000_0004;

pub const LOG_FLAG_IS_LAST: u32 = 0x0000_0001;

pub const fn header_offset(index: usize) -> u64 {
    match index {
        0 => HEADER_1_OFFSET,
        1 => HEADER_2_OFFSET,
        _ => panic!("invalid header index"),
    }
}

pub const fn region_table_offset(index: usize) -> u64 {
    match index {
        0 => REGION_TABLE_1_OFFSET,
        1 => REGION_TABLE_2_OFFSET,
        _ => panic!("invalid region table index"),
    }
}
