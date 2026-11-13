//! The ext4 filesystem engine: [`Ext4ImageBuilder`] lays out, loads and
//! changes an image by inode number.  [`format`] holds the on-disk
//! structures it reads and writes.

use std::{
    collections::HashMap,
    fs::OpenOptions,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

use memmap2::{MmapMut, MmapOptions};

use crate::ext4::format::*;

mod allocation;
mod directory;
mod extent;
mod file;
pub mod format;
mod htree;
mod inode;
mod layout;
use layout::PlannedGeometry;
mod open;
pub mod reader;
mod special;
mod symlink;
pub use inode::PosixMetadata;

pub struct Ext4ImageBuilder {
    pub superblock: Ext4SuperBlock,
    pub group_descriptors: Vec<Ext4GroupDesc>,
    pub groups: Vec<Ext4BlockGroup>,
    pub disk_data: DiskData,
    /// Next bitmap positions to probe, avoiding a full bitmap rescan for
    /// every imported entry.
    pub(crate) next_block_hints: Vec<usize>,
    pub(crate) next_inode_hints: Vec<usize>,
    /// Cached names and append slots for linear directories only. Indexed
    /// directories deliberately bypass this cache and use htree lookup.
    pub(crate) directory_caches: HashMap<u32, directory::DirectoryCache>,
    /// UID and GID forced onto every entry imported from the host, like
    /// `COPY --chown`; `None` keeps the ownership read from the host.
    pub(crate) host_owner: Option<(u32, u32)>,
}

/// Mutable image bytes backed either by memory (primarily tests and library
/// compatibility), by the destination file used for streamed image builds,
/// or by a private copy-on-write mapping of an image opened read-only.
pub enum DiskData {
    Memory(Vec<u8>),
    File {
        mapping: MmapMut,
        path: PathBuf,
    },
    /// Changes stay in private pages and never reach the file.
    ReadOnly(MmapMut),
}

impl Deref for DiskData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Memory(data) => data,
            Self::File { mapping, .. } | Self::ReadOnly(mapping) => mapping,
        }
    }
}

impl DerefMut for DiskData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Memory(data) => data,
            Self::File { mapping, .. } | Self::ReadOnly(mapping) => mapping,
        }
    }
}

impl Ext4ImageBuilder {
    /// Creates an in-memory ext4 image builder of `size_mb`; returns initialized filesystem state.
    /// A trailing block group without room for its metadata and a few free
    /// blocks is dropped, so the
    /// filesystem can be slightly smaller than the requested image; a size
    /// too small for a single block group panics with a clear message.
    pub fn new(size_mb: usize) -> Self {
        let image_size_bytes = size_mb * 1024 * 1024;
        let geometry = Self::plan_geometry(image_size_bytes / BLOCK_SIZE).unwrap_or_else(|err| {
            panic!("cannot lay out an image of {image_size_bytes} bytes: {err}")
        });
        Self::new_with_storage(geometry, DiskData::Memory(vec![0; image_size_bytes]))
    }

    /// Creates an image directly in `output_path`. The mapping is file-backed,
    /// so imported payload pages can be flushed instead of retaining a second
    /// complete image-sized heap allocation.
    pub fn new_file(size_mb: usize, output_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let image_size_bytes = size_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| std::io::Error::other("image size overflow"))?;
        // Reject sizes no block group fits before truncating the output file.
        let geometry = Self::plan_geometry(image_size_bytes / BLOCK_SIZE)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let path = output_path.as_ref().to_owned();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(image_size_bytes as u64)?;
        // the file remains at its fixed length for the lifetime of the
        // mapping, and all access is mediated through the uniquely owned MmapMut.
        let mapping = unsafe { MmapOptions::new().map_mut(&file)? };
        Ok(Self::new_with_storage(
            geometry,
            DiskData::File { mapping, path },
        ))
    }

    fn new_with_storage(geometry: PlannedGeometry, disk_data: DiskData) -> Self {
        if BYTES_PER_INODE_RATIO < BLOCK_SIZE {
            panic!("BYTES_PER_INODE_RATIO must be greater than or equal to BLOCK_SIZE");
        }

        let blocks_number = geometry.blocks_number;
        let block_groups_number = geometry.block_groups_number;

        let inodes_per_group = geometry.inodes_per_group;

        // Re-calculate actual total inodes based on rounded group count
        let total_inodes = inodes_per_group * block_groups_number;

        let mut superblock = Ext4SuperBlock::default();
        // A random filesystem UUID, as mke2fs writes one, so later steps can
        // tie a snapshot sidecar to exactly this image.
        getrandom::fill(&mut superblock.s_uuid)
            .expect("the operating system must provide random bytes for the ext4 UUID");
        // Mark it an RFC 4122 version-4 (random) UUID, as libuuid's
        // uuid_generate() does for mke2fs, so tools that decode the version
        // see a well-formed UUID.
        superblock.s_uuid[6] = (superblock.s_uuid[6] & 0x0f) | 0x40;
        superblock.s_uuid[8] = (superblock.s_uuid[8] & 0x3f) | 0x80;
        // Calculate a random seed for the directory-index hashes
        let mut hash_seed_bytes = [0u8; 16];
        getrandom::fill(&mut hash_seed_bytes)
            .expect("the operating system must provide random bytes for the ext4 hash seed");
        superblock.s_hash_seed = std::array::from_fn(|word| {
            u32::from_le_bytes(hash_seed_bytes[word * 4..word * 4 + 4].try_into().unwrap())
        });
        superblock.s_inodes_count = total_inodes as u32;
        superblock.s_blocks_count_lo = blocks_number as u32;
        superblock.s_free_blocks_count_lo = blocks_number as u32; // Adjusted in init_layout
        superblock.s_free_inodes_count = total_inodes as u32; // Adjusted in init_layout
        superblock.s_log_block_size = 2; // 4KB block size
        superblock.s_blocks_per_group = BLOCKS_PER_GROUP as u32;
        superblock.s_clusters_per_group = BLOCKS_PER_GROUP as u32;
        superblock.s_inodes_per_group = inodes_per_group as u32;

        // Build groups and group descriptor arrays
        let mut groups = Vec::with_capacity(block_groups_number);
        let mut group_descriptors = Vec::with_capacity(block_groups_number);

        for group_idx in 0..block_groups_number {
            let group_blocks = if group_idx == block_groups_number - 1 {
                let remainder = blocks_number % BLOCKS_PER_GROUP;
                if remainder != 0 {
                    remainder
                } else {
                    BLOCKS_PER_GROUP
                }
            } else {
                BLOCKS_PER_GROUP
            };

            groups.push(Ext4BlockGroup {
                group_id: group_idx as u32,
                blocks_count: group_blocks as u32,
                // The on-disk inode/block bitmaps occupie one complete block.
                // During init, the unused bits will be padded with 1s
                block_bitmap: vec![0u8; BLOCKS_PER_GROUP / 8],
                inode_bitmap: vec![0u8; BLOCKS_PER_GROUP / 8],
                inode_table: vec![Ext4Inode::default(); inodes_per_group],
                free_blocks_count: group_blocks as u16,
                free_inodes_count: inodes_per_group as u16,
            });

            group_descriptors.push(Ext4GroupDesc {
                bg_block_bitmap_lo: 0,
                bg_inode_bitmap_lo: 0,
                bg_inode_table_lo: 0,
                bg_free_blocks_count_lo: group_blocks as u16,
                bg_free_inodes_count_lo: inodes_per_group as u16,
                bg_used_dirs_count_lo: 0,
                bg_flags: 0,
                bg_exclude_bitmap_lo: 0,
                bg_block_bitmap_csum_lo: 0,
                bg_inode_bitmap_csum_lo: 0,
                bg_itable_unused_lo: 0,
                bg_checksum: 0,
            });
        }

        let mut builder = Self {
            superblock,
            group_descriptors,
            groups,
            disk_data,
            next_block_hints: vec![0; block_groups_number],
            next_inode_hints: vec![0; block_groups_number],
            directory_caches: HashMap::new(),
            host_owner: None,
        };

        builder.init_layout();
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskData, Ext4ImageBuilder};

    #[test]
    fn file_backed_builder_flushes_without_an_image_sized_heap_buffer() -> anyhow::Result<()> {
        let path =
            std::env::temp_dir().join(format!("ext4kit-file-backed-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut builder = Ext4ImageBuilder::new_file(16, &path)?;
            assert!(matches!(builder.disk_data, DiskData::File { .. }));
            builder.create_root_dir()?;
            builder.create_lost_and_found()?;
            builder.reserve_inodes();
            builder.write_to_file(path.to_str().unwrap())?;
        }
        assert_eq!(std::fs::metadata(&path)?.len(), 16 * 1024 * 1024);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn every_image_gets_a_distinct_nonzero_filesystem_uuid() -> anyhow::Result<()> {
        let first = crate::ext4::test_support::fresh_builder()?;
        let second = crate::ext4::test_support::fresh_builder()?;
        assert_ne!(
            first.superblock.s_uuid, [0u8; 16],
            "the UUID must not stay zeroed"
        );
        assert_ne!(
            first.superblock.s_uuid, second.superblock.s_uuid,
            "two images must not share a UUID"
        );
        let uuid = first.superblock.s_uuid;
        assert_eq!(uuid[6] >> 4, 4, "version 4 (random) UUID");
        assert_eq!(uuid[8] >> 6, 0b10, "RFC 4122 variant");
        Ok(())
    }

    #[test]
    fn open_round_trips_the_random_uuid() -> anyhow::Result<()> {
        let path = std::env::temp_dir().join(format!("ext4kit-uuid-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let uuid;
        {
            let mut builder = Ext4ImageBuilder::new_file(16, &path)?;
            builder.create_root_dir()?;
            builder.create_lost_and_found()?;
            builder.reserve_inodes();
            builder.write_to_file(path.to_str().unwrap())?;
            uuid = builder.superblock.s_uuid;
        }
        let reopened = Ext4ImageBuilder::open(&path)?;
        assert_eq!(reopened.superblock.s_uuid, uuid);
        std::fs::remove_file(path)?;
        Ok(())
    }
}

/// The fixture most unit tests start from: an in-memory 16 MiB image with
/// the root directory, lost+found and the reserved inodes.
#[cfg(test)]
pub(crate) mod test_support {
    use super::Ext4ImageBuilder;

    pub(crate) fn fresh_builder() -> anyhow::Result<Ext4ImageBuilder> {
        let mut builder = Ext4ImageBuilder::new(16);
        builder.create_root_dir()?;
        builder.create_lost_and_found()?;
        builder.reserve_inodes();
        Ok(builder)
    }
}
