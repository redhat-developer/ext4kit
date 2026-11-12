//! Regular files: creation from a reader, reading back and in-place
//! content replacement.

use super::{
    Ext4ImageBuilder,
    extent::{EXT_INIT_MAX_LEN, extent_len, extents_for_runs},
    inode::PosixMetadata,
};
use crate::ext4::format::xattr::prepare_xattrs;
use crate::ext4::format::*;
use anyhow::{Result, anyhow, bail};
use std::io::{Cursor, Read, Write};

/// Stores a 512-byte sector count in `i_blocks_lo` and its high 16 bits.
fn set_block_sectors(inode: &mut Ext4Inode, sectors: u64) {
    inode.i_blocks_lo = sectors as u32;
    inode.i_osd2[0..2].copy_from_slice(&((sectors >> 32) as u16).to_le_bytes());
}

/// Returns a regular file's size from `i_size_lo` and `i_size_high`.
pub(crate) fn file_size(inode: &Ext4Inode) -> u64 {
    u64::from(inode.i_size_lo) | (u64::from(inode.i_size_high) << 32)
}

impl Ext4ImageBuilder {
    /// Creates a regular-file inode, writes `data`, links it under `parent`, and returns its inode number.
    pub fn add_file(
        &mut self,
        parent: u32,
        name: &str,
        data: &[u8],
        meta: &PosixMetadata,
    ) -> Result<u32> {
        self.add_file_from_reader(parent, name, Cursor::new(data), data.len() as u64, meta)
    }

    /// Creates a regular file while copying exactly `size` bytes from `reader`
    /// directly into its allocated inode number.
    pub fn add_file_from_reader(
        &mut self,
        parent: u32,
        name: &str,
        mut reader: impl Read,
        size: u64,
        meta: &PosixMetadata,
    ) -> Result<u32> {
        Self::validate_name(name)?;

        let file_blocks_needed = u32::try_from(size.div_ceil(BLOCK_SIZE as u64))
            .map_err(|_| anyhow::anyhow!("file is too large for ext4: {size} bytes"))?;
        let xattrs = prepare_xattrs(&meta.xattrs)?;

        // it finds the next free inode starting from the block of the parent directory, but still do not allocate it
        let file_inode = self.next_free_inode(Some(parent))?;
        // it gets the group of the free inode found
        let (group, index_in_group) = self.inode_location(file_inode);

        // it plans the data blocks to be allocated
        let target_block_ranges = self.plan_data_blocks(group, file_blocks_needed)?;

        // Extent count is known exactly from the target block ranges
        let exact_tree_meta_blocks = Self::max_extent_tree_blocks(target_block_ranges.len()) as u32;
        let xattr_blocks = u32::from(!xattrs.is_empty());
        let metadata_blocks_required = exact_tree_meta_blocks
            .checked_add(xattr_blocks)
            .ok_or_else(|| anyhow::anyhow!("block allocation count overflow"))?;

        // it ensures that the group has enough capacity to allocate the new inode
        let dir_growth_blocks = self.estimate_dir_expansion_blocks(parent, name)?;
        self.ensure_allocation_capacity(
            file_blocks_needed + dir_growth_blocks,
            metadata_blocks_required,
        )?;

        // Copy the payload into the still-free planned blocks first, so a
        // short or failing reader leaves the filesystem unchanged.
        self.write_payload(&target_block_ranges, &mut reader, size)?;

        // allocate the inode and commit the data blocks
        self.mark_inode_used(file_inode);
        self.commit_data_blocks(&target_block_ranges);

        let leaf_extents = extents_for_runs(&target_block_ranges);

        let mut inode = Ext4Inode::default();
        Self::populate_inode_metadata(&mut inode, meta, S_IFREG, 0o644);
        inode.i_size_lo = size as u32;
        inode.i_size_high = (size >> 32) as u32;
        inode.i_links_count = 1;
        inode.i_flags = EXT4_EXTENTS_FL;

        // A failed write_extent_tree releases its own blocks, and no inode
        // entry describes the data runs yet, so release them explicitly.
        let tree_meta_blocks = match self.write_extent_tree(group, &mut inode, &leaf_extents) {
            Ok(blocks) => blocks,
            Err(error) => {
                return Err(self.rollback_created_runs(file_inode, &target_block_ranges, error));
            }
        };

        // Total 512-byte sectors: data + tree metadata blocks
        let mut total_sectors =
            (file_blocks_needed as u64 + tree_meta_blocks as u64) * (BLOCK_SIZE as u64 / 512);

        // From here on the inode entry describes every block it owns.
        self.groups[group].inode_table[index_in_group] = inode;
        if !xattrs.is_empty() {
            inode.i_file_acl_lo = match self.write_prepared_xattr_block(group, &xattrs) {
                Ok(block) => block,
                Err(error) => return Err(self.rollback_created_inode(file_inode, error)),
            };
            total_sectors += BLOCK_SIZE as u64 / 512;
        }

        set_block_sectors(&mut inode, total_sectors);

        self.groups[group].inode_table[index_in_group] = inode;
        if let Err(error) = self.add_dir_entry(parent, name, file_inode, EXT4_FT_REG_FILE) {
            return Err(self.rollback_created_inode(file_inode, error));
        }

        Ok(file_inode)
    }

    /// Replaces the contents of regular file `ino` with exactly `size` bytes
    /// from `reader`, keeping its inode number, hard links, ownership, mode,
    /// timestamps and xattrs.  On error the file and all free counts are
    /// left unchanged.
    pub fn replace_file_contents(
        &mut self,
        ino: u32,
        mut reader: impl Read,
        size: u64,
    ) -> Result<()> {
        let old_runs = self.regular_file_runs(ino)?;
        let blocks_needed = u32::try_from(size.div_ceil(BLOCK_SIZE as u64))
            .map_err(|_| anyhow!("file is too large for ext4: {size} bytes"))?;
        let (group, _) = self.inode_location(ino);

        // The old blocks are released only after the new ones are written,
        // so both must fit at once.
        let new_runs = self.plan_data_blocks(group, blocks_needed)?;
        let tree_blocks = Self::max_extent_tree_blocks(new_runs.len()) as u32;
        self.ensure_allocation_capacity(blocks_needed, tree_blocks)?;
        self.write_payload(&new_runs, &mut reader, size)?;

        self.commit_data_blocks(&new_runs);
        let mut inode = *self.inode(ino);
        inode.i_block = [0; 60];
        let tree_blocks =
            self.write_extent_tree(group, &mut inode, &extents_for_runs(&new_runs))?;
        for (start, len) in old_runs {
            self.release_blocks(start, len)
                .expect("regular_file_runs validated every released block");
        }

        let xattr_blocks = u64::from(inode.i_file_acl_lo != 0);
        let blocks = u64::from(blocks_needed) + u64::from(tree_blocks) + xattr_blocks;
        set_block_sectors(&mut inode, blocks * (BLOCK_SIZE as u64 / 512));
        inode.i_size_lo = size as u32;
        inode.i_size_high = (size >> 32) as u32;
        *self.inode_mut(ino) = inode;
        Ok(())
    }

    /// Copies the contents of regular file `ino` to `writer`; returns the
    /// number of bytes written, which is the file size.  Holes and unwritten
    /// extents read as zeros.
    pub fn read_file(&self, ino: u32, writer: &mut impl Write) -> Result<u64> {
        self.regular_file_runs(ino)?;
        let inode = self.inode(ino);
        let size = file_size(inode);
        let zeros = [0u8; BLOCK_SIZE];
        let mut written = 0u64;
        let write_zeros = |writer: &mut dyn Write, mut count: u64| -> std::io::Result<()> {
            while count > 0 {
                let chunk = count.min(BLOCK_SIZE as u64) as usize;
                writer.write_all(&zeros[..chunk])?;
                count -= chunk as u64;
            }
            Ok(())
        };
        for extent in self.get_inode_extents(inode)? {
            let logical = u64::from(extent.ee_block) * BLOCK_SIZE as u64;
            if logical < written {
                bail!("file inode {ino} has overlapping or unsorted extents");
            }
            if logical >= size {
                break;
            }
            write_zeros(writer, logical - written)?;
            let length = (u64::from(extent_len(&extent)) * BLOCK_SIZE as u64).min(size - logical);
            if extent.ee_len > EXT_INIT_MAX_LEN {
                write_zeros(writer, length)?;
            } else {
                let offset = extent.ee_start_lo as usize * BLOCK_SIZE;
                writer.write_all(&self.disk_data[offset..offset + length as usize])?;
            }
            written = logical + length;
        }
        write_zeros(writer, size - written)?;
        Ok(size)
    }

    /// Validates that `ino` is an extent-mapped regular file and returns the
    /// physical runs of its data and extent-tree blocks, excluding its xattr
    /// block, after checking that each is allocated and inside the image.
    fn regular_file_runs(&self, ino: u32) -> Result<Vec<(u32, u32)>> {
        if ino == 0 || ino > self.superblock.s_inodes_count || !self.is_inode_allocated(ino) {
            bail!("inode {ino} does not exist");
        }
        let inode = self.inode(ino);
        if inode.file_type() != S_IFREG {
            bail!("inode {ino} is not a regular file");
        }
        if inode.i_flags & EXT4_EXTENTS_FL == 0 {
            bail!("file inode {ino} does not use extents");
        }
        let tree = self.read_extent_tree(inode)?;
        let mut runs: Vec<(u32, u32)> = tree
            .leaves
            .iter()
            .map(|extent| (extent.ee_start_lo, extent_len(extent)))
            .filter(|&(_, len)| len != 0)
            .chain(tree.tree_blocks.into_iter().map(|block| (block, 1)))
            .collect();
        runs.sort_unstable();
        for (index, &(start, len)) in runs.iter().enumerate() {
            self.validate_allocated_run(start, len)?;
            if let Some(&(next, _)) = runs.get(index + 1)
                && u64::from(start) + u64::from(len) > u64::from(next)
            {
                bail!("file inode {ino} maps block {next} more than once");
            }
        }
        Ok(runs)
    }

    /// Copies exactly `size` bytes from `reader` into the planned `runs` and
    /// zeroes the rest of the last block, which may hold data of a file
    /// removed from a reopened image.
    fn write_payload(
        &mut self,
        runs: &[(u32, u32)],
        reader: &mut impl Read,
        size: u64,
    ) -> Result<()> {
        let mut remaining = size;
        for &(start, len) in runs {
            let offset = start as usize * BLOCK_SIZE;
            let run_bytes = len as usize * BLOCK_SIZE;
            let chunk = remaining.min(run_bytes as u64) as usize;
            reader
                .read_exact(&mut self.disk_data[offset..offset + chunk])
                .map_err(|error| anyhow!("cannot read {size}-byte file payload: {error}"))?;
            self.disk_data[offset + chunk..offset + run_bytes].fill(0);
            remaining -= chunk as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::ext4::format::*;
    use crate::ext4::test_support::fresh_builder;
    use crate::ext4::{Ext4ImageBuilder, PosixMetadata};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::io::{Read, repeat};
    use zerocopy::FromBytes;

    fn contents(builder: &Ext4ImageBuilder, ino: u32) -> Vec<u8> {
        let mut data = Vec::new();
        builder.read_file(ino, &mut data).unwrap();
        data
    }

    fn free_counts(builder: &Ext4ImageBuilder) -> (u32, u32, Vec<u16>) {
        (
            builder.superblock.s_free_blocks_count_lo,
            builder.superblock.s_free_inodes_count,
            builder
                .group_descriptors
                .iter()
                .map(|descriptor| descriptor.bg_free_blocks_count_lo)
                .collect(),
        )
    }

    #[test]
    fn replacing_contents_keeps_the_inode_and_its_metadata() -> Result<()> {
        let mut builder = fresh_builder()?;
        let meta = PosixMetadata {
            mode: Some(0o640),
            uid: 0,
            gid: 42,
            mtime: 1_700_000_000,
            xattrs: HashMap::from([("security.selinux".to_owned(), b"shadow_t\0".to_vec())]),
            ..Default::default()
        };
        let etc = builder.mkdir(EXT4_ROOT_INO, "etc", None)?;
        let shadow = builder.add_file(etc, "shadow", b"root:*:1::::::\n", &meta)?;
        builder.add_hard_link(etc, "shadow-link", shadow)?;
        let before = *builder.inode(shadow);
        let free_blocks = builder.superblock.s_free_blocks_count_lo;

        let replacement = vec![b'a'; 3 * BLOCK_SIZE + 10];
        builder.replace_file_contents(shadow, &replacement[..], replacement.len() as u64)?;

        let after = *builder.inode(shadow);
        assert_eq!(contents(&builder, shadow), replacement);
        let link = builder.lookup_dir_entry(etc, "shadow-link")?.unwrap();
        assert_eq!(contents(&builder, link), replacement);
        let (mode, uid, gid, mtime) = (after.i_mode, after.i_uid, after.i_gid, after.i_mtime);
        assert_eq!(mode, { before.i_mode });
        assert_eq!((uid, gid, mtime), (0, 42, 1_700_000_000));
        let (links, xattr_block) = (after.i_links_count, after.i_file_acl_lo);
        assert_eq!((links, xattr_block), (2, before.i_file_acl_lo));
        // Four data blocks and the xattr block, in 512-byte sectors.
        assert_eq!({ after.i_blocks_lo }, 5 * 8);
        assert_eq!(
            { builder.superblock.s_free_blocks_count_lo },
            free_blocks - 3
        );

        builder.replace_file_contents(shadow, &b""[..], 0)?;
        assert!(contents(&builder, shadow).is_empty());
        assert_eq!({ builder.inode(shadow).i_blocks_lo }, 8);
        assert_eq!(
            { builder.superblock.s_free_blocks_count_lo },
            free_blocks + 1
        );
        Ok(())
    }

    #[test]
    fn replacing_contents_can_spread_over_reclaimed_holes() -> Result<()> {
        let mut builder = fresh_builder()?;
        let meta = PosixMetadata::default();
        let dir = builder.mkdir(EXT4_ROOT_INO, "holes", None)?;
        for index in 0..40 {
            builder.add_file(dir, &format!("f{index}"), &[1; BLOCK_SIZE], &meta)?;
        }
        let target = builder.add_file(EXT4_ROOT_INO, "target", b"old", &meta)?;
        for index in (0..40).step_by(2) {
            builder.remove_dir_entry(dir, &format!("f{index}"))?;
        }
        // A reopened image starts allocating at the beginning of each group.
        builder.next_block_hints.fill(0);
        let free_blocks = builder.superblock.s_free_blocks_count_lo;

        let replacement: Vec<u8> = (0..10 * BLOCK_SIZE).map(|byte| byte as u8).collect();
        builder.replace_file_contents(target, &replacement[..], replacement.len() as u64)?;

        // Ten one-block holes need an external extent leaf block; the old
        // single data block is released.
        assert_eq!(contents(&builder, target), replacement);
        assert_eq!({ builder.inode(target).i_blocks_lo }, 11 * 8);
        assert_eq!(
            { builder.superblock.s_free_blocks_count_lo },
            free_blocks - 10
        );
        Ok(())
    }

    #[test]
    fn reused_blocks_do_not_leak_old_data_past_the_end_of_a_file() -> Result<()> {
        let mut builder = fresh_builder()?;
        let meta = PosixMetadata::default();
        builder.add_file(EXT4_ROOT_INO, "secret", &[0xAA; BLOCK_SIZE], &meta)?;
        builder.remove_dir_entry(EXT4_ROOT_INO, "secret")?;
        builder.next_block_hints.fill(0);
        let small = builder.add_file(EXT4_ROOT_INO, "small", b"hi", &meta)?;

        let block = builder.get_inode_extents(builder.inode(small))?[0].ee_start_lo;
        let offset = block as usize * BLOCK_SIZE;
        assert_eq!(&builder.disk_data[offset..offset + 2], b"hi");
        assert!(
            builder.disk_data[offset + 2..offset + BLOCK_SIZE]
                .iter()
                .all(|&byte| byte == 0)
        );
        Ok(())
    }

    #[test]
    fn failed_replace_preflight_leaves_the_file_unchanged() -> Result<()> {
        let mut builder = fresh_builder()?;
        let file = builder.add_file(EXT4_ROOT_INO, "f", b"original", &PosixMetadata::default())?;
        let inode = *builder.inode(file);
        let counts = free_counts(&builder);

        // More data than the image can hold.
        let size = 64 * 1024 * 1024;
        assert!(
            builder
                .replace_file_contents(file, repeat(0).take(size), size)
                .is_err()
        );
        // A reader that ends early.
        let short = builder.replace_file_contents(file, &b"short"[..], 4 * BLOCK_SIZE as u64);
        assert!(short.is_err());
        // Not a regular file.
        assert!(
            builder
                .replace_file_contents(EXT4_ROOT_INO, &b""[..], 0)
                .is_err()
        );

        assert_eq!(free_counts(&builder), counts);
        assert_eq!(builder.inode(file).i_block, inode.i_block);
        assert_eq!(contents(&builder, file), b"original");
        Ok(())
    }

    #[test]
    fn failed_add_file_reader_leaves_counts_unchanged() -> Result<()> {
        let mut builder = fresh_builder()?;
        let counts = free_counts(&builder);
        let meta = PosixMetadata::default();
        let result = builder.add_file_from_reader(EXT4_ROOT_INO, "f", &b"short"[..], 100, &meta);
        assert!(result.is_err());
        assert_eq!(free_counts(&builder), counts);
        assert_eq!(builder.lookup_dir_entry(EXT4_ROOT_INO, "f")?, None);
        Ok(())
    }

    #[test]
    fn oversized_xattrs_fail_before_the_payload_is_read() -> Result<()> {
        // Rejecting the set in preflight, rather than when the xattr block is
        // written, avoids streaming a payload that would only be rolled back.
        struct Unread;
        impl Read for Unread {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("the payload must not be read");
            }
        }
        let mut builder = fresh_builder()?;
        let counts = free_counts(&builder);
        let mut meta = PosixMetadata::default();
        meta.xattrs.insert("user.big".into(), vec![0; BLOCK_SIZE]);
        let size = 4 * BLOCK_SIZE as u64;
        let error = builder
            .add_file_from_reader(EXT4_ROOT_INO, "f", Unread, size, &meta)
            .unwrap_err();
        assert!(error.to_string().contains("exceed one filesystem block"));
        assert_eq!(free_counts(&builder), counts);
        Ok(())
    }

    #[test]
    fn reading_a_non_regular_file_fails() -> Result<()> {
        let builder = fresh_builder()?;
        assert!(builder.read_file(EXT4_ROOT_INO, &mut Vec::new()).is_err());
        assert!(builder.read_file(9999, &mut Vec::new()).is_err());
        Ok(())
    }

    #[test]
    fn file_is_linked_and_uses_contiguous_extent_and_xattrs() {
        let mut builder = fresh_builder().expect("failed to create builder");
        let mut metadata = PosixMetadata {
            mode: Some(0o640),
            uid: 70_000,
            gid: 80_000,
            mtime: 42,
            ..Default::default()
        };
        metadata
            .xattrs
            .insert("user.note".into(), b"metadata".to_vec());
        let ino = builder
            .add_file(EXT4_ROOT_INO, "payload", &[7; BLOCK_SIZE + 10], &metadata)
            .expect("failed to add file");
        let (group, index) = builder.inode_location(ino);
        let inode = &builder.groups[group].inode_table[index];
        let mode = inode.i_mode;
        let size = inode.i_size_lo;
        let sectors = inode.i_blocks_lo;
        let xattr_block = inode.i_file_acl_lo;
        assert_eq!(mode, S_IFREG | 0o640);
        assert_eq!(size, (BLOCK_SIZE + 10) as u32);
        assert_eq!(sectors, 24); // two payload blocks plus one xattr block, in sectors
        assert_ne!(xattr_block, 0);
        let extent = Ext4Extent::read_from_bytes(&inode.i_block[12..24]).unwrap();
        let extent_len = extent.ee_len;
        let extent_start = extent.ee_start_lo;
        assert_eq!(extent_len, 2);
        assert_eq!(
            &builder.disk_data[extent_start as usize * BLOCK_SIZE
                ..extent_start as usize * BLOCK_SIZE + BLOCK_SIZE + 10],
            &[7; BLOCK_SIZE + 10]
        );
    }
}
