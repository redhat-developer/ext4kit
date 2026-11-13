//! Read-only, positional-I/O access to an ext4 image.
//!
//! [`ImageReader`] exists separately from [`Ext4ImageBuilder`](super::Ext4ImageBuilder)
//! because the snapshot feature must read an image that a VM still has
//! mounted: memory-mapping a file another process writes is undefined
//! behavior under memmap2's contract, so this reader uses positional reads
//! only, and it never loads the block bitmaps or rewrites anything. It is a
//! small surface meant to be hardened, audited and fuzzed as a unit.
//!
//! Everything read from the image is hostile input (the agent inside the VM
//! is root and can write raw blocks), so every on-disk index — block
//! numbers, extent starts and lengths, inode numbers, `rec_len`, name
//! lengths — is bounds-checked through [`ImageReader::read_bytes`], extent
//! depth is capped, and directory walks are capped at the inode's own block
//! count. Malformed structures are errors, never panics.

use super::extent::{EXT_INIT_MAX_LEN, ExtentTree, extent_len, verify_extent_header};
use super::file::file_size;
use super::open::{EXPECTED_COMPAT, EXPECTED_INCOMPAT, validate_geometry, validate_layout};
use crate::ext4::format::*;
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::Write,
    path::Path,
    sync::Mutex,
};
use zerocopy::FromBytes;

/// The jbd2 magic and superblock block type. Unlike every ext4 field, all
/// jbd2 fields are big-endian.
pub(crate) const JBD2_MAGIC: u32 = 0xC03B_3998;
pub(crate) const JBD2_SUPERBLOCK_V2: u32 = 4;

/// Container-format magics that must not be mistaken for a raw image. ext4kit
/// zeroes the boot block, so a raw-block writer can plant these to produce a
/// misleading error; rejecting them up front keeps the message honest.
const QCOW2_MAGIC: &[u8; 4] = b"QFI\xfb";
const VHDX_MAGIC: &[u8; 8] = b"vhdxfile";
const VHD_MAGIC: &[u8; 8] = b"conectix";

/// Maximum extent tree depth accepted by e2fsprogs and the kernel.
const MAX_EXTENT_DEPTH: u16 = 5;

/// Inode-table blocks kept in the reader's cache.
const INODE_CACHE_BLOCKS: usize = 16;

/// What [`ImageReader::open`] and [`ImageReader::recheck_state`] report about
/// the image's on-disk state.
///
/// `mounted` is [`EXT4_FEATURE_INCOMPAT_RECOVER`] set on a journaled image
/// (the kernel keeps `EXT2_VALID_FS` set while a journal is active, so that
/// bit says nothing there) and `EXT2_VALID_FS` clear on a journal-less one.
/// `RECOVER` clear while the VM runs means frozen: `ext4_freeze` clears it
/// after flushing the journal, and `ext4_unfreeze` sets it again before the
/// jbd2 writers wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageState {
    pub mounted: bool,
    pub has_journal: bool,
    /// `true` on a journal-less image. On a journaled one it means the
    /// journal superblock reports `s_start == 0`: no unreplayed
    /// transactions. It proves consistency, not frozenness.
    pub journal_empty: bool,
    pub errors_flagged: bool,
    pub warnings: Vec<String>,
}

/// The jbd2 journal superblock: the first block of the journal inode. Only
/// the fields the emptiness check needs are decoded.
pub(crate) struct JournalSuperblock {
    /// `s_start`: the journal block of the first unprocessed transaction.
    /// Zero means the log is empty (fresh, frozen, or cleanly unmounted).
    start: u32,
}

impl JournalSuperblock {
    /// Parses and validates a journal superblock. `bytes` must cover at
    /// least `0x20` bytes.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let field = |offset: usize| -> Result<u32> {
            let bytes = bytes
                .get(offset..offset + 4)
                .ok_or_else(|| anyhow!("journal superblock is truncated"))?;
            Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
        };
        if field(0)? != JBD2_MAGIC {
            bail!("journal superblock has a bad magic");
        }
        if field(4)? != JBD2_SUPERBLOCK_V2 {
            bail!("journal superblock is not a v2 superblock");
        }
        // s_start sits at 0x1C: journal_header_t is 12 bytes (magic,
        // blocktype, h_sequence) followed by s_blocksize, s_maxlen, s_first
        // and s_sequence. Reading it as 0x18 (an 8-byte header) silently
        // decodes s_sequence instead, which makes the emptiness check
        // always pass.
        Ok(Self {
            start: field(0x1C)?,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == 0
    }
}

/// Where the reader's bytes come from: a file read positionally (the normal
/// case, safe against a concurrent writer), or an in-memory buffer (tests
/// and the fuzz target).
enum Backing {
    File { file: File, length: u64 },
    Memory(Vec<u8>),
}

impl Backing {
    fn len(&self) -> u64 {
        match self {
            Self::File { length, .. } => *length,
            Self::Memory(data) => data.len() as u64,
        }
    }

    /// Reads exactly `buf.len()` bytes at `offset`. Positional, so reads do
    /// not disturb (or depend on) any file pointer, and the same reader can
    /// be used while another process writes the file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        match self {
            Self::Memory(data) => {
                let end = offset
                    .checked_add(buf.len() as u64)
                    .ok_or_else(|| std::io::Error::other("read offset overflows"))?;
                let slice = data.get(offset as usize..end as usize).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "read past the buffer")
                })?;
                buf.copy_from_slice(slice);
                Ok(())
            }
            Self::File { file, .. } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.read_exact_at(buf, offset)
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    let mut filled = 0;
                    while filled < buf.len() {
                        let read = file.seek_read(&mut buf[filled..], offset + filled as u64)?;
                        if read == 0 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "read past the end of the image",
                            ));
                        }
                        filled += read;
                    }
                    Ok(())
                }
            }
        }
    }
}

/// Caches recently read 4 KiB inode-table blocks (up to [`INODE_CACHE_BLOCKS`])
/// so walking directories doesn't repeat positional syscalls for neighboring
/// inodes sharing the same block.
/// [`ImageReader::read_inode_fresh`] bypasses this cache: on
/// a live image the cached copy could be stale.
struct InodeBlockCache {
    blocks: HashMap<u64, Box<[u8; BLOCK_SIZE]>>,
    order: VecDeque<u64>,
}

impl InodeBlockCache {
    fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(
        &mut self,
        offset: u64,
        fill: impl FnOnce(&mut [u8; BLOCK_SIZE]) -> Result<()>,
    ) -> Result<Box<[u8; BLOCK_SIZE]>> {
        if let Some(block) = self.blocks.get(&offset) {
            // Refresh the LRU order on a hit.
            if let Some(position) = self.order.iter().position(|&key| key == offset) {
                self.order.remove(position);
                self.order.push_back(offset);
            }
            return Ok(block.clone());
        }
        let mut block: Box<[u8; BLOCK_SIZE]> = Box::new([0u8; BLOCK_SIZE]);
        fill(&mut block)?;
        if self.blocks.len() == INODE_CACHE_BLOCKS
            && let Some(evicted) = self.order.pop_front()
        {
            self.blocks.remove(&evicted);
        }
        self.order.push_back(offset);
        self.blocks.insert(offset, block.clone());
        Ok(block)
    }
}

/// Read-only positional access to an ext4 image, for inspection and for the
/// snapshot scan. Never loads bitmaps, never writes, never memory-maps.
pub struct ImageReader {
    backing: Backing,
    superblock: Ext4SuperBlock,
    /// `bg_inode_table_lo` per group, validated at open.
    inode_table_start: Vec<u32>,
    has_journal: bool,
    inode_blocks: Mutex<InodeBlockCache>,
}

impl ImageReader {
    /// Opens an image for reading, whether the VM runs or not (a stopped VM
    /// is the easy case). Returns the reader and its view of the on-disk
    /// state; see [`ImageState`].
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, ImageState)> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|error| {
            // ERROR_SHARING_VIOLATION: a process holds the image without
            // read sharing, which for this tool means the hypervisor.
            #[cfg(windows)]
            let is_sharing_violation = error.raw_os_error() == Some(32);
            #[cfg(not(windows))]
            let is_sharing_violation = false;
            if is_sharing_violation {
                anyhow!(
                    "{} is held by another process without read sharing; \
                     ext4kit needs FILE_SHARE_READ to read an image attached \
                     to a running VM",
                    path.display()
                )
            } else {
                anyhow!("cannot open {}: {error}", path.display())
            }
        })?;
        let length = file.metadata()?.len();
        Self::from_backing(Backing::File { file, length })
    }

    /// Opens an in-memory image; the same validation as [`ImageReader::open`].
    /// Tests and the fuzz target use this to feed crafted bytes directly.
    pub fn from_memory(data: Vec<u8>) -> Result<(Self, ImageState)> {
        Self::from_backing(Backing::Memory(data))
    }

    fn from_backing(backing: Backing) -> Result<(Self, ImageState)> {
        if backing.len() < 2 * 1024 {
            bail!("the image is too small to be an ext4 image");
        }
        let boot = read_exact(&backing, 0, 8)?;
        if boot.starts_with(QCOW2_MAGIC) {
            bail!("the file is a qcow2 image; ext4kit reads the raw image libkrun attaches");
        }
        if boot.starts_with(VHDX_MAGIC) || boot.starts_with(VHD_MAGIC) {
            bail!("the file is a VHD/VHDX image; ext4kit reads the raw image libkrun attaches");
        }

        let superblock_bytes = read_exact(&backing, 1024, std::mem::size_of::<Ext4SuperBlock>())?;
        let superblock =
            Ext4SuperBlock::read_from_bytes(&superblock_bytes).expect("the slice matches the size");
        if superblock.s_magic != EXT4_SUPER_MAGIC {
            bail!("not an ext4 image: bad superblock magic");
        }
        validate_layout(&superblock)?;
        let (groups_number, inodes_per_group) = validate_geometry(&superblock)?;

        // The reader tolerates the states of a running VM but still refuses
        // features that change how structures are read.
        let (compat, incompat) = (superblock.s_feature_compat, superblock.s_feature_incompat);
        let has_journal = compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL != 0;
        let needs_recovery = incompat & EXT4_FEATURE_INCOMPAT_RECOVER != 0;
        if compat & !EXT4_FEATURE_COMPAT_HAS_JOURNAL != EXPECTED_COMPAT
            || incompat & !EXT4_FEATURE_INCOMPAT_RECOVER != EXPECTED_INCOMPAT
            || (needs_recovery && !has_journal)
        {
            bail!(
                "unsupported ext4 features (compat {compat:#x}, incompat {incompat:#x}): \
                 only raw images created by ext4kit can be read"
            );
        }
        // Extra ro_compat bits are accepted: they only constrain writers.
        // The dx hash is computed with signed chars; an image flagged for
        // unsigned hashing would get index entries the kernel cannot find.
        let flags = read_exact(&backing, 1024 + EXT4_SUPERBLOCK_FLAGS_OFFSET as u64, 4)?;
        if u32::from_le_bytes(flags.try_into().unwrap()) & EXT2_FLAGS_UNSIGNED_HASH != 0 {
            bail!("unsupported ext4 image: directory hashes use unsigned characters");
        }

        // The file may be larger than the filesystem, never smaller.
        let fs_bytes = u64::from(superblock.s_blocks_count_lo) * BLOCK_SIZE as u64;
        if backing.len() < fs_bytes {
            bail!(
                "image is truncated: the superblock describes {fs_bytes} bytes, but only {} are present",
                backing.len()
            );
        }

        let descriptor_size = std::mem::size_of::<Ext4GroupDesc>();
        let descriptor_bytes =
            read_exact(&backing, BLOCK_SIZE as u64, groups_number * descriptor_size)?;
        let inode_table_blocks = (inodes_per_group * INODE_SIZE).div_ceil(BLOCK_SIZE);
        let mut inode_table_start = Vec::with_capacity(groups_number);
        for (index, chunk) in descriptor_bytes.chunks_exact(descriptor_size).enumerate() {
            let descriptor = Ext4GroupDesc::read_from_bytes(chunk).expect("chunks match the size");
            let start = u64::from(descriptor.bg_inode_table_lo);
            if start == 0 || start + inode_table_blocks as u64 > superblock.s_blocks_count_lo.into()
            {
                bail!("block group {index} inode table lies outside the filesystem");
            }
            inode_table_start.push(descriptor.bg_inode_table_lo);
        }

        let reader = Self {
            backing,
            superblock,
            inode_table_start,
            has_journal,
            inode_blocks: Mutex::new(InodeBlockCache::new()),
        };
        let state = reader.derive_state()?;
        Ok((reader, state))
    }

    /// Re-reads the primary superblock and the journal superblock with fresh
    /// positional reads and returns the current state. The publication gates
    /// call this between the last content read and the ref update.
    pub fn recheck_state(&self) -> Result<ImageState> {
        self.derive_state()
    }

    fn derive_state(&self) -> Result<ImageState> {
        let superblock_bytes =
            read_exact(&self.backing, 1024, std::mem::size_of::<Ext4SuperBlock>())?;
        let superblock =
            Ext4SuperBlock::read_from_bytes(&superblock_bytes).expect("the slice matches the size");
        let state = superblock.s_state;
        let mounted = if self.has_journal {
            superblock.s_feature_incompat & EXT4_FEATURE_INCOMPAT_RECOVER != 0
        } else {
            state & EXT2_VALID_FS == 0
        };
        let mut warnings = Vec::new();
        let errors_flagged = state & EXT2_ERROR_FS != 0;
        if errors_flagged {
            warnings.push(
                "the filesystem has errors flagged; run e2fsck -f once the VM is stopped"
                    .to_owned(),
            );
        }
        let journal_empty = self.journal_empty()?;
        Ok(ImageState {
            mounted,
            has_journal: self.has_journal,
            journal_empty,
            errors_flagged,
            warnings,
        })
    }

    /// Reads the journal superblock and reports whether the log is empty.
    /// The reader never replays a journal.
    fn journal_empty(&self) -> Result<bool> {
        if !self.has_journal {
            return Ok(true);
        }
        let journal_inum = self.superblock._s_journal_inum;
        if journal_inum == 0 || journal_inum > self.superblock.s_inodes_count {
            bail!("has_journal is set but the journal inode number is invalid");
        }
        let inode = self.read_inode_fresh(journal_inum)?;
        if inode.i_flags & EXT4_EXTENTS_FL == 0 {
            bail!("journal inode {journal_inum} does not use extents");
        }
        let tree = self.read_extent_tree(&inode)?;
        let block = self.extent_at_block_zero(&tree.leaves)?;
        let bytes = self.read_bytes(block * BLOCK_SIZE as u64, BLOCK_SIZE)?;
        Ok(JournalSuperblock::parse(&bytes)?.is_empty())
    }

    /// The physical start of the extent covering logical block 0, validated
    /// against the filesystem size. `leaves.first()` is not enough: a
    /// crafted tree can order or place its extents arbitrarily, and both
    /// the journal superblock and slow symlinks live at logical block 0.
    fn extent_at_block_zero(&self, leaves: &[Ext4Extent]) -> Result<u64> {
        let extent = leaves
            .iter()
            .find(|extent| extent.ee_block == 0)
            .ok_or_else(|| anyhow!("the extent tree has no extent at logical block 0"))?;
        let start = u64::from(extent.ee_start_lo) | (u64::from(extent.ee_start_hi) << 32);
        if start >= u64::from(self.superblock.s_blocks_count_lo) {
            bail!("the extent at logical block 0 starts outside the filesystem");
        }
        Ok(start)
    }

    /// The primary superblock as read at open.
    pub fn superblock(&self) -> &Ext4SuperBlock {
        &self.superblock
    }

    /// The filesystem UUID from the superblock.
    pub fn fs_uuid(&self) -> [u8; 16] {
        self.superblock.s_uuid
    }

    /// Reads `len` bytes at `offset`, bounds-checked against the file. This
    /// is the one accessor every on-disk index funnels through: an
    /// out-of-range read is an error, never a panic or a short buffer.
    pub fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        read_exact(&self.backing, offset, len)
    }

    /// Returns the inode-table record for `ino` through the block cache.
    /// For an inode whose bytes must be current (the snapshot framing
    /// invariant), use [`ImageReader::read_inode_fresh`].
    pub fn read_inode(&self, ino: u32) -> Result<Ext4Inode> {
        let offset = self.inode_offset(ino)?;
        let block_offset = offset & !(BLOCK_SIZE as u64 - 1);
        let mut cache = self
            .inode_blocks
            .lock()
            .expect("another thread panicked while holding the inode cache");
        let block = cache.get(block_offset, |block| {
            self.backing
                .read_at(block, block_offset)
                .map_err(|error| anyhow!("cannot read the inode table: {error}"))
        })?;
        let within = (offset - block_offset) as usize;
        let bytes = block
            .get(within..within + INODE_SIZE)
            .ok_or_else(|| anyhow!("inode {ino} straddles an inode-table block"))?;
        Ok(Ext4Inode::read_from_bytes(bytes).expect("INODE_SIZE slices always parse"))
    }

    /// Returns the inode-table record for `ino` with a direct positional
    /// read, bypassing the cache: the snapshot framing invariant reads the
    /// inode exactly once and streams its extents from that read.
    pub fn read_inode_fresh(&self, ino: u32) -> Result<Ext4Inode> {
        let offset = self.inode_offset(ino)?;
        let bytes = self.read_bytes(offset, INODE_SIZE)?;
        Ok(Ext4Inode::read_from_bytes(&bytes).expect("INODE_SIZE slices always parse"))
    }

    /// The byte offset of `ino` inside its inode table, after validating the
    /// number against the superblock's inode count.
    fn inode_offset(&self, ino: u32) -> Result<u64> {
        if ino == 0 || ino > self.superblock.s_inodes_count {
            bail!("inode {ino} does not exist");
        }
        let inodes_per_group = u64::from(self.superblock.s_inodes_per_group);
        let group = (u64::from(ino) - 1) / inodes_per_group;
        let index = (u64::from(ino) - 1) % inodes_per_group;
        Ok(
            u64::from(self.inode_table_start[group as usize]) * BLOCK_SIZE as u64
                + index * INODE_SIZE as u64,
        )
    }

    /// Decodes the extent tree rooted in `inode.i_block`, reading child
    /// nodes positionally, in on-disk order (the builder's recursive
    /// `walk_extent_node` order): index children are gathered per node and
    /// pushed onto the stack reversed, so popping yields them left to right
    /// and `read_file_from`'s sorted-extents check sees the order the kernel
    /// produces. Every child block is validated against the filesystem
    /// size, the depth is capped and must strictly decrease, a node may be
    /// referenced only once, and the walk is bounded by the inode's own
    /// block count, so a crafted tree cannot cycle, read outside the image
    /// or exhaust memory.
    fn read_extent_tree(&self, inode: &Ext4Inode) -> Result<ExtentTree> {
        let fs_blocks = u64::from(self.superblock.s_blocks_count_lo);
        // Every leaf and tree node consumes at least one of the inode's own
        // blocks (i_blocks counts data, tree and xattr blocks alike), so it
        // bounds the walk: a crafted tree cannot make the reader
        // materialize more entries than the kernel ever allocated. The
        // count is clamped to the filesystem size: i_blocks is read from
        // the image, and an agent-crafted inode can claim any amount.
        let owned = owned_blocks(inode).min(fs_blocks);
        let mut tree = ExtentTree::default();
        let mut visited: HashSet<u64> = HashSet::new();
        let mut stack: Vec<(Vec<u8>, Option<u16>)> = vec![(inode.i_block.to_vec(), None)];
        while let Some((node, expected_depth)) = stack.pop() {
            let header = verify_extent_header(&node)?;
            let depth = header.eh_depth;
            if depth > MAX_EXTENT_DEPTH || expected_depth.is_some_and(|d| d != depth) {
                bail!("invalid extent tree depth {depth}");
            }
            let mut children = Vec::new();
            for i in 0..header.eh_entries as usize {
                let entry = node
                    .get(12 + i * 12..24 + i * 12)
                    .ok_or_else(|| anyhow!("extent entry {i} is truncated"))?;
                if depth == 0 {
                    tree.leaves.push(
                        Ext4Extent::read_from_bytes(entry)
                            .map_err(|_| anyhow!("invalid extent entry"))?,
                    );
                } else {
                    let idx = Ext4ExtentIdx::read_from_bytes(entry)
                        .map_err(|_| anyhow!("invalid extent index"))?;
                    let block = u64::from(idx.ei_leaf_lo) | (u64::from(idx.ei_leaf_hi) << 32);
                    if block == 0 || block >= fs_blocks {
                        bail!("extent block {block} is outside the filesystem");
                    }
                    // The kernel never points two index entries at the same
                    // node; unrepeated references would let a crafted tree
                    // amplify the walk without limit.
                    if !visited.insert(block) {
                        bail!("extent tree references block {block} more than once");
                    }
                    tree.tree_blocks.push(idx.ei_leaf_lo);
                    children.push((block, depth));
                }
                if tree.leaves.len() + tree.tree_blocks.len() > owned as usize {
                    bail!(
                        "the extent tree maps more blocks than its inode owns \
                         ({} entries, {} owned)",
                        tree.leaves.len() + tree.tree_blocks.len(),
                        owned
                    );
                }
            }
            // Push reversed so the stack pops the children in on-disk order.
            for (block, depth) in children.into_iter().rev() {
                let child = self.read_bytes(block * BLOCK_SIZE as u64, BLOCK_SIZE)?;
                stack.push((child, Some(depth - 1)));
            }
        }
        Ok(tree)
    }

    /// Lists the children of directory `dir` as `(raw name bytes, inode)`,
    /// excluding `.` and `..` and deleted records. Names are returned as
    /// bytes: a name that is not valid UTF-8 is returned as-is instead of
    /// failing the listing (the tracked layer classifies it).
    ///
    /// Every block of an indexed directory is scanned, like
    /// [`Ext4ImageBuilder::directory_entries`]: dx root and node blocks
    /// parse as ordinary blocks whose only live records are `.` and `..`.
    pub fn directory_entries_raw(&self, dir: u32) -> Result<Vec<(Vec<u8>, u32)>> {
        let inode = self.read_inode(dir)?;
        if !inode.is_dir() {
            bail!("inode {dir} is not a directory");
        }
        if inode.i_flags & EXT4_EXTENTS_FL == 0 {
            bail!("directory inode {dir} does not use extents");
        }
        let mut children = Vec::new();
        for block in self.directory_blocks(dir, &inode)? {
            let bytes = self.read_bytes(u64::from(block) * BLOCK_SIZE as u64, BLOCK_SIZE)?;
            children.extend(parse_dir_block(&bytes)?);
        }
        Ok(children)
    }

    /// A directory's data blocks in logical order, with the walk capped at
    /// the inode's own block count (`i_blocks_lo` counts 512-byte sectors):
    /// a crafted directory cannot make the reader scan more blocks than the
    /// kernel ever allocated for it.
    fn directory_blocks(&self, dir: u32, inode: &Ext4Inode) -> Result<Vec<u32>> {
        let extents = self.read_extent_tree(inode)?.leaves;
        let fs_blocks = u64::from(self.superblock.s_blocks_count_lo);
        let mut blocks = Vec::new();
        let mut mapped = 0u64;
        for extent in extents {
            let start = u64::from(extent.ee_start_lo) | (u64::from(extent.ee_start_hi) << 32);
            let len = u64::from(extent_len(&extent));
            if start + len > fs_blocks {
                bail!("directory inode {dir} maps blocks outside the filesystem");
            }
            mapped += len;
            blocks.extend((0..len).map(|b| extent.ee_start_lo + b as u32));
        }
        let owned = u64::from(inode.i_blocks_lo) / (BLOCK_SIZE as u64 / 512);
        if mapped > owned {
            bail!(
                "directory inode {dir} maps {mapped} blocks but owns only {owned}; \
                 the image is corrupt"
            );
        }
        Ok(blocks)
    }

    /// Copies the contents of the regular file described by `inode` to
    /// `writer` and returns the byte count written, which is the file size.
    /// Holes and unwritten extents read as zeros. The extents read belong
    /// to the same inode snapshot the caller holds, so the size framing the
    /// stream and the data come from one read of the inode (the snapshot
    /// framing invariant). The number of bytes written is the inode's
    /// `i_size`; bounding it against a policy limit is the caller's job.
    pub fn read_file_from(&self, inode: &Ext4Inode, writer: &mut impl Write) -> Result<u64> {
        if inode.file_type() != S_IFREG {
            bail!("the inode is not a regular file");
        }
        if inode.i_flags & EXT4_EXTENTS_FL == 0 {
            bail!("the file inode does not use extents");
        }
        let size = file_size(inode);
        let zeros = [0u8; BLOCK_SIZE];
        let write_zeros = |writer: &mut dyn Write, mut count: u64| -> std::io::Result<()> {
            while count > 0 {
                let chunk = count.min(BLOCK_SIZE as u64) as usize;
                writer.write_all(&zeros[..chunk])?;
                count -= chunk as u64;
            }
            Ok(())
        };
        let extents = self.read_extent_tree(inode)?.leaves;
        let fs_blocks = u64::from(self.superblock.s_blocks_count_lo);
        let mut written = 0u64;
        for extent in extents {
            let logical = u64::from(extent.ee_block) * BLOCK_SIZE as u64;
            if logical < written {
                bail!("the file has overlapping or unsorted extents");
            }
            if logical >= size {
                break;
            }
            write_zeros(writer, logical - written)
                .map_err(|error| anyhow!("cannot write file holes: {error}"))?;
            let length = (u64::from(extent_len(&extent)) * BLOCK_SIZE as u64).min(size - logical);
            if extent.ee_len > EXT_INIT_MAX_LEN {
                // An unwritten extent: the kernel reads it as zeros.
                write_zeros(writer, length)
                    .map_err(|error| anyhow!("cannot write file holes: {error}"))?;
            } else {
                let start = u64::from(extent.ee_start_lo) | (u64::from(extent.ee_start_hi) << 32);
                if start + length.div_ceil(BLOCK_SIZE as u64) > fs_blocks {
                    bail!("the file maps blocks outside the filesystem");
                }
                // Stream in chunks instead of materializing an extent (up to
                // 128 MiB) in memory.
                let mut remaining = length;
                let mut offset = start * BLOCK_SIZE as u64;
                while remaining > 0 {
                    let chunk = remaining.min(BLOCK_SIZE as u64) as usize;
                    let bytes = self.read_bytes(offset, chunk)?;
                    writer
                        .write_all(&bytes)
                        .map_err(|error| anyhow!("cannot stream file content: {error}"))?;
                    offset += chunk as u64;
                    remaining -= chunk as u64;
                }
            }
            written = logical + length;
        }
        write_zeros(writer, size - written)
            .map_err(|error| anyhow!("cannot write file holes: {error}"))?;
        Ok(size)
    }

    /// Returns the target of the symlink described by `inode`, as raw bytes:
    /// a target that is not valid UTF-8 is returned as-is instead of
    /// failing. Fast targets live in `i_block`; slow ones in the extent at
    /// logical block 0.
    pub fn symlink_target_bytes(&self, inode: &Ext4Inode) -> Result<Vec<u8>> {
        if inode.file_type() != S_IFLNK {
            bail!("the inode is not a symlink");
        }
        let size = inode.i_size_lo as usize;
        if inode.i_flags & EXT4_EXTENTS_FL != 0 {
            if size >= BLOCK_SIZE {
                bail!("the symlink inode has an invalid size");
            }
            let block = self.extent_at_block_zero(&self.read_extent_tree(inode)?.leaves)?;
            Ok(self.read_bytes(block * BLOCK_SIZE as u64, size)?)
        } else {
            inode
                .i_block
                .get(..size)
                .map(|bytes| bytes.to_vec())
                .ok_or_else(|| anyhow!("the symlink inode has an invalid size"))
        }
    }
}

/// The inode's own block count: `i_blocks_lo` plus its high 16 bits in
/// `i_osd2[0..2]` (the builder's `set_block_sectors` writes both), counted
/// in 512-byte sectors like the kernel counts them.
fn owned_blocks(inode: &Ext4Inode) -> u64 {
    let high = u16::from_le_bytes([inode.i_osd2[0], inode.i_osd2[1]]) as u64;
    (u64::from(inode.i_blocks_lo) | (high << 32)) / (BLOCK_SIZE as u64 / 512)
}

/// Reads exactly `len` bytes at `offset` from `backing`.
fn read_exact(backing: &Backing, offset: u64, len: usize) -> Result<Vec<u8>> {
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| anyhow!("read at {offset} of {len} bytes overflows"))?;
    if end > backing.len() {
        bail!("read at {offset} of {len} bytes lies outside the image");
    }
    let mut buf = vec![0u8; len];
    backing
        .read_at(&mut buf, offset)
        .map_err(|error| anyhow!("cannot read the image at {offset}: {error}"))?;
    Ok(buf)
}

/// Parses one directory block into `(name bytes, inode)` records, skipping
/// deleted records and `.` / `..`. Mirrors the record validation of the
/// builder's `dir_block_records` (rec_len at least 8, a multiple of 4,
/// inside the block, with the name inside the record), but keeps the names
/// as raw bytes so hostile content is classified by the caller instead of
/// failing the walk.
fn parse_dir_block(block: &[u8]) -> Result<Vec<(Vec<u8>, u32)>> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off < BLOCK_SIZE {
        let header = block
            .get(off..off + 8)
            .and_then(|bytes| Ext4DirEntry2Header::read_from_bytes(bytes).ok())
            .ok_or_else(|| anyhow!("a directory block is truncated"))?;
        let (rec_len, name_len) = (header.rec_len as usize, header.name_len as usize);
        if rec_len < 8
            || !rec_len.is_multiple_of(4)
            || off + rec_len > BLOCK_SIZE
            || name_len + 8 > rec_len
        {
            bail!("a directory block has an invalid record");
        }
        if header.inode != 0 {
            let name = block
                .get(off + 8..off + 8 + name_len)
                .ok_or_else(|| anyhow!("a directory record name is truncated"))?
                .to_vec();
            if name != b"." && name != b".." {
                entries.push((name, header.inode));
            }
        }
        off += rec_len;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::test_support::fresh_builder;
    use crate::ext4::{Ext4ImageBuilder, PosixMetadata};
    use std::io::Cursor;
    use zerocopy::IntoBytes;

    /// A builder fixture with nested directories, files (one multi-block),
    /// fast and slow symlinks, a hard link and an empty file.
    fn fixture() -> Ext4ImageBuilder {
        let mut builder = fresh_builder().unwrap();
        let meta = PosixMetadata::default();
        let opt = builder.mkdir(EXT4_ROOT_INO, "opt", None).unwrap();
        builder.mkdir(opt, "sub", None).unwrap();
        builder
            .add_file(opt, "small.txt", b"hello ext4\n", &meta)
            .unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        builder
            .add_file_from_reader(opt, "big.bin", Cursor::new(&big), big.len() as u64, &meta)
            .unwrap();
        builder.add_file(opt, "empty.txt", b"", &meta).unwrap();
        builder
            .add_symlink(opt, "fast", "small.txt", &meta)
            .unwrap();
        let slow_target = "a-very-long-symlink-target-".repeat(4);
        builder
            .add_symlink(opt, "slow", &slow_target, &meta)
            .unwrap();
        let small = builder.lookup_dir_entry(opt, "small.txt").unwrap().unwrap();
        builder.add_hard_link(opt, "hard.txt", small).unwrap();
        // A fragmented file whose extent tree needs an external index node
        // with two leaf blocks: 360 one-block extents do not fit the four
        // inline entries or one 340-entry leaf, so the tree is depth 1 with
        // siblings — the case where a reversed walk order breaks the
        // sorted-extents check in read_file_from. Built like
        // `replacing_contents_can_spread_over_reclaimed_holes`: fill,
        // punch every other block, reset the allocation hints, then
        // replace the target's contents with a payload that spreads over
        // the holes.
        let holes = builder.mkdir(EXT4_ROOT_INO, "holes", None).unwrap();
        for index in 0..720 {
            builder
                .add_file(holes, &format!("f{index}"), &[1; BLOCK_SIZE], &meta)
                .unwrap();
        }
        let target = builder
            .add_file(EXT4_ROOT_INO, "target", b"old", &meta)
            .unwrap();
        for index in (0..720).step_by(2) {
            builder
                .remove_dir_entry(holes, &format!("f{index}"))
                .unwrap();
        }
        builder.next_block_hints.fill(0);
        let fragmented: Vec<u8> = (0..360 * BLOCK_SIZE as u32)
            .map(|byte| byte as u8)
            .collect();
        builder
            .replace_file_contents(target, Cursor::new(&fragmented), fragmented.len() as u64)
            .unwrap();
        // Pin the fixture's tree shape: 360 extents need an external index
        // node with two leaf blocks. If the allocator ever changes and the
        // tree collapses to depth 0, the parity test would quietly lose its
        // multi-leaf coverage, so fail the fixture instead.
        let tree = builder.read_extent_tree(builder.inode(target)).unwrap();
        assert!(
            tree.tree_blocks.len() >= 2,
            "the fragmented fixture must have at least two extent-tree \
             blocks (depth 1), found {}",
            tree.tree_blocks.len()
        );
        // Serialize the superblock, descriptors and bitmaps into the
        // backing bytes, like write_to_file does, so the reader sees them.
        builder.commit_metadata();
        builder
    }

    fn reader_from(builder: &Ext4ImageBuilder) -> ImageReader {
        let (reader, state) = ImageReader::from_memory(builder.disk_data.to_vec()).unwrap();
        assert!(!state.mounted);
        assert!(state.journal_empty);
        reader
    }

    #[test]
    fn reader_agrees_with_the_builder_on_entries_files_and_symlinks() {
        let builder = fixture();
        let reader = reader_from(&builder);

        // Compare every directory recursively (iteratively: no recursion
        // over trees, even in tests, to match the house style).
        let mut stack = vec![EXT4_ROOT_INO];
        while let Some(dir) = stack.pop() {
            let mut builder_entries: Vec<(String, u32)> = builder
                .directory_entries(dir)
                .unwrap()
                .into_iter()
                .collect();
            let mut reader_entries = reader.directory_entries_raw(dir).unwrap();
            builder_entries.sort();
            reader_entries.sort();
            assert_eq!(builder_entries.len(), reader_entries.len());
            for ((name, expected_ino), (raw, ino)) in builder_entries.iter().zip(&reader_entries) {
                assert_eq!(name.as_bytes(), raw.as_slice());
                assert_eq!(expected_ino, ino);
            }
            for (_, ino) in reader_entries {
                let inode = reader.read_inode(ino).unwrap();
                match inode.file_type() {
                    S_IFDIR => stack.push(ino),
                    S_IFREG => {
                        let mut reader_bytes = Vec::new();
                        let count = reader.read_file_from(&inode, &mut reader_bytes).unwrap();
                        let mut builder_bytes = Vec::new();
                        let builder_count = builder.read_file(ino, &mut builder_bytes).unwrap();
                        assert_eq!(count, builder_count);
                        assert_eq!(reader_bytes, builder_bytes, "content of inode {ino}");
                    }
                    S_IFLNK => {
                        let reader_target = reader.symlink_target_bytes(&inode).unwrap();
                        let builder_target = builder.read_symlink(ino).unwrap();
                        assert_eq!(reader_target, builder_target.as_bytes());
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn read_inode_fresh_matches_the_cached_read() {
        let builder = fixture();
        let reader = reader_from(&builder);
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let cached = reader.read_inode(opt).unwrap();
        let fresh = reader.read_inode_fresh(opt).unwrap();
        assert_eq!(cached.as_bytes(), fresh.as_bytes());
    }

    #[test]
    fn recheck_state_matches_the_open_state() {
        let builder = fixture();
        let (reader, state) = ImageReader::from_memory(builder.disk_data.to_vec()).unwrap();
        assert_eq!(reader.recheck_state().unwrap(), state);
    }

    #[test]
    fn container_magics_are_rejected_with_a_pass_the_raw_image_error() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        bytes[..4].copy_from_slice(QCOW2_MAGIC);
        let error = ImageReader::from_memory(bytes).err().unwrap().to_string();
        assert!(error.contains("qcow2"), "{error}");
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        bytes[..8].copy_from_slice(VHDX_MAGIC);
        let error = ImageReader::from_memory(bytes).err().unwrap().to_string();
        assert!(error.contains("VHD"), "{error}");
    }

    #[test]
    fn a_truncated_image_is_refused() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        bytes.truncate(bytes.len() - BLOCK_SIZE);
        let error = ImageReader::from_memory(bytes).err().unwrap().to_string();
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn an_extent_block_outside_the_filesystem_is_refused() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        // Corrupt the big file's first extent to point past the filesystem.
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let big = builder.lookup_dir_entry(opt, "big.bin").unwrap().unwrap();
        let offset = reader_inode_offset(&builder, big);
        // i_block starts at inode offset 40; the first extent entry at
        // i_block[12..24] has ee_start_lo at entry offset 8.
        let start_lo = offset + 40 + 12 + 8;
        bytes[start_lo..start_lo + 4].copy_from_slice(&0xFFFF_F000u32.to_le_bytes());
        let reader = ImageReader::from_memory(bytes).unwrap().0;
        let inode = reader.read_inode(big).unwrap();
        let error = reader
            .read_file_from(&inode, &mut Vec::new())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("outside the filesystem"), "{error}");
    }

    #[test]
    fn an_extent_depth_above_five_is_refused() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let big = builder.lookup_dir_entry(opt, "big.bin").unwrap().unwrap();
        let offset = reader_inode_offset(&builder, big);
        // eh_depth is at i_block[6..8]; i_block starts at inode offset 40.
        bytes[offset + 46..offset + 48].copy_from_slice(&6u16.to_le_bytes());
        let reader = ImageReader::from_memory(bytes).unwrap().0;
        let inode = reader.read_inode(big).unwrap();
        let error = reader
            .read_file_from(&inode, &mut Vec::new())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("depth"), "{error}");
    }

    #[test]
    fn a_tree_referencing_the_same_node_twice_is_refused_without_blowing_up() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let big = builder.lookup_dir_entry(opt, "big.bin").unwrap().unwrap();
        let offset = reader_inode_offset(&builder, big);

        // Build a depth-1 root whose two index entries point at the same
        // child block: without the visited set this fan-out would
        // materialize one subtree per reference, and a crafted tree of
        // depth 3-4 with 340 entries per node reaches billions of leaves.
        let jsb_block = builder.superblock.s_blocks_count_lo - 1;
        let scratch = jsb_block as usize * BLOCK_SIZE;
        // The child: a depth-0 leaf node with one entry, inside a block the
        // builder owns (the last block, unused by the fixture data).
        let header = Ext4ExtentHeader {
            eh_magic: EXT4_EH_MAGIC,
            eh_entries: 1,
            eh_max: EXT4_EXTENTS_PER_BLOCK as u16,
            eh_depth: 0,
            eh_generation: 0,
        };
        bytes[scratch..scratch + 12].copy_from_slice(header.as_bytes());
        let extent = Ext4Extent {
            ee_block: 0,
            ee_len: 1,
            ee_start_hi: 0,
            ee_start_lo: jsb_block - 1,
        };
        bytes[scratch + 12..scratch + 24].copy_from_slice(extent.as_bytes());

        // The root: depth 1, two index entries, both pointing at the child.
        let root = Ext4ExtentHeader {
            eh_magic: EXT4_EH_MAGIC,
            eh_entries: 2,
            eh_max: EXT4_EXTENTS_IN_INODE as u16,
            eh_depth: 1,
            eh_generation: 0,
        };
        bytes[offset + 40..offset + 52].copy_from_slice(root.as_bytes());
        let index = Ext4ExtentIdx {
            ei_block: 0,
            ei_leaf_lo: jsb_block,
            ei_leaf_hi: 0,
            ei_unused: 0,
        };
        for entry in 0..2 {
            let at = offset + 40 + 12 + entry * 12;
            bytes[at..at + 12].copy_from_slice(index.as_bytes());
        }

        let reader = ImageReader::from_memory(bytes).unwrap().0;
        let inode = reader.read_inode(big).unwrap();
        let error = reader
            .read_file_from(&inode, &mut Vec::new())
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("more than once"),
            "expected the repeated-reference refusal, got: {error}"
        );
    }

    #[test]
    fn a_directory_mapping_more_blocks_than_it_owns_is_refused() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let offset = reader_inode_offset(&builder, opt);
        // i_blocks_lo is at inode offset 28; zero it so the mapped block
        // count exceeds the owned count. The in-walk ownership bound in
        // read_extent_tree fires (same invariant, earlier): the tree's
        // entries already exceed what a zero-block inode owns.
        bytes[offset + 28..offset + 32].copy_from_slice(&0u32.to_le_bytes());
        let reader = ImageReader::from_memory(bytes).unwrap().0;
        let error = reader.directory_entries_raw(opt).err().unwrap().to_string();
        assert!(error.contains("more blocks than its inode owns"), "{error}");
    }

    #[test]
    fn a_directory_record_with_a_bad_rec_len_is_refused() {
        let builder = fixture();
        let mut bytes = builder.disk_data.to_vec();
        let opt = builder.resolve_directory("/opt").unwrap().unwrap();
        let blocks = builder.get_dir_blocks(opt).unwrap();
        let block = blocks[0] as usize * BLOCK_SIZE;
        // The `..` record starts at block offset 12 (after `.`'s 12 bytes);
        // its rec_len sits at record offset 4. Make it a non-multiple of 4.
        bytes[block + 16..block + 18].copy_from_slice(&7u16.to_le_bytes());
        let reader = ImageReader::from_memory(bytes).unwrap().0;
        let error = reader.directory_entries_raw(opt).err().unwrap().to_string();
        assert!(error.contains("invalid record"), "{error}");
    }

    #[test]
    fn inode_numbers_out_of_range_are_refused() {
        let builder = fixture();
        let reader = reader_from(&builder);
        let count = reader.superblock().s_inodes_count;
        assert!(reader.read_inode(0).is_err());
        assert!(reader.read_inode(count + 1).is_err());
        assert!(reader.read_inode_fresh(count + 1).is_err());
    }

    #[test]
    fn journal_states_are_decoded_from_crafted_images() {
        let builder = fixture();
        let jsb_block = builder.superblock.s_blocks_count_lo - 1;
        for s_start in [0u32, 1, 0x1234_5678] {
            let mut bytes = builder.disk_data.to_vec();
            install_journal(&builder, &mut bytes, jsb_block, s_start);
            let (reader, state) = ImageReader::from_memory(bytes).unwrap();
            assert!(state.has_journal);
            // RECOVER is clear in the crafted image, so it is not mounted.
            assert!(!state.mounted);
            assert_eq!(state.journal_empty, s_start == 0);
            assert_eq!(reader.recheck_state().unwrap(), state);
        }
    }

    /// Installs a journal (inode 8, extent-mapped, `has_journal`,
    /// `_s_journal_inum` 8) whose superblock reports `s_start`.
    fn install_journal(builder: &Ext4ImageBuilder, bytes: &mut [u8], jsb_block: u32, s_start: u32) {
        let table = builder.group_descriptors[0].bg_inode_table_lo as usize;
        let inode_offset = table * BLOCK_SIZE + 7 * INODE_SIZE;
        let mut inode =
            Ext4Inode::read_from_bytes(&bytes[inode_offset..inode_offset + INODE_SIZE]).unwrap();
        inode.i_flags |= EXT4_EXTENTS_FL;
        // One 4 KiB data block = 8 sectors, so the walk's ownership bound
        // accepts the crafted tree.
        inode.i_blocks_lo = 8;
        let header = Ext4ExtentHeader {
            eh_magic: EXT4_EH_MAGIC,
            eh_entries: 1,
            eh_max: EXT4_EXTENTS_IN_INODE as u16,
            eh_depth: 0,
            eh_generation: 0,
        };
        inode.i_block[..12].copy_from_slice(header.as_bytes());
        let extent = Ext4Extent {
            ee_block: 0,
            ee_len: 1,
            ee_start_hi: 0,
            ee_start_lo: jsb_block,
        };
        inode.i_block[12..24].copy_from_slice(extent.as_bytes());
        bytes[inode_offset..inode_offset + INODE_SIZE].copy_from_slice(inode.as_bytes());

        let range = 1024..1024 + std::mem::size_of::<Ext4SuperBlock>();
        let mut superblock = Ext4SuperBlock::read_from_bytes(&bytes[range.clone()]).unwrap();
        superblock.s_feature_compat |= EXT4_FEATURE_COMPAT_HAS_JOURNAL;
        superblock._s_journal_inum = 8;
        bytes[range].copy_from_slice(superblock.as_bytes());

        let jsb_offset = jsb_block as usize * BLOCK_SIZE;
        let jsb = &mut bytes[jsb_offset..jsb_offset + BLOCK_SIZE];
        jsb.fill(0);
        jsb[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        jsb[4..8].copy_from_slice(&JBD2_SUPERBLOCK_V2.to_be_bytes());
        jsb[0x0C..0x10].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
        jsb[0x1C..0x20].copy_from_slice(&s_start.to_be_bytes());
    }

    /// The byte offset of `ino` in `builder`'s on-disk inode table.
    fn reader_inode_offset(builder: &Ext4ImageBuilder, ino: u32) -> usize {
        let (group, index) = builder.inode_location(ino);
        builder.group_descriptors[group].bg_inode_table_lo as usize * BLOCK_SIZE
            + index * INODE_SIZE
    }

    #[test]
    fn journal_superblock_parsing_rejects_bad_magic_and_type() {
        let mut jsb = vec![0u8; 0x20];
        jsb[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        jsb[4..8].copy_from_slice(&JBD2_SUPERBLOCK_V2.to_be_bytes());
        jsb[0x1C..0x20].copy_from_slice(&7u32.to_be_bytes());
        let parsed = JournalSuperblock::parse(&jsb).unwrap();
        assert!(!parsed.is_empty());
        jsb[..4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(JournalSuperblock::parse(&jsb).is_err());
        jsb[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        jsb[4..8].copy_from_slice(&2u32.to_be_bytes()); // a commit block
        assert!(JournalSuperblock::parse(&jsb).is_err());
        assert!(JournalSuperblock::parse(&jsb[..0x10]).is_err());
    }

    #[test]
    fn concurrent_block_rewrites_never_panic_or_lose_the_length() {
        use std::io::{Seek as _, Write as _};
        // A file-backed image, rewritten through a second handle while the
        // reader streams. Positional reads are memory-safe by construction
        // (no mmap of a concurrently written file); that is the guarantee
        // this test pins. The snapshot design does NOT rely on the content
        // staying whole-block atomic — freezing is that guarantee — so this
        // asserts only that every read succeeds with the framed length, not
        // old-or-new content (neither Linux filemap_read nor NTFS cached
        // I/O makes a 4 KiB pwrite/pread pair atomic).
        let path = std::env::temp_dir().join(format!(
            "ext4kit-reader-concurrent-{}.img",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let content_a = vec![0xAAu8; BLOCK_SIZE];
        let content_b = vec![0x55u8; BLOCK_SIZE];
        let (reader, small) = {
            let mut builder = Ext4ImageBuilder::new_file(16, &path).unwrap();
            let meta = PosixMetadata::default();
            builder.create_root_dir().unwrap();
            builder.create_lost_and_found().unwrap();
            builder.reserve_inodes();
            let small = builder
                .add_file(EXT4_ROOT_INO, "small.bin", &content_a, &meta)
                .unwrap();
            builder.write_to_file(path.to_str().unwrap()).unwrap();
            let (reader, state) = ImageReader::open(&path).unwrap();
            assert!(!state.mounted);
            (reader, small)
        };
        // The data block of the one-block file: its first extent's start.
        let inode = reader.read_inode_fresh(small).unwrap();
        let extents = reader.read_extent_tree(&inode).unwrap().leaves;
        let data_block = extents[0].ee_start_lo as usize * BLOCK_SIZE;

        let writer_path = path.clone();
        let writer_contents = (content_a, content_b);
        let writer = std::thread::spawn(move || {
            let (content_a, content_b) = writer_contents;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .unwrap();
            for iteration in 0..100 {
                let content = if iteration % 2 == 0 {
                    &content_b
                } else {
                    &content_a
                };
                file.seek(std::io::SeekFrom::Start(data_block as u64))
                    .unwrap();
                file.write_all(content).unwrap();
            }
        });
        for _ in 0..100 {
            let mut out = Vec::new();
            reader.read_file_from(&inode, &mut out).unwrap();
            assert_eq!(out.len(), BLOCK_SIZE);
            // Only what the OS guarantees: reads succeed with the framed
            // length. The content may be old, new or interleaved.
        }
        writer.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
