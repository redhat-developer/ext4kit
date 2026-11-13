# AGENTS.md

Guidance for coding agents working on `ext4kit`. Read this before making
changes; the user-facing docs are [README.md](README.md) and [docs/](docs/).

## What this is

A Rust library + CLI that writes **real ext4 images from scratch**, without
Linux, `mke2fs` or a loop mount. The main platform is Windows. Sources come
from host directories (`--add`) and from OCI images in a registry or a local
Podman/Docker engine (`--from`). The images get attached to Linux VMs.

Correctness is judged by `e2fsck`, not by whether the code runs. A bug usually
produces an image that the kernel mounts with silent corruption, so every
on-disk change has to be verified with e2fsprogs (see [Verifying images](#verifying-images)).

## Commands

```sh
cargo build
cargo test --lib --bins          # unit tests, offline, well under a second
cargo test --test cli            # end-to-end: builds images and runs e2fsck (~15 s)
cargo test                       # both of the above
cargo test --test oci -- --ignored  # --from tests; need network and Docker
cargo test <name_substring>      # a single test
cargo fmt                        # CI-style check: cargo fmt --check
cargo clippy --all-targets       # must report zero warnings
cargo run -- create --add <SRC>[:/DEST] [--from IMAGE] [--size MIB] out.img
cargo run -- edit out.img <rm|mkdir|chmod|chown|add|write|cat|test|ln> ...
bash scripts/verify-image.sh out.img ['debugfs request' ...]
```

The compiler version is pinned in `rust-toolchain.toml`; rustup installs and
activates it automatically for this directory, and CI installs exactly that
version, so local runs and CI cannot diverge over std changes between minor
releases. Bump the pin deliberately when adopting a new stable.

Before you report a change as done, `cargo fmt --check`, `cargo test` and
`cargo clippy --all-targets` must all pass, and clippy must report no
warnings. Fix a lint rather than silencing it. Use `#[allow(clippy::…)]`
only with a comment that explains why the code has to stay as it is. If the
change touches anything written to disk, the verify script must exit 0 as
well.

## Layout

The library is layered; a module uses only the layers above it:

1. `ext4::format`: the on-disk format as plain data and encodings, no builder.
2. `ext4`: the filesystem engine, `Ext4ImageBuilder`, which lays out, loads
   and changes an image by **inode number**.
3. `vfs`: name-based access, turning paths into inodes through directories
   and symlinks, and the coreutils-style edits.
4. `host`, `oci`: importers of external content, built on `vfs` and `ext4`.

`main.rs` only parses arguments and orchestrates these.

| Path | Responsibility |
|------|----------------|
| `src/ext4/format/mod.rs` | On-disk structs (`zerocopy`) and constants: `BLOCK_SIZE` = 4096, `INODE_SIZE` = 256, `BLOCKS_PER_GROUP`, inode numbers, file types |
| `src/ext4/format/dirhash.rs`, `xattr.rs` | The half-MD4 directory hash; xattr name parsing, xattr block encoding (`encode_xattr_block`) and POSIX ACL conversion |
| `src/ext4/mod.rs` | The `Ext4ImageBuilder` struct, `DiskData` (memory, mmap'd file or read-only copy-on-write), constructors, and the `test_support::fresh_builder()` fixture |
| `src/ext4/layout.rs` | Block-group geometry, root and `lost+found`, serializing superblocks, descriptors, bitmaps and inode tables (`commit_metadata`, `write_to_file`) |
| `src/ext4/open.rs` | `Ext4ImageBuilder::open` / `open_read_only` / `from_image`: load an existing ext4kit image, rejecting foreign features and inconsistent counters |
| `src/ext4/allocation.rs` | Block and inode bitmaps: plan, commit and release blocks and inodes, free counts, `ensure_allocation_capacity` |
| `src/ext4/inode.rs` | `PosixMetadata`, inode table access, mode/owner/metadata updates (`set_inode_mode`, `set_owner_recursive`), writing xattr blocks |
| `src/ext4/extent.rs` | Reading and validating extent trees, writing them (`write_extent_tree`), the block runs an inode owns |
| `src/ext4/directory.rs` | `mkdir`, dir-entry add/remove/lookup, recursive removal, directory growth, linear-directory cache |
| `src/ext4/htree.rs` | Hashed (dx) directory indexes |
| `src/ext4/file.rs`, `symlink.rs`, `special.rs` | Regular files (streamed from a `Read`, read back with `read_file`, rewritten in place with `replace_file_contents`), symlinks (fast < 60 bytes / slow), device nodes, FIFOs, sockets |
| `src/vfs/resolve.rs` | Path resolution through symlinks, confined to the image root (`resolve_entry`, `resolve_directory`, `resolve_path`), and `ensure_directory` |
| `src/vfs/edit.rs` | Path-level edits with coreutils semantics (`remove_path`, `make_directory`, `chmod_path`, `chown_path`, `write_path`, `test_path`, ...) behind `ext4kit edit` |
| `src/host/` | Host directory import (`dua-core` walk) and sizing; `tree.rs` is the pre-scanned `HostTree` arena |
| `src/oci/` | Tar layer application with whiteouts (`mod.rs`), registry pulls (`remote.rs`), Docker/Podman engines (`local.rs`) |
| `src/main.rs` | Thin clap CLI: argument parsing, sizing and orchestration only. Filesystem logic belongs in the library |

See [docs/architecture.md](docs/architecture.md) for the two-pass and
streaming import design.

## Invariants: do not break these

- **Preflight, then commit.** Every mutating builder operation first validates
  input and plans *all* resources it needs: the inode, data blocks,
  extent-tree metadata blocks, the xattr block, and the blocks for parent
  directory growth or an htree split. Only then does it mark anything used
  (`ensure_allocation_capacity` → `mark_inode_used` / `commit_data_blocks`).
  A failed call must leave free counts, bitmaps, link counts and directory
  caches exactly as they were. The `failed_*_preflight_*` tests assert
  this; write the same kind of test for any new operation.
- **Keep counters consistent.** Free block/inode counts in both the superblock
  and the group descriptors, `i_links_count` (a subdirectory increments its
  parent), `i_blocks`, and directory `i_size` must all match what `e2fsck`
  recomputes.
- **Directory caches.** `directory_caches` only covers linear directories. Code
  that rewrites directory blocks outside the normal add/remove paths must call
  `invalidate_directory_cache`.
- **No recursion over trees.** Host import walks a flat, parent-first arena,
  and OCI layers are a stream. Large images must not blow the stack or keep
  per-file payloads in memory. File data is copied from a reader straight into
  its extents.
- **Fixed format choices**, modeled on e2fsprogs: 4 KiB blocks, 256-byte
  inodes, 32-bit block numbers, 32-byte group descriptors, sparse superblocks,
  extents, **no journal**. Changing any of these is a design decision, so ask
  before doing it.
- **Cross-platform.** Windows is the primary target but the code also compiles
  on Unix (`#[cfg(unix)]` / `#[cfg(windows)]` in `host/tree.rs`,
  `oci/local.rs` and `main.rs`). Never derive ext4 paths from `std::path`
  separators. Tar paths split on `/` only, and a `\` is a legal filename
  byte.
- **OCI scope:** linux/amd64 only, anonymous registry access, digest-verified
  layers. `--from` entries keep their image ownership, and `--chown` applies
  only to `--add` content.

## Tests

- Unit tests live in `#[cfg(test)] mod tests` beside the code.
- End-to-end tests live in `tests/`. `cli.rs` runs the real binary on
  generated host trees and checks every image with `e2fsck -fn` and
  `debugfs`, using the helpers in `tests/common/mod.rs` (`create`, then
  `Image::check` and `cat`/`stat`/`owner`/`list`). e2fsprogs is found
  natively or through WSL, like the verify script. When it is missing the
  checks are skipped with a note; CI sets `EXT4KIT_REQUIRE_E2FSPROGS=1` so
  they fail instead. `oci.rs` holds `#[ignore]`d `--from` tests that need
  Docker Hub or a Docker engine. `open.rs` reopens created images through
  the library, and `edit.rs` runs sequences of `ext4kit edit` calls.
- Build small images in memory with `Ext4ImageBuilder::new(16)`, then call
  `create_root_dir()`, `create_lost_and_found()` and `reserve_inodes()`
  (`ext4::test_support::fresh_builder()` does this). Assert by reading back
  through the builder (`lookup_dir_entry`, `inode`, `directory_entries`,
  `get_dir_blocks`) and by comparing free counts before and after.
- OCI tests build synthetic tar/gzip layers in memory (`layer()` helper in
  `oci/mod.rs`). Tests that are not `#[ignore]`d must not need the network,
  Docker or Podman.
- Unit tests can't catch a structurally wrong image. For a new on-disk feature,
  add a case to `tests/cli.rs` whose fixture exercises it, and use the
  verify script described next for ad-hoc checks.

## Verifying images

`scripts/verify-image.sh` runs `e2fsck -fn` on an image and exits non-zero on
any finding. It can also run `debugfs -R` requests. It works natively on Linux
and through WSL from Git Bash on Windows; it finds a WSL distro that has
e2fsprogs, or you can set `EXT4KIT_WSL_DISTRO`.

```sh
tmp=$(mktemp -d); command -v cygpath >/dev/null && tmp=$(cygpath -m "$tmp")  # Git Bash: C:/... paths
mkdir -p "$tmp/src/sub" && echo hi > "$tmp/src/sub/a.txt"
cargo run -q -- create --add "$tmp/src:/opt" "$tmp/out.img"
bash scripts/verify-image.sh "$tmp/out.img" 'ls -l /opt/src/sub' 'stat /opt/src/sub/a.txt'
rm -rf "$tmp"
```

Useful `debugfs` requests are `ls -l DIR`, `stat PATH` (extent tree, size,
blocks, ownership), `ea_list PATH` (xattrs), `htree DIR` (dx index) and
`stats` (superblock).
To exercise a feature, generate inputs that hit it: thousands of files in one
directory for htrees, or fragmented or large files for multi-level extent
trees. The `/verify-image` skill in `.claude/skills/` walks through this.

## Gotchas

- **Never commit `*.img` files.** Images are hundreds of MiB to GiB. Create
  them in a temp directory and delete them afterwards.
- Git Bash rewrites POSIX-looking arguments: `/tmp/x:/opt` becomes a `;`-joined
  Windows path list, and backslashes passed to `wsl` vanish. Give `--add` a
  `C:/...` path from `cygpath -m` (as in the snippet above), use
  `MSYS_NO_PATHCONV=1` for `wsl`, or just use the verify script.
- `debugfs -R 'htree DIR'` prints "Directory block does not have space for
  checksum" on every indexed directory. Images built by `mke2fs` without
  `metadata_csum` get the same message, so it is not an ext4kit bug.
- The ext4 hash seed is random per image, so dx-directory layouts differ from
  run to run. Do not write tests that compare whole-image bytes.
- `--from` without `docker://` queries the local engine first. Behavior can
  therefore depend on what Podman/Docker has cached on the machine.
- Reference material: the kernel's ext4 on-disk format docs
  (`Documentation/filesystems/ext4/`) and the e2fsprogs sources
  (`lib/ext2fs/`, `misc/mke2fs.c`). When in doubt, match what `mke2fs`
  writes.

## Conventions

- Rust 2024 edition. Errors use `anyhow` (`bail!`, `anyhow!`, `?`); library
  code returns errors and does not panic on bad input.
- Doc comments state what a function does and what it returns. Comments
  explain *why* (ext4 rules, e2fsprogs compatibility), not what the next line
  does.
- Commits use Conventional Commits with a scope, for example
  `feat(cli): …`, `feat(oci): …`, `refactor: …`, `fix(core): …`. The body
  explains the user-visible motivation.
- When behavior changes, update `README.md` and
  `docs/supported-features.md` in the same change.
