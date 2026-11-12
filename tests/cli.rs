//! End-to-end tests for `ext4kit create` with host directories. Each test
//! builds a real image file and has e2fsck judge it; see `common` for how
//! e2fsprogs is located.

mod common;

use common::{Image, arg, create, e2fsprogs_tools, ext4kit};
use std::{fs, path::Path};
use tempfile::TempDir;

/// A small tree with nested directories, empty and non-empty files and a
/// maximum-length name.
fn sample_tree(root: &Path) {
    fs::create_dir_all(root.join("app/config/nested")).unwrap();
    fs::create_dir_all(root.join("app/empty-dir")).unwrap();
    fs::write(root.join("app/README"), "hello from ext4kit\n").unwrap();
    fs::write(root.join("app/config/settings.toml"), "enabled = true\n").unwrap();
    fs::write(root.join("app/config/nested/empty"), "").unwrap();
    fs::write(root.join("app/with space.txt"), "spaced\n").unwrap();
    fs::write(root.join("app").join("n".repeat(255)), "long\n").unwrap();
}

fn assert_sample_tree(image: &Image, at: &str) {
    let mut entries = image.list(at);
    entries.sort();
    let mut expected = vec![
        "README".to_owned(),
        "config".to_owned(),
        "empty-dir".to_owned(),
        "n".repeat(255),
        "with space.txt".to_owned(),
    ];
    expected.sort();
    assert_eq!(entries, expected);
    assert_eq!(image.cat(&format!("{at}/README")), "hello from ext4kit\n");
    assert_eq!(
        image.cat(&format!("{at}/config/settings.toml")),
        "enabled = true\n"
    );
    assert_eq!(image.cat(&format!("{at}/with space.txt")), "spaced\n");
    assert_eq!(image.cat(&format!("{at}/config/nested/empty")), "");
    assert!(image.list(&format!("{at}/empty-dir")).is_empty());
    assert_eq!(image.mode(&format!("{at}/config")), 0o755);
}

#[test]
fn host_tree_with_sizing_pass_is_clean() {
    let tmp = TempDir::new().unwrap();
    sample_tree(&tmp.path().join("src"));
    let out = tmp.path().join("out.img");
    create(
        &[
            "--add",
            &format!("{}:/opt", arg(&tmp.path().join("src/app"))),
        ],
        &out,
    );
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_sample_tree(&image, "/opt/app");
    let mut root = image.list("/");
    root.sort();
    assert_eq!(root, ["lost+found", "opt"]);
}

#[test]
fn host_tree_with_explicit_size_is_clean() {
    let tmp = TempDir::new().unwrap();
    sample_tree(&tmp.path().join("src"));
    let out = tmp.path().join("out.img");
    create(
        &[
            "--size",
            "32",
            "--add",
            &format!("{}:/srv/data", arg(&tmp.path().join("src/app"))),
        ],
        &out,
    );
    assert_eq!(fs::metadata(&out).unwrap().len(), 32 * 1024 * 1024);
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_sample_tree(&image, "/srv/data/app");
}

#[test]
fn explicit_size_with_an_undersized_trailing_group_is_clean() {
    // 129 MiB is 33,024 blocks: the 256-block trailing group cannot hold its
    // backup superblock, bitmaps and inode table, so the filesystem is
    // planned as a single full 128 MiB group and the image's tail blocks
    // stay unused. This size used to underflow the free-block counters.
    let tmp = TempDir::new().unwrap();
    sample_tree(&tmp.path().join("src"));
    let out = tmp.path().join("out.img");
    create(
        &[
            "--size",
            "129",
            "--add",
            &format!("{}:/opt", arg(&tmp.path().join("src/app"))),
        ],
        &out,
    );
    // Reopening must accept a filesystem that ends before the image does.
    let edited = ext4kit(&["edit", &arg(&out), "mkdir", "/opt/edited"]);
    assert!(
        edited.status.success(),
        "edit failed with {}\nstderr:\n{}",
        edited.status,
        String::from_utf8_lossy(&edited.stderr),
    );
    assert_eq!(fs::metadata(&out).unwrap().len(), 129 * 1024 * 1024);
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_sample_tree(&image, "/opt/app");
    assert!(image.list("/opt").contains(&"edited".to_owned()));
    let stats = image.debugfs("stats");
    let block_count = stats
        .lines()
        .find_map(|line| line.strip_prefix("Block count:"))
        .and_then(|rest| rest.trim().parse::<u64>().ok())
        .expect("debugfs stats reports a block count");
    assert_eq!(block_count, 32768, "{stats}");
}

#[test]
fn single_file_lands_in_destination_directory() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("hosts");
    fs::write(&file, "127.0.0.1 localhost\n").unwrap();
    let out = tmp.path().join("out.img");
    create(&["--add", &format!("{}:/etc", arg(&file))], &out);
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_eq!(image.cat("/etc/hosts"), "127.0.0.1 localhost\n");
}

#[test]
fn created_images_carry_a_random_filesystem_uuid() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("src");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("a.txt"), "hi\n").unwrap();
    let out = tmp.path().join("out.img");
    create(&["--add", &format!("{}:/opt", arg(&dir))], &out);
    let Some(image) = Image::check(&out) else {
        return;
    };
    let stats = image.debugfs("stats");
    let uuid = stats
        .lines()
        .find_map(|line| line.strip_prefix("Filesystem UUID:"))
        .map(str::trim)
        .expect("debugfs stats reports a filesystem UUID");
    assert!(
        !uuid.chars().all(|c| c == '0'),
        "the UUID must not be all zeros: {stats}"
    );
    // xxxxxxxx-xxxx-4xxx-…: the third group starts with the version digit.
    assert_eq!(
        uuid.as_bytes().get(14),
        Some(&b'4'),
        "a version-4 UUID like mke2fs writes: {uuid}"
    );
}

/// The jsb decoder must agree with `dumpe2fs` on an image `mke2fs` made: a
/// fresh journaled filesystem reports `Journal start: 0` and `needs_recovery`
/// clear. Testing against a kernel-written jsb catches offset and endianness
/// mistakes that a decode-what-we-encoded test cannot.
#[test]
fn reader_agrees_with_dumpe2fs_on_an_mke2fs_journal() {
    use ext4kit::ext4::reader::ImageReader;

    let Some(tools) = e2fsprogs_tools() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("j.img");
    fs::write(&image, vec![0u8; 64 * 1024 * 1024]).unwrap();

    // mkfs.ext4 writes a journaled filesystem with modern defaults ext4kit's
    // fixed format does not carry (checksums, resize inodes, orphan file);
    // ask mke2fs for the reader's own feature set so only the journal
    // differs from an ext4kit image.
    let target = tools.target(&image);
    let output = common::run_tool(
        tools,
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-O",
            "^metadata_csum,^64bit,^flex_bg,^resize_inode,^orphan_file",
            &target,
        ],
    );
    assert!(
        output.status.success(),
        "mkfs.ext4 failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // What dumpe2fs reports about the fresh journal.
    let output = common::run_tool(tools, "dumpe2fs", &["-h", &target]);
    let dump = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let journal_start = dump
        .lines()
        .find_map(|line| line.trim().strip_prefix("Journal start:"))
        .map(str::trim)
        .expect("dumpe2fs reports a Journal start");
    let needs_recovery = dump.contains("needs_recovery");
    assert_eq!(journal_start, "0", "{dump}");

    // What the reader decodes from the same bytes. mke2fs also sets
    // features the reader tolerates (extra ro_compat) or requires the same
    // way; the state facts are what matters here.
    let (reader, state) = ImageReader::open(&image).expect("the reader opens an mke2fs image");
    assert!(state.has_journal, "{dump}");
    assert!(state.journal_empty, "the reader must see the journal empty");
    assert!(!state.mounted, "a fresh image has needs_recovery clear");
    assert!(!needs_recovery, "{dump}");
    assert_eq!(reader.recheck_state().unwrap(), state);
}

/// The full journal state machine on a kernel-mounted filesystem (the
/// round-9 table, as fixtures): mounted with committed transactions (a
/// write followed by `sync`), frozen, and cleanly unmounted. Each state is
/// snapshotted to a host-side copy and cross-checked against `dumpe2fs -h`
/// and the reader.
///
/// Needs root for the loop mount: on WSL through `wsl -u root`, natively
/// as root or through passwordless `sudo -n` (as CI runners have), and
/// skipped with a note otherwise (see `common::root_shell`). `fsfreeze`
/// comes from util-linux, not e2fsprogs, so the script resolves it — like
/// `mount` and `sync` — through the shell's PATH instead of the located
/// e2fsprogs directory.
#[test]
fn reader_matches_the_journal_state_machine_of_a_mounted_filesystem() {
    use ext4kit::ext4::reader::{ImageReader, ImageState};

    let Some(tools) = e2fsprogs_tools() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let synced = tools.target(&tmp.path().join("synced.img"));
    let frozen = tools.target(&tmp.path().join("frozen.img"));
    let unmounted = tools.target(&tmp.path().join("unmounted.img"));
    let script = format!(
        "set -e\n\
         umount /mnt/ext4kit-jsb 2>/dev/null || true\n\
         mkdir -p /mnt/ext4kit-jsb\n\
         rm -f /tmp/ext4kit-jsb.img\n\
         dd if=/dev/zero of=/tmp/ext4kit-jsb.img bs=1M count=64 status=none\n\
         ${{BIN}}mkfs.ext4 -q -F -O \
         ^metadata_csum,^64bit,^flex_bg,^resize_inode,^orphan_file \
         /tmp/ext4kit-jsb.img\n\
         mount -o loop /tmp/ext4kit-jsb.img /mnt/ext4kit-jsb\n\
         echo hi > /mnt/ext4kit-jsb/f.txt\n\
         sync\n\
         cp /tmp/ext4kit-jsb.img \"{synced}\"\n\
         fsfreeze --freeze /mnt/ext4kit-jsb\n\
         cp /tmp/ext4kit-jsb.img \"{frozen}\"\n\
         fsfreeze --unfreeze /mnt/ext4kit-jsb\n\
         umount /mnt/ext4kit-jsb\n\
         cp /tmp/ext4kit-jsb.img \"{unmounted}\"\n\
         rm -f /tmp/ext4kit-jsb.img"
    );
    let Some(output) = common::root_shell(tools, &script) else {
        return;
    };
    assert!(
        output.status.success(),
        "the state-machine script failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Each state: the reader and dumpe2fs must tell the same story.
    // (mounted, journal_empty) from left: RECOVER set/clear, s_start
    // non-zero/zero.
    let expect = |copy: &Path, mounted: bool, journal_empty: bool| {
        let (reader, state): (_, ImageState) =
            ImageReader::open(copy).expect("the reader opens the state copy");
        assert_eq!(state.mounted, mounted, "{:?}", state);
        assert_eq!(state.journal_empty, journal_empty, "{:?}", state);
        assert!(state.has_journal);
        assert_eq!(reader.recheck_state().unwrap(), state);

        let target = tools.target(copy);
        let output = common::run_tool(tools, "dumpe2fs", &["-h", &target]);
        let dump = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let start = dump
            .lines()
            .find_map(|line| line.trim().strip_prefix("Journal start:"))
            .map(str::trim)
            .expect("dumpefs reports a Journal start");
        assert_eq!(
            start == "0",
            journal_empty,
            "dumpe2fs Journal start {start} vs reader journal_empty {journal_empty}: {dump}"
        );
        assert_eq!(
            dump.contains("needs_recovery"),
            mounted,
            "dumpe2fs needs_recovery vs reader mounted {mounted}: {dump}"
        );
    };

    let base = tmp.path();
    // Write + sync: mounted, journal holds committed transactions.
    expect(&base.join("synced.img"), true, false);
    // fsfreeze: the journal is flushed and RECOVER clears while the VM runs.
    expect(&base.join("frozen.img"), false, true);
    // Clean unmount: same on-disk state as frozen.
    expect(&base.join("unmounted.img"), false, true);
}

#[test]
fn large_directory_gets_an_htree_index() {
    let tmp = TempDir::new().unwrap();
    let big = tmp.path().join("big");
    fs::create_dir(&big).unwrap();
    for i in 0..5000 {
        fs::write(big.join(format!("file-{i:05}-{}", "x".repeat(40))), "").unwrap();
    }
    let out = tmp.path().join("out.img");
    create(&["--add", &arg(&big)], &out);
    let Some(image) = Image::check(&out) else {
        return;
    };
    // Without metadata_csum, debugfs still complains that dx blocks lack
    // checksum space; mke2fs-built images get the same message, so only the
    // "not indexed" error counts.
    let (htree, error) = image.debugfs_with_stderr("htree /big");
    assert!(!error.contains("Not a hash-indexed"), "/big: {error}");
    assert!(htree.contains("Root node dump"), "{htree}");
    assert_eq!(image.list("/big").len(), 5000);
}

#[test]
fn large_file_uses_an_extent_tree_block() {
    // An extent covers at most 128 MiB and the inode holds four extents, so a
    // file of more than 512 MiB needs an extent-tree block. The source file is
    // sparse, so the host side costs no disk space.
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let large = fs::File::create(src.join("large.bin")).unwrap();
    large.set_len(600 * 1024 * 1024).unwrap();
    drop(large);
    let out = tmp.path().join("out.img");
    create(&["--add", &arg(&src)], &out);
    let Some(image) = Image::check(&out) else {
        return;
    };
    let stat = image.stat("/src/large.bin");
    assert!(stat.contains("Size: 629145600"), "{stat}");
    assert!(
        stat.contains("(ETB0)"),
        "expected an extent-tree block:\n{stat}"
    );
}

#[test]
fn ownership_flags_apply_to_additions_and_root() {
    let tmp = TempDir::new().unwrap();
    sample_tree(&tmp.path().join("src"));
    let out = tmp.path().join("out.img");
    create(
        &[
            "--chown",
            "1000:1001",
            "--root-owner",
            "2000",
            "--add",
            &format!("{}:/home/user", arg(&tmp.path().join("src/app"))),
        ],
        &out,
    );
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_eq!(image.owner("/"), (2000, 2000));
    assert_eq!(image.owner("/lost+found"), (0, 0));
    for path in [
        "/home",
        "/home/user",
        "/home/user/app",
        "/home/user/app/config/nested/empty",
    ] {
        assert_eq!(image.owner(path), (1000, 1001), "owner of {path}");
    }
}

#[test]
fn directory_addition_can_host_a_later_addition() {
    // `--add b:/a` provides `/a/b`, which a later `--add c:/a/b` targets,
    // regardless of the order on the command line.
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    fs::create_dir(&b).unwrap();
    fs::write(b.join("from-b"), "b\n").unwrap();
    let c = tmp.path().join("c");
    fs::write(&c, "c\n").unwrap();
    let out = tmp.path().join("out.img");
    create(
        &[
            "--add",
            &format!("{}:/a/b", arg(&c)),
            "--add",
            &format!("{}:/a", arg(&b)),
        ],
        &out,
    );
    let Some(image) = Image::check(&out) else {
        return;
    };
    let mut entries = image.list("/a/b");
    entries.sort();
    assert_eq!(entries, ["c", "from-b"]);
    assert_eq!(image.cat("/a/b/c"), "c\n");
}

#[cfg(unix)]
#[test]
fn unix_metadata_and_symlinks_are_preserved() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let script = src.join("run.sh");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    // 59 bytes is the longest inline target; 60 and longer use a data block.
    for len in [1, 59, 60, 4095] {
        symlink("t".repeat(len), src.join(format!("link-{len}"))).unwrap();
    }
    let out = tmp.path().join("out.img");
    create(&["--add", &arg(&src)], &out);
    let Some(image) = Image::check(&out) else {
        return;
    };
    assert_eq!(image.mode("/src/run.sh"), 0o750);
    for len in [1, 59] {
        let stat = image.stat(&format!("/src/link-{len}"));
        assert!(stat.contains("Fast link dest"), "{stat}");
    }
    for len in [60, 4095] {
        let stat = image.stat(&format!("/src/link-{len}"));
        assert!(stat.contains("Type: symlink"), "{stat}");
        assert!(stat.contains("EXTENTS:"), "{stat}");
    }
}

#[test]
fn rejects_an_image_below_the_minimum_size() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let out = tmp.path().join("out.img");
    let result = ext4kit(&["create", "--size", "8", "--add", &arg(&src), &arg(&out)]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("at least 16 MiB"));
    assert!(
        !out.exists(),
        "a rejected run must not leave an image behind"
    );
}

#[test]
fn rejects_colliding_additions() {
    let tmp = TempDir::new().unwrap();
    for dir in ["one/data", "two/data"] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }
    let out = tmp.path().join("out.img");
    let result = ext4kit(&[
        "create",
        "--add",
        &arg(&tmp.path().join("one/data")),
        "--add",
        &arg(&tmp.path().join("two/data")),
        &arg(&out),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("multiple additions target /data"));
}
