# Supported features

## Current status

The formatter currently creates valid ext4 images with a 4 KiB block size and
256-byte inodes. It writes sparse-superblock layout (including backups and
group descriptors), block and inode bitmaps, inode tables, the root directory,
and `lost+found`. Every image carries a random filesystem UUID (`s_uuid`),
as `mke2fs` writes one, alongside the random directory-hash seed.

It supports regular files across fragmented block runs, building multi-level
extent trees when needed. Directories have `.` and `..` entries, update parent
link counts, and grow across multiple blocks, including fragmented
extent-backed directories. Symlinks use inline storage below 60 bytes and one
data-block extent for longer targets.

Character and block devices, FIFOs, and Unix-domain sockets use
Linux-compatible device-number encoding. Files and directories support POSIX
permissions, 32-bit UID/GID, access/change/modification timestamps, external
xattr blocks with deterministic e2fsprogs-compatible hashes, and conversion
of `system.posix_acl_access` / `system.posix_acl_default` into compact ext4
ACL encoding.

The demonstration image contains:

```text
/
├── lost+found/
└── test_dir/
    └── hello.txt
```

`hello.txt` contains `Created by ext4kit` and has `user.creator=ext4kit`.

## Metadata API

Creation methods accept `PosixMetadata`:

```rust
let metadata = PosixMetadata {
    mode: 0o640,
    uid: 1000,
    gid: 1000,
    mtime: 1_700_000_000,
    ..Default::default()
};

let directory = builder.mkdir(EXT4_ROOT_INO, "app", Some(&metadata))?;
builder.add_file(directory, "config", b"enabled=true\n", &metadata)?;
builder.add_symlink(directory, "current", "config", &metadata)?;
builder.add_char_device(directory, "null", 1, 3, &metadata)?;
builder.add_fifo(directory, "events", &metadata)?;
```

Xattr keys use fully qualified Linux names such as `user.comment`,
`security.capability`, `trusted.overlay.opaque`, and
`system.posix_acl_access`. Each inode currently gets one unshared external
4 KiB xattr block; oversized sets fail before allocation state changes.

Regular files and directories may span fragmented runs. Extent-tree metadata
is added automatically when extents do not fit in the inode. Directory growth
reserves its data and required extent-tree blocks before creating a child, and
reclaims replaced extent-tree blocks. Directories that outgrow their first
block are converted to hashed (dx) indexes. `lost+found` is created as a
multi-block linear directory, as mke2fs writes it, and is not converted: once
its four blocks are full, adding to it fails with an error. How many entries
fit depends on name length, from about 1,000 short names down to about 60
names of 255 bytes.

Symlink targets hold from 1 to `BLOCK_SIZE - 1` bytes (4,095 bytes with
4 KiB blocks). An empty target is rejected, as `symlink(2)` does: e2fsck
reports a zero-length symlink as invalid, and recent Linux kernels refuse to
load one. Targets under 60 bytes are inline; longer targets occupy one data 
block. Inline-data symlinks are not implemented.

Device methods accept a Linux major number up to 4,095 and minor number up to
1,048,575. Special nodes do not allocate data extents, though they may use an
external xattr block.

## Editing existing images

`Ext4ImageBuilder::open` loads an image that ext4kit wrote back into the
builder, and `ext4kit edit` exposes path-level operations on it: `rm`,
`mkdir`, `chmod`, `chown` (recursive without following symlinks), `add`,
`write` (in-place content replacement that keeps the inode, links, timestamps
and xattrs), `cat`, `test` and `ln`. Images in any other format, including
those made by `mke2fs`, are rejected. The image size is fixed. See
[Editing images](editing.md).

## Design constraints and limitations

- Image creation runs on Windows; Linux tools are only optional verification.
- Layout is modeled on `e2fsprogs` and written directly using ext4 structures.
- The implementation uses 32-bit block addresses and 32-byte group descriptors.
- Images are deliberately created without a journal.
- Sparse files, inline/shared xattrs, and SELinux validation are not
  implemented yet.
- Host import preserves the portable subset of metadata supported by the host
  OS. OCI import applies layer metadata, whiteouts (including opaque
  directories), and hard links; only linux/amd64 images and anonymous registry
  access are supported.
