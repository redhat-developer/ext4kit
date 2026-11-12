pub mod dirhash;
pub mod xattr;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ============================================================================
// On-Disk Ext4 Structures (Matching Linux Kernel Specs)
// ============================================================================

pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;
pub const BLOCK_SIZE: usize = 4096;
pub const INODE_SIZE: usize = 256;
pub const BYTES_PER_INODE_RATIO: usize = 4096;
pub const BLOCKS_PER_GROUP: usize = 32768;
pub const EXT4_EXTENTS_IN_INODE: usize = 4;
/// Size reserved by `ext4_extent_tail` when extent-tree metadata checksums are
/// enabled. At a 4 KiB block size it occupies the four bytes left after 340
/// entries, so it does not alter the capacity constants below.
pub const EXT4_EXTENT_TAIL_SIZE: usize = 4;
/// Capacity of an external extent leaf block, with or without its checksum tail.
pub const EXT4_EXTENTS_PER_BLOCK: usize =
    (BLOCK_SIZE - std::mem::size_of::<Ext4ExtentHeader>()) / std::mem::size_of::<Ext4Extent>(); // 340
/// Capacity of an external extent index block, with or without its checksum tail.
pub const EXT4_INDEXES_PER_BLOCK: usize =
    (BLOCK_SIZE - std::mem::size_of::<Ext4ExtentHeader>()) / std::mem::size_of::<Ext4ExtentIdx>(); // 340

// Inode Numbers
pub const EXT4_BAD_INO: u32 = 1;
pub const EXT4_ROOT_INO: u32 = 2;
pub const EXT4_FIRST_INO: u32 = 11;

// File Types
pub const EXT4_FT_UNKNOWN: u8 = 0;
pub const EXT4_FT_REG_FILE: u8 = 1;
pub const EXT4_FT_DIR: u8 = 2;
pub const EXT4_FT_CHRDEV: u8 = 3;
pub const EXT4_FT_BLKDEV: u8 = 4;
pub const EXT4_FT_FIFO: u8 = 5;
pub const EXT4_FT_SOCK: u8 = 6;
pub const EXT4_FT_SYMLINK: u8 = 7;

pub const EXT4_EXTENTS_FL: u32 = 0x80000;

pub const EXT4_EH_MAGIC: u16 = 0xF30A;
pub const EXT4_XATTR_MAGIC: u32 = 0xEA02_0000;
pub const EXT4_XATTR_INDEX_USER: u8 = 1;
pub const EXT4_XATTR_INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const EXT4_XATTR_INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const EXT4_XATTR_INDEX_TRUSTED: u8 = 4;
pub const EXT4_XATTR_INDEX_SECURITY: u8 = 6;
pub const EXT4_XATTR_INDEX_SYSTEM: u8 = 7;

// File Modes (inode.i_mode type masks)
pub const S_IFIFO: u16 = 0x1000; // Named pipe (FIFO)
pub const S_IFCHR: u16 = 0x2000; // Character device
pub const S_IFDIR: u16 = 0x4000; // Directory
pub const S_IFBLK: u16 = 0x6000; // Block device
pub const S_IFREG: u16 = 0x8000; // Regular file
pub const S_IFLNK: u16 = 0xA000; // Symbolic link
pub const S_IFSOCK: u16 = 0xC000; // Socket
pub const S_IFMT: u16 = 0xF000; // File type mask

/// Maximum link count of a non-directory inode (EXT4_LINK_MAX / EXT2_LINK_MAX).
pub const EXT4_LINK_MAX: u16 = 65000;

// Feature Flags
pub const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
pub const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
pub const EXT4_BG_INODE_ZEROED: u16 = 0x0004;
pub const EXT4_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
pub const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
/// The kernel sets this incompat bit while a journal is active (mounted or
/// crashed with replay pending) and clears it on clean unmount and on
/// `fsfreeze`. `ext4_freeze` clears it after flushing the journal, so a
/// frozen image reads it clear.
pub const EXT4_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
pub const EXT4_FEATURE_COMPAT_EXT_ATTR: u32 = 0x0008;
/// `has_journal`: the image carries a jbd2 journal in `s_journal_inum`.
pub const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const EXT4_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub const EXT4_FEATURE_RO_COMPAT_DIR_NLINK: u32 = 0x0020;
/// Metadata checksum features are intentionally unsupported: corresponding
/// superblock feature flags stay clear and checksum fields remain zero.
pub const EXT4_FEATURE_RO_COMPAT_GDT_CSUM: u32 = 0x0010;
pub const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
pub const EXT4_FEATURE_COMPAT_DIR_INDEX: u32 = 0x0020;
pub const EXT4_INDEX_FL: u32 = 0x0000_1000;

// Superblock state and flags
pub const EXT2_VALID_FS: u16 = 0x0001;
pub const EXT2_ERROR_FS: u16 = 0x0002;
/// Byte offset of `s_flags` inside the superblock. The field lies in the
/// part of [`Ext4SuperBlock`] that is kept as opaque padding.
pub const EXT4_SUPERBLOCK_FLAGS_OFFSET: usize = 0x160;
pub const EXT2_FLAGS_UNSIGNED_HASH: u32 = 0x0002;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4SuperBlock {
    pub s_inodes_count: u32,
    pub s_blocks_count_lo: u32,
    pub s_r_blocks_count_lo: u32,
    pub s_free_blocks_count_lo: u32,
    pub s_free_inodes_count: u32,
    pub s_first_data_block: u32, // 0 for 4KB blocks
    pub s_log_block_size: u32,   // 2 for 4KB (1024 << 2)
    pub s_log_cluster_size: u32, // 2
    pub s_blocks_per_group: u32,
    pub s_clusters_per_group: u32,
    pub s_inodes_per_group: u32,
    pub s_mtime: u32,
    pub s_wtime: u32,
    pub s_mnt_count: u16,
    pub s_max_mnt_count: u16,
    pub s_magic: u16, // 0xEF53
    pub s_state: u16,
    pub s_errors: u16,
    pub s_minor_rev_level: u16,
    pub s_lastcheck: u32,
    pub s_checkinterval: u32,
    pub s_creator_os: u32, // 0 = Linux
    pub s_rev_level: u32,  // 1 = Dynamic revision
    pub s_def_resuid: u16,
    pub s_def_resgid: u16,
    // Revision 1 fields
    pub s_first_ino: u32,  // 11
    pub s_inode_size: u16, // 256
    pub s_block_group_nr: u16,
    pub s_feature_compat: u32,
    pub s_feature_incompat: u32,
    pub s_feature_ro_compat: u32,
    pub s_uuid: [u8; 16],
    pub s_volume_name: [u8; 16],
    pub s_last_mounted: [u8; 64],
    pub s_algorithm_usage_bitmap: u32,
    // Keep these intervening revision-one fields so the dx hash seed and
    // version land at the ext4-specified offsets (236 and 252).
    pub _s_prealloc_blocks: u8,
    pub _s_prealloc_dir_blocks: u8,
    pub _s_reserved_gdt_blocks: u16,
    pub _s_journal_uuid: [u8; 16],
    pub _s_journal_inum: u32,
    pub _s_journal_dev: u32,
    pub _s_last_orphan: u32,
    pub s_hash_seed: [u32; 4], // 16-byte htree hash seed
    pub s_def_hash_version: u8,
    pub _s_jnl_backup_type: u8,
    pub _s_desc_size: u16,
    pub _padding: [u8; 768],
}

impl Default for Ext4SuperBlock {
    fn default() -> Self {
        Self {
            s_inodes_count: 0,
            s_blocks_count_lo: 0,
            s_r_blocks_count_lo: 0,
            s_free_blocks_count_lo: 0,
            s_free_inodes_count: 0,
            s_first_data_block: 0, // 0 for 4KB blocks (1 for 1KB blocks)
            s_log_block_size: 2,   // 2 = 4096 bytes (1024 << 2)
            s_log_cluster_size: 2, // Must match s_log_block_size (unless bigalloc)
            s_blocks_per_group: BLOCKS_PER_GROUP as u32, // Standard max blocks per group for 4KB blocks
            s_clusters_per_group: BLOCKS_PER_GROUP as u32,
            s_inodes_per_group: 0, // Dynamically calculated during geometry phase
            s_mtime: 0,
            s_wtime: 0,
            s_mnt_count: 0,
            s_max_mnt_count: 0xFFFF,   // -1 (disabled mount count check)
            s_magic: EXT4_SUPER_MAGIC, // EXT4 Magic Number
            s_state: 1,                // EXT2_VALID_FS (Clean)
            s_errors: 1,               // EXT2_ERRORS_CONTINUE
            s_minor_rev_level: 0,
            s_lastcheck: 0,
            s_checkinterval: 0,
            s_creator_os: 0, // 0 = Linux
            s_rev_level: 1,  // 1 = Dynamic revision
            s_def_resuid: 0,
            s_def_resgid: 0,
            s_first_ino: 11,   // EXT4_FIRST_INO
            s_inode_size: 256, // Standard ext4 inode size
            s_block_group_nr: 0,
            // External xattr blocks are referenced through i_file_acl_lo.
            s_feature_compat: EXT4_FEATURE_COMPAT_EXT_ATTR | EXT4_FEATURE_COMPAT_DIR_INDEX,
            s_feature_incompat: EXT4_FEATURE_INCOMPAT_FILETYPE | EXT4_FEATURE_INCOMPAT_EXTENTS,
            // The allocator below places backups only in sparse-super groups.
            // Keep GDT_CSUM and METADATA_CSUM clear: this writer does not
            // calculate group-descriptor, bitmap, or extent-block checksums.
            // dir_nlink matches mke2fs's ext4 defaults and lets directories
            // with more than EXT4_LINK_MAX subdirectories keep a link count of 1.
            s_feature_ro_compat: EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER
                | EXT4_FEATURE_RO_COMPAT_DIR_NLINK,
            s_uuid: [0; 16],
            s_volume_name: [0; 16],
            s_last_mounted: [0; 64],
            s_algorithm_usage_bitmap: 0,
            _s_prealloc_blocks: 0,
            _s_prealloc_dir_blocks: 0,
            _s_reserved_gdt_blocks: 0,
            _s_journal_uuid: [0; 16],
            _s_journal_inum: 0,
            _s_journal_dev: 0,
            _s_last_orphan: 0,
            s_hash_seed: [0x8a1f_3b4c, 0x5e92_10d8, 0x4f7c_2b90, 0x61e3_8a5d],
            s_def_hash_version: dirhash::DX_HASH_HALF_MD4,
            _s_jnl_backup_type: 0,
            _s_desc_size: 0,
            _padding: [0; 768],
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4GroupDesc {
    pub bg_block_bitmap_lo: u32,
    pub bg_inode_bitmap_lo: u32,
    pub bg_inode_table_lo: u32,
    pub bg_free_blocks_count_lo: u16,
    pub bg_free_inodes_count_lo: u16,
    pub bg_used_dirs_count_lo: u16,
    pub bg_flags: u16,
    pub bg_exclude_bitmap_lo: u32,
    pub bg_block_bitmap_csum_lo: u16,
    pub bg_inode_bitmap_csum_lo: u16,
    pub bg_itable_unused_lo: u16,
    pub bg_checksum: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
// https://github.com/torvalds/linux/blob/master/fs/ext4/ext4.h#L794
pub struct Ext4Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size_lo: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks_lo: u32, // 512-byte sector count
    pub i_flags: u32,     // 0x80000 = EXT4_EXTENTS_FL
    pub i_osd1: u32,
    pub i_block: [u8; 60], // Extent tree root or block pointers
    pub i_generation: u32,
    pub i_file_acl_lo: u32,
    pub i_size_high: u32,
    pub i_obso_faddr: u32,
    pub i_osd2: [u8; 12], // Completes standard 128-byte base inode
    // Extended Inode Fields (for 256-byte inode size)
    pub i_extra_isize: u16, // Typically set to 32 (size of extra fields)
    pub i_checksum_hi: u16,
    pub i_ctime_extra: u32,
    pub i_mtime_extra: u32,
    pub i_atime_extra: u32,
    pub i_crtime: u32, // File creation time
    pub i_crtime_extra: u32,
    pub i_version_hi: u32,
    pub i_projid: u32,
    pub _padding: [u8; 96], // 128 + 32 + 96 = 256 bytes total
}

impl Default for Ext4Inode {
    fn default() -> Self {
        Self {
            i_mode: 0,
            i_uid: 0,
            i_size_lo: 0,
            i_atime: 0,
            i_ctime: 0,
            i_mtime: 0,
            i_dtime: 0,
            i_gid: 0,
            i_links_count: 0,
            i_blocks_lo: 0,
            i_flags: 0,
            i_osd1: 0,
            i_block: [0; 60],
            i_generation: 0,
            i_file_acl_lo: 0,
            i_size_high: 0,
            i_obso_faddr: 0,
            i_osd2: [0; 12],
            i_extra_isize: 0,
            i_checksum_hi: 0,
            i_ctime_extra: 0,
            i_mtime_extra: 0,
            i_atime_extra: 0,
            i_crtime: 0,
            i_crtime_extra: 0,
            i_version_hi: 0,
            i_projid: 0,
            _padding: [0; 96],
        }
    }
}

impl Ext4Inode {
    /// Returns the `S_IF*` file-type bits of `i_mode`.
    pub fn file_type(&self) -> u16 {
        self.i_mode & S_IFMT
    }

    /// Returns whether the inode is a directory.  Comparing the full type
    /// field matters: block devices and sockets also carry the S_IFDIR bit.
    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_default_is_a_zeroed_on_disk_inode() {
        assert!(
            Ext4Inode::default()
                .as_bytes()
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn checksum_features_are_not_advertised_without_checksum_support() {
        let features = Ext4SuperBlock::default().s_feature_ro_compat;
        assert_eq!(features & EXT4_FEATURE_RO_COMPAT_GDT_CSUM, 0);
        assert_eq!(features & EXT4_FEATURE_RO_COMPAT_METADATA_CSUM, 0);
    }

    #[test]
    fn extent_capacities_leave_exactly_room_for_a_checksum_tail() {
        assert_eq!(EXT4_EXTENTS_PER_BLOCK, 340);
        assert_eq!(EXT4_INDEXES_PER_BLOCK, 340);
        assert!(
            std::mem::size_of::<Ext4ExtentHeader>()
                + EXT4_EXTENTS_PER_BLOCK * std::mem::size_of::<Ext4Extent>()
                <= BLOCK_SIZE
        );
        assert_eq!(
            std::mem::size_of::<Ext4ExtentHeader>()
                + EXT4_EXTENTS_PER_BLOCK * std::mem::size_of::<Ext4Extent>()
                + EXT4_EXTENT_TAIL_SIZE,
            BLOCK_SIZE
        );
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4ExtentIdx {
    pub ei_block: u32,   // Subtree covers logical blocks starting from here
    pub ei_leaf_lo: u32, // Physical block number of child node
    pub ei_leaf_hi: u16, // High 16 bits of child block number
    pub ei_unused: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4ExtentHeader {
    pub eh_magic: u16,   // 0xF30A
    pub eh_entries: u16, // Number of valid entries
    pub eh_max: u16,     // Max capacity
    pub eh_depth: u16,   // 0 for leaf nodes
    pub eh_generation: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4Extent {
    pub ee_block: u32,    // First logical block
    pub ee_len: u16,      // Number of blocks
    pub ee_start_hi: u16, // High 16 bits of physical block (0 for <16TB)
    pub ee_start_lo: u32, // Low 32 bits of physical block
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4DirEntry2Header {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8, // 1 = file, 2 = directory
}

/// `struct ext4_xattr_header`, used at the start of an external xattr block.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4XattrHeader {
    pub h_magic: u32,
    pub h_refcount: u32,
    pub h_blocks: u32,
    pub h_hash: u32,
    pub h_checksum: u32,
    pub h_reserved: [u32; 3],
}

/// `struct ext4_xattr_entry`; the un-terminated name follows this header.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4XattrEntry {
    pub e_name_len: u8,
    pub e_name_index: u8,
    pub e_value_offs: u16,
    pub e_value_block: u32,
    pub e_value_size: u32,
    pub e_hash: u32,
}

// Represents a single 128MB Section
pub struct Ext4BlockGroup {
    pub group_id: u32,
    pub blocks_count: u32,
    pub block_bitmap: Vec<u8>,
    pub inode_bitmap: Vec<u8>,
    pub inode_table: Vec<Ext4Inode>,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4DxRootInfo {
    pub reserved_zero: u32,
    pub hash_version: u8,
    pub info_length: u8,
    pub indirect_levels: u8,
    pub unused_flags: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4DxCountlimit {
    pub limit: u16,
    pub count: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct Ext4DxEntry {
    pub hash: u32,
    pub block: u32, // Logical block index inside directory
}
