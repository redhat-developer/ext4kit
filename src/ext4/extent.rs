//! Extent trees: reading and validating them, and writing them into an
//! inode and its external index and leaf blocks.

use super::Ext4ImageBuilder;
use crate::ext4::format::*;
use anyhow::Result;
use zerocopy::{FromBytes, IntoBytes};

/// Maximum extent tree depth accepted by e2fsprogs and the kernel.
const EXT4_MAX_EXTENT_DEPTH: u16 = 5;

/// Initialized extents are at most 32,768 blocks long; larger `ee_len`
/// values encode an uninitialized extent of `ee_len - 32768` blocks.
pub(crate) const EXT_INIT_MAX_LEN: u16 = 1 << 15;

/// Leaf extents and external index/leaf blocks reachable from an inode.
#[derive(Default)]
pub(crate) struct ExtentTree {
    pub(crate) leaves: Vec<Ext4Extent>,
    pub(crate) tree_blocks: Vec<u32>,
}

/// Builds initialized leaf extents from physical runs, assigning consecutive logical blocks.
pub(super) fn extents_for_runs(runs: &[(u32, u32)]) -> Vec<Ext4Extent> {
    let mut logical = 0u32;
    runs.iter()
        .map(|&(start, len)| {
            let extent = Ext4Extent {
                ee_block: logical,
                ee_len: len as u16,
                ee_start_hi: 0,
                ee_start_lo: start,
            };
            logical += len;
            extent
        })
        .collect()
}

/// Returns the number of blocks mapped by `extent`, initialized or not.
pub(crate) fn extent_len(extent: &Ext4Extent) -> u32 {
    let len = extent.ee_len;
    u32::from(if len > EXT_INIT_MAX_LEN {
        len - EXT_INIT_MAX_LEN
    } else {
        len
    })
}

/// Mirrors ext2fs_extent_header_verify(): validates magic, entry count and
/// `eh_max` against the node size (allowing room for a checksum tail).
pub(crate) fn verify_extent_header(node: &[u8]) -> Result<Ext4ExtentHeader> {
    let header = node
        .get(..12)
        .and_then(|bytes| Ext4ExtentHeader::read_from_bytes(bytes).ok())
        .ok_or_else(|| anyhow::anyhow!("truncated extent header"))?;
    if header.eh_magic != EXT4_EH_MAGIC {
        return Err(anyhow::anyhow!("invalid extent magic"));
    }
    let (entries, max) = (header.eh_entries as usize, header.eh_max as usize);
    let capacity = (node.len() - 12) / 12;
    if entries > max || max > capacity || max + 2 < capacity {
        return Err(anyhow::anyhow!("invalid extent header entry count"));
    }
    Ok(header)
}

impl Ext4ImageBuilder {
    /// Walks an extent tree node, requiring each child to be exactly one
    /// level below its parent.  Strictly decreasing depth bounds the walk, so
    /// a corrupt tree cannot cycle.
    fn walk_extent_node(
        &self,
        node: &[u8],
        expected_depth: Option<u16>,
        tree: &mut ExtentTree,
    ) -> Result<()> {
        let header = verify_extent_header(node)?;
        let depth = header.eh_depth;
        if depth > EXT4_MAX_EXTENT_DEPTH || expected_depth.is_some_and(|d| d != depth) {
            return Err(anyhow::anyhow!("invalid extent tree depth {depth}"));
        }
        for i in 0..header.eh_entries as usize {
            let entry = &node[12 + i * 12..24 + i * 12];
            if depth == 0 {
                tree.leaves.push(
                    Ext4Extent::read_from_bytes(entry)
                        .map_err(|_| anyhow::anyhow!("invalid extent entry"))?,
                );
                continue;
            }
            let idx = Ext4ExtentIdx::read_from_bytes(entry)
                .map_err(|_| anyhow::anyhow!("invalid extent index"))?;
            let block = idx.ei_leaf_lo;
            let offset = block as usize * BLOCK_SIZE;
            let child = self
                .disk_data
                .get(offset..offset + BLOCK_SIZE)
                .ok_or_else(|| anyhow::anyhow!("extent block {block} is outside the image"))?;
            tree.tree_blocks.push(block);
            self.walk_extent_node(child, Some(depth - 1), tree)?;
        }
        Ok(())
    }

    /// Decodes the complete extent tree rooted in `inode.i_block`.
    pub(crate) fn read_extent_tree(&self, inode: &Ext4Inode) -> Result<ExtentTree> {
        let mut tree = ExtentTree::default();
        self.walk_extent_node(&inode.i_block, None, &mut tree)?;
        Ok(tree)
    }

    /// Decodes every leaf extent referenced by `inode`; returns them in tree traversal order.
    pub(crate) fn get_inode_extents(&self, inode: &Ext4Inode) -> Result<Vec<Ext4Extent>> {
        Ok(self.read_extent_tree(inode)?.leaves)
    }

    /// Returns a directory's physical data blocks in logical order, or an error if it is invalid.
    pub(crate) fn get_dir_blocks(&self, ino: u32) -> Result<Vec<u32>> {
        if ino == 0 {
            return Err(anyhow::anyhow!("inode 0 does not exist"));
        }
        if !self.is_inode_allocated(ino) {
            return Err(anyhow::anyhow!("inode {ino} does not exist"));
        }
        let inode = self.inode(ino);
        if !inode.is_dir() {
            return Err(anyhow::anyhow!("inode {ino} is not a directory"));
        }
        if inode.i_flags & EXT4_EXTENTS_FL == 0 {
            return Err(anyhow::anyhow!(
                "directory inode {ino} does not use extents"
            ));
        }
        let extents = self.get_inode_extents(inode)?;
        let mut blocks = Vec::new();
        for ext in extents {
            blocks.extend((0..extent_len(&ext)).map(|b| ext.ee_start_lo + b));
        }
        Ok(blocks)
    }

    /// Calculate maximum metadata blocks required for an extent tree.
    /// Calculates the maximum external extent-tree blocks for leaf extent count; returns that count.
    pub fn max_extent_tree_blocks(num_extents: usize) -> usize {
        if num_extents <= EXT4_EXTENTS_IN_INODE {
            return 0;
        }

        // External leaf blocks (depth 0)
        let leaf_blocks = num_extents.div_ceil(EXT4_EXTENTS_PER_BLOCK);
        let mut total_metadata_blocks = leaf_blocks;

        // Index blocks (depth 1+)
        let mut current_index_entries = leaf_blocks;
        while current_index_entries > EXT4_EXTENTS_IN_INODE {
            let index_blocks = current_index_entries.div_ceil(EXT4_INDEXES_PER_BLOCK);
            total_metadata_blocks += index_blocks;
            current_index_entries = index_blocks;
        }

        total_metadata_blocks
    }

    /// Allocates one extent-tree block and records it in `allocated`; on
    /// failure releases every block recorded so far.
    fn allocate_tree_block(&mut self, group: usize, allocated: &mut Vec<u32>) -> Result<u32> {
        match self.allocate_free_blocks(group, 1) {
            Ok(block) => {
                allocated.push(block);
                Ok(block)
            }
            Err(error) => {
                for &block in allocated.iter() {
                    self.release_block(block)
                        .expect("extent-tree block was allocated by this call");
                }
                Err(error)
            }
        }
    }

    /// Serializes an extent tree into the inode and external blocks.
    /// Returns the number of external tree metadata blocks allocated.
    /// Serializes an inode's extent tree; returns the number of external tree blocks allocated.
    pub(crate) fn write_extent_tree(
        &mut self,
        preferred_group: usize,
        inode: &mut Ext4Inode,
        leaf_extents: &[Ext4Extent],
    ) -> Result<u32> {
        // Depth 0: Extents fit entirely within the inode's 60-byte i_block
        if leaf_extents.len() <= EXT4_EXTENTS_IN_INODE {
            let header = Ext4ExtentHeader {
                eh_magic: EXT4_EH_MAGIC,
                eh_entries: leaf_extents.len() as u16,
                eh_max: EXT4_EXTENTS_IN_INODE as u16,
                eh_depth: 0,
                eh_generation: 0,
            };
            inode.i_block[..12].copy_from_slice(header.as_bytes());
            for (i, ext) in leaf_extents.iter().enumerate() {
                let off = 12 + i * 12;
                inode.i_block[off..off + 12].copy_from_slice(ext.as_bytes());
            }
            return Ok(0);
        }

        // Depth >= 1: Build bottom-up.  A failed allocation releases the
        // tree blocks already taken, so the call allocates all or nothing.
        let mut allocated = Vec::new();
        let mut metadata_blocks_count = 0u32;
        let mut current_indices = Vec::new();

        // Level 0: External leaf blocks containing Ext4Extent
        for chunk in leaf_extents.chunks(EXT4_EXTENTS_PER_BLOCK) {
            let meta_blk = self.allocate_tree_block(preferred_group, &mut allocated)?;
            metadata_blocks_count += 1;

            let mut block_data = vec![0u8; BLOCK_SIZE];
            let header = Ext4ExtentHeader {
                eh_magic: EXT4_EH_MAGIC,
                eh_entries: chunk.len() as u16,
                eh_max: EXT4_EXTENTS_PER_BLOCK as u16,
                eh_depth: 0,
                eh_generation: 0,
            };
            block_data[..12].copy_from_slice(header.as_bytes());
            for (i, ext) in chunk.iter().enumerate() {
                let off = 12 + i * 12;
                block_data[off..off + 12].copy_from_slice(ext.as_bytes());
            }

            let disk_offset = meta_blk as usize * BLOCK_SIZE;
            self.disk_data[disk_offset..disk_offset + BLOCK_SIZE].copy_from_slice(&block_data);

            current_indices.push(Ext4ExtentIdx {
                ei_block: chunk[0].ee_block,
                ei_leaf_lo: meta_blk,
                ei_leaf_hi: 0,
                ei_unused: 0,
            });
        }

        let mut current_depth = 1u16;

        // Level 1+: External index blocks containing Ext4ExtentIdx
        while current_indices.len() > EXT4_EXTENTS_IN_INODE {
            let mut next_level_indices = Vec::new();
            for chunk in current_indices.chunks(EXT4_INDEXES_PER_BLOCK) {
                let meta_blk = self.allocate_tree_block(preferred_group, &mut allocated)?;
                metadata_blocks_count += 1;

                let mut block_data = vec![0u8; BLOCK_SIZE];
                let header = Ext4ExtentHeader {
                    eh_magic: EXT4_EH_MAGIC,
                    eh_entries: chunk.len() as u16,
                    eh_max: EXT4_INDEXES_PER_BLOCK as u16,
                    eh_depth: current_depth,
                    eh_generation: 0,
                };
                block_data[..12].copy_from_slice(header.as_bytes());
                for (i, idx) in chunk.iter().enumerate() {
                    let off = 12 + i * 12;
                    block_data[off..off + 12].copy_from_slice(idx.as_bytes());
                }

                let disk_offset = meta_blk as usize * BLOCK_SIZE;
                self.disk_data[disk_offset..disk_offset + BLOCK_SIZE].copy_from_slice(&block_data);

                next_level_indices.push(Ext4ExtentIdx {
                    ei_block: chunk[0].ei_block,
                    ei_leaf_lo: meta_blk,
                    ei_leaf_hi: 0,
                    ei_unused: 0,
                });
            }
            current_indices = next_level_indices;
            current_depth += 1;
        }

        // Write root index node into inode's i_block
        let header = Ext4ExtentHeader {
            eh_magic: EXT4_EH_MAGIC,
            eh_entries: current_indices.len() as u16,
            eh_max: EXT4_EXTENTS_IN_INODE as u16,
            eh_depth: current_depth,
            eh_generation: 0,
        };
        inode.i_block[..12].copy_from_slice(header.as_bytes());
        for (i, idx) in current_indices.iter().enumerate() {
            let off = 12 + i * 12;
            inode.i_block[off..off + 12].copy_from_slice(idx.as_bytes());
        }

        Ok(metadata_blocks_count)
    }

    /// Returns the block runs an inode owns, following
    /// ext2fs_inode_has_valid_blocks2(): only extent-mapped inodes have data
    /// blocks here.  Fast symlinks and device nodes keep their payload in
    /// `i_block`, which must never be decoded as block pointers.
    pub(crate) fn inode_storage_runs(&self, inode: &Ext4Inode) -> Result<Vec<(u32, u32)>> {
        let mut runs = Vec::new();
        if inode.i_flags & EXT4_EXTENTS_FL != 0 {
            let tree = self.read_extent_tree(inode)?;
            runs.extend(
                tree.leaves
                    .iter()
                    .map(|extent| (extent.ee_start_lo, extent_len(extent)))
                    .filter(|&(_, len)| len != 0),
            );
            runs.extend(tree.tree_blocks.into_iter().map(|block| (block, 1)));
        }
        if inode.i_file_acl_lo != 0 {
            runs.push((inode.i_file_acl_lo, 1));
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::PosixMetadata;
    use crate::ext4::test_support::fresh_builder;

    #[test]
    fn failed_extent_tree_write_releases_its_blocks() -> Result<()> {
        let mut builder = fresh_builder()?;
        // Leave room for one of the two leaf blocks the tree needs.
        while builder.superblock.s_free_blocks_count_lo > 1 {
            builder.allocate_free_blocks(0, 1)?;
        }
        let extents: Vec<Ext4Extent> = (0..=EXT4_EXTENTS_PER_BLOCK as u32)
            .map(|i| Ext4Extent {
                ee_block: i,
                ee_len: 1,
                ee_start_hi: 0,
                ee_start_lo: 100 + 2 * i,
            })
            .collect();

        let mut inode = Ext4Inode::default();
        assert!(builder.write_extent_tree(0, &mut inode, &extents).is_err());
        assert_eq!({ builder.superblock.s_free_blocks_count_lo }, 1);
        Ok(())
    }

    #[test]
    fn multi_extent_tree_depth_1_verification() {
        let mut builder = fresh_builder().expect("failed to create builder");
        let meta = PosixMetadata::default();

        // Artificially fragment free blocks to force >= 5 extents
        // Group 0 has some free blocks. We allocate single blocks with gaps.
        let mut fragmented_blocks = Vec::new();
        for _ in 0..5 {
            let blk = builder.allocate_free_blocks(0, 1).unwrap();
            fragmented_blocks.push(blk);
            let _gap = builder.allocate_free_blocks(0, 1).unwrap();
        }

        // Free the 5 blocks we want to use as fragmented targets
        for &blk in &fragmented_blocks {
            builder.release_block(blk).unwrap();
        }

        // Allocate a 5-block file that will consume the 5 isolated gaps
        let file_data = vec![0xABu8; 5 * BLOCK_SIZE];
        let ino = builder
            .add_file(EXT4_ROOT_INO, "fragmented.bin", &file_data, &meta)
            .expect("should create fragmented file");

        let (grp, idx) = builder.inode_location(ino);
        let inode = &builder.groups[grp].inode_table[idx];

        // Root inode must now be at depth 1 with 1 index entry
        let root_hdr = Ext4ExtentHeader::read_from_bytes(&inode.i_block[..12]).unwrap();

        let root_magic = root_hdr.eh_magic;
        let root_depth = root_hdr.eh_depth;
        let root_entries = root_hdr.eh_entries;
        let root_max = root_hdr.eh_max;
        assert_eq!(root_magic, EXT4_EH_MAGIC);
        assert_eq!(root_depth, 1);
        assert_eq!(root_entries, 1);
        assert_eq!(root_max, 4);

        let root_idx = Ext4ExtentIdx::read_from_bytes(&inode.i_block[12..24]).unwrap();
        let root_index = root_idx.ei_block;
        assert_eq!(root_index, 0);

        // Inspect the external leaf block pointed to by ei_leaf_lo
        let leaf_blk_offset = root_idx.ei_leaf_lo as usize * BLOCK_SIZE;
        let leaf_hdr = Ext4ExtentHeader::read_from_bytes(
            &builder.disk_data[leaf_blk_offset..leaf_blk_offset + 12],
        )
        .unwrap();
        let leaf_magic = leaf_hdr.eh_magic;
        let leaf_depth = leaf_hdr.eh_depth;
        let leaf_entries = leaf_hdr.eh_entries;
        let leaf_max = leaf_hdr.eh_max;
        assert_eq!(leaf_magic, EXT4_EH_MAGIC);
        assert_eq!(leaf_depth, 0);
        assert_eq!(leaf_entries, 5);
        assert_eq!(leaf_max, 340);

        // Accounted sectors: 5 data blocks + 1 extent tree block = 6 blocks * 8 sectors = 48
        let inode_blocks_lo = inode.i_blocks_lo;
        assert_eq!(inode_blocks_lo, 48);
    }
}
