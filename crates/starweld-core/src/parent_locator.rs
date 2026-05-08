//! Parent locator metadata item used by AVHDX (differencing) files.

use byteorder::{ByteOrder, LittleEndian};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::format::{
    KEY_ABSOLUTE_WIN32_PATH, KEY_PARENT_LINKAGE, KEY_PARENT_LINKAGE2, KEY_RELATIVE_PATH,
    KEY_VOLUME_PATH, PARENT_LOCATOR_TYPE_VHDX,
};
use crate::guid::{read_guid_at, write_guid_at};

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParentLocator {
    pub locator_type: Uuid,
    pub entries: Vec<(String, String)>,
}

impl ParentLocator {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 20 {
            return Err(Error::InvalidStructure("parent locator too small".into()));
        }
        let locator_type = read_guid_at(buf, 0);
        // 16: reserved (u16) at offset 16, key_value_count at offset 18
        let count = LittleEndian::read_u16(&buf[18..20]) as usize;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let off = 20 + i * 12;
            if off + 12 > buf.len() {
                break;
            }
            let key_off = LittleEndian::read_u32(&buf[off..off + 4]) as usize;
            let val_off = LittleEndian::read_u32(&buf[off + 4..off + 8]) as usize;
            let key_len = LittleEndian::read_u16(&buf[off + 8..off + 10]) as usize;
            let val_len = LittleEndian::read_u16(&buf[off + 10..off + 12]) as usize;
            let key = decode_utf16(buf, key_off, key_len)?;
            let value = decode_utf16(buf, val_off, val_len)?;
            entries.push((key, value));
        }
        Ok(ParentLocator {
            locator_type,
            entries,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let count = self.entries.len();
        let table_size = 20 + count * 12;
        // Encode strings as UTF-16LE (no NUL terminator), 8-byte aligned tails.
        let mut tail: Vec<u8> = Vec::new();
        let mut offsets = Vec::with_capacity(count);
        let table_base = table_size as u32;

        for (k, v) in &self.entries {
            let kb = encode_utf16(k);
            let kbo = table_base + tail.len() as u32;
            tail.extend_from_slice(&kb);
            while tail.len() % 8 != 0 {
                tail.push(0);
            }
            let vb = encode_utf16(v);
            let vbo = table_base + tail.len() as u32;
            tail.extend_from_slice(&vb);
            while tail.len() % 8 != 0 {
                tail.push(0);
            }
            offsets.push((kbo, vbo, kb.len() as u16, vb.len() as u16));
        }

        let mut out = vec![0u8; table_size + tail.len()];
        write_guid_at(&mut out, 0, self.locator_type);
        LittleEndian::write_u16(&mut out[18..20], count as u16);
        for (i, (ko, vo, kl, vl)) in offsets.iter().enumerate() {
            let off = 20 + i * 12;
            LittleEndian::write_u32(&mut out[off..off + 4], *ko);
            LittleEndian::write_u32(&mut out[off + 4..off + 8], *vo);
            LittleEndian::write_u16(&mut out[off + 8..off + 10], *kl);
            LittleEndian::write_u16(&mut out[off + 10..off + 12], *vl);
        }
        out[table_size..table_size + tail.len()].copy_from_slice(&tail);
        out
    }

    pub fn is_vhdx_parent(&self) -> bool {
        self.locator_type == PARENT_LOCATOR_TYPE_VHDX
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn parent_linkage(&self) -> Option<Uuid> {
        let s = self.get(KEY_PARENT_LINKAGE)?;
        Uuid::parse_str(s.trim_start_matches('{').trim_end_matches('}')).ok()
    }

    pub fn parent_linkage2(&self) -> Option<Uuid> {
        let s = self.get(KEY_PARENT_LINKAGE2)?;
        Uuid::parse_str(s.trim_start_matches('{').trim_end_matches('}')).ok()
    }

    pub fn relative_path(&self) -> Option<&str> {
        self.get(KEY_RELATIVE_PATH)
    }
    pub fn volume_path(&self) -> Option<&str> {
        self.get(KEY_VOLUME_PATH)
    }
    pub fn absolute_win32_path(&self) -> Option<&str> {
        self.get(KEY_ABSOLUTE_WIN32_PATH)
    }

    /// Construct a fresh VHDX-typed locator with only the relative-path entry,
    /// recording the parent's data_write_guid.
    pub fn new_vhdx_relative(relative: &str, parent_data_write_guid: Uuid) -> Self {
        let linkage = format!("{{{}}}", parent_data_write_guid);
        ParentLocator {
            locator_type: PARENT_LOCATOR_TYPE_VHDX,
            entries: vec![
                (KEY_PARENT_LINKAGE.into(), linkage),
                (KEY_RELATIVE_PATH.into(), relative.into()),
            ],
        }
    }
}

fn decode_utf16(buf: &[u8], off: usize, len: usize) -> Result<String> {
    if off + len > buf.len() {
        return Err(Error::InvalidStructure(
            "parent locator string out of bounds".into(),
        ));
    }
    if len % 2 != 0 {
        return Err(Error::InvalidStructure(
            "parent locator string has odd length".into(),
        ));
    }
    let mut units = Vec::with_capacity(len / 2);
    for i in 0..(len / 2) {
        units.push(LittleEndian::read_u16(&buf[off + i * 2..off + i * 2 + 2]));
    }
    String::from_utf16(&units)
        .map_err(|_| Error::InvalidStructure("invalid UTF-16 in parent locator".into()))
}

fn encode_utf16(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for u in s.encode_utf16() {
        let mut b = [0u8; 2];
        LittleEndian::write_u16(&mut b, u);
        out.extend_from_slice(&b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let parent = Uuid::new_v4();
        let pl = ParentLocator::new_vhdx_relative(".\\parent.vhdx", parent);
        let buf = pl.encode();
        let back = ParentLocator::parse(&buf).unwrap();
        assert!(back.is_vhdx_parent());
        assert_eq!(back.relative_path(), Some(".\\parent.vhdx"));
        assert_eq!(back.parent_linkage(), Some(parent));
    }
}
