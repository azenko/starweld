//! Walk a parent>child VHDX/AVHDX chain.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::disk::VhdxDisk;
use crate::error::{Error, Result};
use crate::format::PARENT_LOCATOR_TYPE_VHDX;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainNode {
    pub path: PathBuf,
    pub data_write_guid: Uuid,
    pub file_write_guid: Uuid,
    pub virtual_disk_size: u64,
    pub block_size: u32,
    pub has_parent: bool,
    pub parent_link: Option<ParentLink>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParentLink {
    pub locator_type: Uuid,
    pub relative_path: Option<String>,
    pub volume_path: Option<String>,
    pub absolute_win32_path: Option<String>,
    pub expected_data_write_guid: Option<Uuid>,
    pub resolved_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Chain {
    pub leaf_path: PathBuf,
    pub nodes: Vec<ChainNode>,
}

impl Chain {
    pub fn discover<P: AsRef<Path>>(leaf: P) -> Result<Self> {
        let leaf_path = leaf.as_ref().to_path_buf();
        let mut nodes = Vec::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut current = leaf_path.clone();

        loop {
            let canonical = match std::fs::canonicalize(&current) {
                Ok(p) => p,
                Err(_) => current.clone(),
            };
            if !visited.insert(canonical.clone()) {
                return Err(Error::InvalidStructure(format!(
                    "cycle detected in parent chain at {}",
                    canonical.display()
                )));
            }

            let (disk, _file) = VhdxDisk::open(&current, true)?;
            let active = disk.active_header();
            let mut node = ChainNode {
                path: current.clone(),
                data_write_guid: active.data_write_guid,
                file_write_guid: active.file_write_guid,
                virtual_disk_size: disk.virtual_disk_size(),
                block_size: disk.block_size(),
                has_parent: disk.has_parent(),
                parent_link: None,
            };

            if disk.has_parent() {
                let pl = disk.metadata.parent_locator.as_ref().ok_or_else(|| {
                    Error::InvalidStructure("HasParent set but no parent locator".into())
                })?;
                let link = ParentLink {
                    locator_type: pl.locator_type,
                    relative_path: pl.relative_path().map(str::to_string),
                    volume_path: pl.volume_path().map(str::to_string),
                    absolute_win32_path: pl.absolute_win32_path().map(str::to_string),
                    expected_data_write_guid: pl.parent_linkage(),
                    resolved_path: resolve_parent(&current, pl),
                };
                node.parent_link = Some(link.clone());
                nodes.push(node);

                let next = link.resolved_path.ok_or_else(|| Error::ParentNotFound {
                    child: current.clone(),
                    reason: "no candidate path resolved on this filesystem".into(),
                })?;
                current = next;
            } else {
                nodes.push(node);
                break;
            }
        }

        // Verify each parent's DataWriteGuid matches the child's expectation.
        for i in 0..nodes.len() {
            if let Some(link) = &nodes[i].parent_link {
                if let Some(expected) = link.expected_data_write_guid {
                    let parent_guid = nodes
                        .get(i + 1)
                        .map(|n| n.data_write_guid)
                        .unwrap_or(Uuid::nil());
                    if parent_guid != expected && parent_guid != Uuid::nil() {
                        return Err(Error::ParentGuidMismatch {
                            child: nodes[i].path.clone(),
                            expected,
                            actual: parent_guid,
                        });
                    }
                }
            }
        }

        Ok(Chain { leaf_path, nodes })
    }

    pub fn leaf(&self) -> &ChainNode {
        &self.nodes[0]
    }

    pub fn root(&self) -> &ChainNode {
        self.nodes.last().unwrap()
    }

    pub fn pretty(&self) -> String {
        let mut out = String::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let prefix = if i == 0 {
                "leaf".to_string()
            } else if i == self.nodes.len() - 1 {
                "root".to_string()
            } else {
                format!("link {i}")
            };
            out.push_str(&format!("{prefix:<6} {}\n", n.path.display()));
            out.push_str(&format!("       data_write_guid = {}\n", n.data_write_guid));
            out.push_str(&format!(
                "       virtual_size    = {} bytes ({:.2} GiB)\n",
                n.virtual_disk_size,
                n.virtual_disk_size as f64 / (1024.0 * 1024.0 * 1024.0)
            ));
            if let Some(link) = &n.parent_link {
                out.push_str("       parent\n");
                if let Some(rp) = &link.relative_path {
                    out.push_str(&format!("         relative_path       = {}\n", rp));
                }
                if let Some(vp) = &link.volume_path {
                    out.push_str(&format!("         volume_path         = {}\n", vp));
                }
                if let Some(ap) = &link.absolute_win32_path {
                    out.push_str(&format!("         absolute_win32_path = {}\n", ap));
                }
                if let Some(rp) = &link.resolved_path {
                    out.push_str(&format!(
                        "         resolved            = {}\n",
                        rp.display()
                    ));
                }
            }
        }
        out
    }
}

/// Resolve a parent locator to a path that exists on this filesystem.
/// Resolution order per [MS-VHDX]: relative_path > volume_path >
/// absolute_win32_path. We translate Windows-style separators to native ones.
pub fn resolve_parent(child: &Path, pl: &crate::parent_locator::ParentLocator) -> Option<PathBuf> {
    if pl.locator_type != PARENT_LOCATOR_TYPE_VHDX {
        return None;
    }
    let child_dir = child.parent().unwrap_or(Path::new("."));

    let candidates = [
        pl.relative_path(),
        pl.volume_path(),
        pl.absolute_win32_path(),
    ];
    for c in candidates.iter().flatten() {
        let normalized = c.replace('\\', std::path::MAIN_SEPARATOR_STR);
        let candidate = if Path::new(&normalized).is_absolute() {
            PathBuf::from(&normalized)
        } else {
            child_dir.join(&normalized)
        };
        if candidate.exists() {
            return Some(candidate);
        }
        // Also try just the filename next to the child.
        if let Some(name) = Path::new(&normalized).file_name() {
            let alt = child_dir.join(name);
            if alt.exists() {
                return Some(alt);
            }
        }
    }
    None
}
