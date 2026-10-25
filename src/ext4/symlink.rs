//! Symbolic links, stored inline below 60 bytes and in one data block
//! otherwise.

use super::{Ext4ImageBuilder, inode::PosixMetadata};
use crate::ext4::format::xattr::prepare_xattrs;
use crate::ext4::format::*;
use anyhow::Result;
use zerocopy::IntoBytes;

impl Ext4ImageBuilder {
    /// Creates and links a symbolic link, returning its inode number. Targets shorter than the inode's 60-byte
    /// `i_block` field are stored inline; longer targets use one data block.
    pub fn add_symlink(
        &mut self,
        parent: u32,
        name: &str,
        target: &str,
        meta: &PosixMetadata,
    ) -> Result<u32> {
        Self::validate_name(name)?;
        let expansion_blocks = self.estimate_dir_expansion_blocks(parent, name)?;

        let target_bytes = target.as_bytes();
        // The kernel refuses to create an empty symlink (ENOENT) and e2fsck
        // reports a zero-length one as invalid, although ext2fs_symlink()
        // itself would write it.
        if target_bytes.is_empty() {
            anyhow::bail!("symlink target must not be empty");
        }
        // Match ext2fs_symlink(): the terminating NUL must fit in one block,
        // so targets may hold up to BLOCK_SIZE - 1 bytes.
        if target_bytes.len() >= BLOCK_SIZE {
            return Err(anyhow::anyhow!(
                "symlink target must be no longer than {} bytes",
                BLOCK_SIZE - 1
            ));
        }

        let xattrs = prepare_xattrs(&meta.xattrs)?;

        // e2fsprogs defines fastlink as strictly less than i_block size (60 bytes)
        let is_fastlink = target_bytes.len() < 60;
        let block_needed = if is_fastlink { 0 } else { 1 };

        // it finds the next free inode starting from the block of the parent directory, but still do not allocate it
        let ino = self.next_free_inode(Some(parent))?;
        // it gets the group of the free inode found
        let (group, index_in_group) = self.inode_location(ino);
        // it ensures that the group has enough capacity to allocate the new inode
        self.ensure_allocation_capacity(
            block_needed as u32 + expansion_blocks,
            u32::from(!xattrs.is_empty()),
        )?;

        // allocate the inode
        self.mark_inode_used(ino);

        let mut inode = Ext4Inode::default();
        Self::populate_inode_metadata(&mut inode, meta, S_IFLNK, 0o777);
        inode.i_size_lo = target_bytes.len() as u32;
        inode.i_links_count = 1;

        if is_fastlink {
            // Fast Symlink: Write directly into the 60-byte i_block array
            inode.i_blocks_lo = 0;
            inode.i_block[..target_bytes.len()].copy_from_slice(target_bytes);
        } else {
            // Note: e2fsprogs attempts inline_data here if the feature is enabled.
            // Since this builder currently do not have inline_data support, we route
            // directly to the slow block-allocated fallback.
            let data_block = match self.allocate_free_blocks(group, 1) {
                Ok(block) => block,
                Err(error) => return Err(self.rollback_created_runs(ino, &[], error)),
            };
            let offset = data_block as usize * BLOCK_SIZE;
            // The e2fsprogs buffer is zero-initialized before copying the
            // target, which supplies the NUL terminator and a zero-filled tail.
            self.disk_data[offset..offset + BLOCK_SIZE].fill(0);
            self.disk_data[offset..offset + target_bytes.len()].copy_from_slice(target_bytes);
            inode.i_blocks_lo = (BLOCK_SIZE / 512) as u32;
            inode.i_flags = EXT4_EXTENTS_FL;
            Self::init_single_extent(&mut inode, 1, data_block);
        }

        // From here on the inode entry describes every block it owns.
        self.groups[group].inode_table[index_in_group] = inode;
        if !xattrs.is_empty() {
            inode.i_file_acl_lo = match self.write_prepared_xattr_block(group, &xattrs) {
                Ok(block) => block,
                Err(error) => return Err(self.rollback_created_inode(ino, error)),
            };
            inode.i_blocks_lo += (BLOCK_SIZE / 512) as u32;
        }

        self.groups[group].inode_table[index_in_group] = inode;
        if let Err(error) = self.add_dir_entry(parent, name, ino, EXT4_FT_SYMLINK) {
            return Err(self.rollback_created_inode(ino, error));
        }

        Ok(ino)
    }

    /// Initializes a depth-0 inline extent header with a single contiguous extent.
    /// Initializes `inode` with one depth-zero extent; returns no value because it mutates the inode in place.
    pub(crate) fn init_single_extent(inode: &mut Ext4Inode, blocks: u16, start: u32) {
        let extent_header = Ext4ExtentHeader {
            eh_magic: EXT4_EH_MAGIC,
            eh_entries: u16::from(blocks != 0),
            eh_max: 4,
            eh_depth: 0,
            eh_generation: 0,
        };
        inode.i_block[..12].copy_from_slice(extent_header.as_bytes());
        if blocks != 0 {
            let extent = Ext4Extent {
                ee_block: 0,
                ee_len: blocks,
                ee_start_hi: 0,
                ee_start_lo: start,
            };
            inode.i_block[12..24].copy_from_slice(extent.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::test_support::fresh_builder;
    use zerocopy::FromBytes;

    #[test]
    fn symlinks_use_fast_and_slow_storage_at_e2fsprogs_boundary() {
        let mut builder = fresh_builder().expect("failed to create builder");
        let metadata = PosixMetadata::default();

        let fast_ino = builder
            .add_symlink(EXT4_ROOT_INO, "fast", &"f".repeat(59), &metadata)
            .expect("failed to create fast symlink");
        let (fast_group, fast_index) = builder.inode_location(fast_ino);
        let fast = &builder.groups[fast_group].inode_table[fast_index];
        let (fast_mode, fast_size, fast_blocks) = (fast.i_mode, fast.i_size_lo, fast.i_blocks_lo);
        assert_eq!(fast_mode, S_IFLNK | 0o777);
        assert_eq!(fast_size, 59);
        assert_eq!(fast_blocks, 0);
        assert_eq!(
            &fast.i_block[..59],
            b"fffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );

        let target = "s".repeat(60);
        let slow_ino = builder
            .add_symlink(EXT4_ROOT_INO, "slow", &target, &metadata)
            .expect("failed to create slow symlink");
        let (slow_group, slow_index) = builder.inode_location(slow_ino);
        let slow = &builder.groups[slow_group].inode_table[slow_index];
        let extent = Ext4Extent::read_from_bytes(&slow.i_block[12..24]).unwrap();
        let block_offset = extent.ee_start_lo as usize * BLOCK_SIZE;
        let (slow_size, slow_blocks, extent_len) =
            (slow.i_size_lo, slow.i_blocks_lo, extent.ee_len);
        assert_eq!(slow_size, 60);
        assert_eq!(slow_blocks, (BLOCK_SIZE / 512) as u32);
        assert_eq!(extent_len, 1);
        assert_eq!(
            &builder.disk_data[block_offset..block_offset + 60],
            target.as_bytes()
        );
        assert!(
            builder.disk_data[block_offset + 60..block_offset + BLOCK_SIZE]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn symlink_target_must_be_non_empty_and_leave_room_for_nul() {
        let mut builder = fresh_builder().expect("failed to create builder");
        let metadata = PosixMetadata::default();
        let free_inodes = builder.superblock.s_free_inodes_count;
        let free_blocks = builder.superblock.s_free_blocks_count_lo;

        // e2fsck reports a zero-length symlink as invalid.
        let error = builder
            .add_symlink(EXT4_ROOT_INO, "empty", "", &metadata)
            .unwrap_err();
        assert!(error.to_string().contains("must not be empty"), "{error}");
        assert!(
            builder
                .add_symlink(
                    EXT4_ROOT_INO,
                    "too-long",
                    &"x".repeat(BLOCK_SIZE),
                    &metadata,
                )
                .is_err()
        );
        assert_eq!({ builder.superblock.s_free_inodes_count }, free_inodes);
        assert_eq!({ builder.superblock.s_free_blocks_count_lo }, free_blocks);
        assert_eq!(
            builder.lookup_dir_entry(EXT4_ROOT_INO, "empty").unwrap(),
            None
        );

        // One byte is the shortest target, and BLOCK_SIZE - 1 the longest.
        builder
            .add_symlink(EXT4_ROOT_INO, "one", "x", &metadata)
            .expect("a one-byte target is valid");
        builder
            .add_symlink(
                EXT4_ROOT_INO,
                "longest",
                &"x".repeat(BLOCK_SIZE - 1),
                &metadata,
            )
            .expect("a BLOCK_SIZE - 1 byte target is valid");
    }
}
