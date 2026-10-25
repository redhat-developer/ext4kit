//! Pre-scanned host-tree snapshot used for image sizing and import.

use crate::{
    ext4::format::BLOCK_SIZE,
    ext4::{Ext4ImageBuilder, PosixMetadata},
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default)]
pub(super) struct HostUsage {
    pub(super) data_blocks: u64,
    pub(super) inodes: u64,
}

/// A pre-order host-tree snapshot used to size and import an image without a
/// second metadata or directory-enumeration pass. Entries live in a flat arena
/// so scanning and importing arbitrarily deep directory trees never recurse.
pub struct HostTree {
    pub(super) entries: Vec<HostEntry>,
    pub(super) usage: HostUsage,
}

pub(super) struct HostEntry {
    pub(super) path: PathBuf,
    pub(super) metadata: HostMetadata,
    pub(super) file_type: HostFileType,
    pub(super) symlink_target: Option<String>,
    pub(super) parent_index: Option<usize>,
}

/// Returns the walker options for the host importers. They stat every entry
/// themselves, because the walker's metadata lacks the fields ext4 needs on
/// Windows (access/creation times, the read-only bit). On Windows and macOS
/// directory enumeration supplies metadata for free, and asking for types
/// only would switch to a slower enumeration; elsewhere the walker's metadata
/// is an extra `lstat` per entry, so it is skipped.
pub(super) fn walk_options() -> dua_core::Options {
    if cfg!(any(windows, target_os = "macos")) {
        dua_core::Options::default()
    } else {
        dua_core::Options::default().skip_metadata()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostFileType {
    Directory,
    File,
    Symlink,
}

/// Compact, platform-neutral metadata retained between the scan and import
/// passes. Unlike `fs::Metadata`, this stores only values written to ext4.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HostMetadata {
    pub(super) size: u64,
    uid: u32,
    gid: u32,
    mode: u16,
    atime: u32,
    ctime: u32,
    mtime: u32,
}

impl HostMetadata {
    pub(super) fn from_std(metadata: &fs::Metadata, is_dir: bool) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = is_dir; // Unix hosts supply real permission bits.
            Self {
                size: metadata.len(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: (metadata.mode() & 0o7777) as u16,
                atime: clamp_unix_timestamp(metadata.atime()),
                ctime: clamp_unix_timestamp(metadata.ctime()),
                mtime: clamp_unix_timestamp(metadata.mtime()),
            }
        }
        #[cfg(not(unix))]
        {
            let default_mode = if is_dir { 0o755 } else { 0o644 };
            let mode = if metadata.permissions().readonly() {
                default_mode & !0o222
            } else {
                default_mode
            };
            Self {
                size: metadata.len(),
                mode,
                atime: system_time_seconds(metadata.accessed()),
                ctime: system_time_seconds(metadata.created()),
                mtime: system_time_seconds(metadata.modified()),
                ..Self::default()
            }
        }
    }

    pub(super) fn as_posix(self) -> PosixMetadata {
        PosixMetadata {
            mode: Some(self.mode),
            uid: self.uid,
            gid: self.gid,
            atime: self.atime,
            ctime: self.ctime,
            mtime: self.mtime,
            ..Default::default()
        }
    }
}

#[cfg(unix)]
fn clamp_unix_timestamp(seconds: i64) -> u32 {
    seconds.clamp(0, i64::from(u32::MAX)) as u32
}

#[cfg(not(unix))]
fn system_time_seconds(time: std::io::Result<std::time::SystemTime>) -> u32 {
    time.ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_secs().min(u64::from(u32::MAX)) as u32)
}

struct PendingHostEntry {
    entry: HostEntry,
    directory_id: Option<dua_core::DirectoryId>,
    parent_directory_id: Option<dua_core::DirectoryId>,
}

impl HostTree {
    /// Streams `source` through dua-core's bounded parallel walker and retains
    /// compact metadata in a stable pre-order arena for sizing and import.
    pub fn scan(source: impl AsRef<Path>) -> Result<Self> {
        let mut usage = HostUsage::default();
        let mut pending = Vec::with_capacity(8192);
        let mut directory_bytes = HashMap::new();
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let walker = dua_core::walk(
            source.as_ref(),
            threads,
            dua_core::Order::ParentFirst,
            walk_options(),
            |_| true,
        );

        for walked in walker {
            let walked = walked
                .with_context(|| format!("cannot walk host tree {}", source.as_ref().display()))?;
            let path = walked.path();
            let std_metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("cannot inspect host path {}", path.display()))?;
            let file_type = if walked.file_type.is_dir() {
                HostFileType::Directory
            } else if walked.file_type.is_file() {
                HostFileType::File
            } else if walked.file_type.is_symlink() {
                HostFileType::Symlink
            } else {
                return Err(anyhow::anyhow!(
                    "unsupported host file type at {} (only files, directories, and symlinks can be imported)",
                    path.display()
                ));
            };
            let metadata =
                HostMetadata::from_std(&std_metadata, file_type == HostFileType::Directory);
            let mut symlink_target = None;

            usage.inodes += 1;
            if walked.depth > 0 {
                let name = walked.file_name.to_str().ok_or_else(|| {
                    anyhow::anyhow!("host path is not valid UTF-8: {}", path.display())
                })?;
                Ext4ImageBuilder::validate_name(name)?;
                *directory_bytes
                    .entry(
                        walked
                            .parent_directory_id
                            .expect("non-root entry has a parent"),
                    )
                    .or_insert(24u64) += (8 + name.len()).next_multiple_of(4) as u64;
            }

            if file_type == HostFileType::Directory {
                directory_bytes
                    .entry(walked.directory_id.expect("directory has an id"))
                    .or_insert(24);
            } else if file_type == HostFileType::File {
                usage.data_blocks += metadata.size.div_ceil(BLOCK_SIZE as u64);
            } else {
                let target = fs::read_link(&path)
                    .with_context(|| format!("cannot read symbolic link {}", path.display()))?;
                let target = target.to_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "symbolic-link target is not valid UTF-8: {}",
                        path.display()
                    )
                })?;
                // Reject what add_symlink would, before an import writes
                // anything.
                if target.is_empty() {
                    return Err(anyhow::anyhow!(
                        "symlink target must not be empty: {}",
                        path.display()
                    ));
                }
                if target.len() >= BLOCK_SIZE {
                    return Err(anyhow::anyhow!(
                        "symlink target must be no longer than {} bytes: {}",
                        BLOCK_SIZE - 1,
                        path.display()
                    ));
                }
                usage.data_blocks += u64::from(target.len() >= 60);
                symlink_target = Some(target.to_owned());
            }

            pending.push(PendingHostEntry {
                entry: HostEntry {
                    path,
                    metadata,
                    file_type,
                    symlink_target,
                    parent_index: None,
                },
                directory_id: walked.directory_id,
                parent_directory_id: walked.parent_directory_id,
            });
        }

        // Parallel completion does not define sibling order. Sorting full paths
        // restores deterministic pre-order while directory IDs preserve parents.
        pending.sort_unstable_by(|left, right| left.entry.path.cmp(&right.entry.path));
        let directory_indices: HashMap<_, _> = pending
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.directory_id.map(|id| (id, index)))
            .collect();
        let entries = pending
            .into_iter()
            .map(|mut pending| {
                pending.entry.parent_index =
                    pending.parent_directory_id.map(|id| directory_indices[&id]);
                pending.entry
            })
            .collect();
        usage.data_blocks += directory_bytes
            .values()
            .map(|bytes| bytes.div_ceil(BLOCK_SIZE as u64))
            .sum::<u64>();

        Ok(Self { entries, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_host_metadata_is_compact() {
        assert_eq!(std::mem::size_of::<HostMetadata>(), 32);
        assert!(std::mem::size_of::<HostMetadata>() < std::mem::size_of::<fs::Metadata>());
    }

    #[test]
    fn scanned_tree_is_a_preorder_flat_arena() -> Result<()> {
        let base = std::env::temp_dir().join(format!("ext4kit-arena-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a").join("nested"))?;
        fs::write(base.join("a").join("nested").join("child"), b"child")?;
        fs::write(base.join("z"), b"z")?;

        let tree = HostTree::scan(&base)?;
        assert_eq!(tree.entries[0].parent_index, None);
        for (index, entry) in tree.entries.iter().enumerate().skip(1) {
            assert!(entry.parent_index.is_some_and(|parent| parent < index));
        }
        let names: Vec<_> = tree
            .entries
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                base.file_name().unwrap().to_str().unwrap(),
                "a",
                "nested",
                "child",
                "z"
            ]
        );

        fs::remove_dir_all(base)?;
        Ok(())
    }
}
