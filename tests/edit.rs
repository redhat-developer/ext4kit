//! End-to-end tests for `ext4kit edit`: each test creates an image, changes
//! it with a sequence of edit calls, and has e2fsck judge the result.

mod common;

use common::{Image, arg, create, ext4kit};
use std::{fs, path::Path, process::Output};
use tempfile::TempDir;

/// Runs `ext4kit edit IMAGE ARGS` and returns its output without judging it.
fn edit_output(image: &Path, args: &[&str]) -> Output {
    let image = arg(image);
    let mut full = vec!["edit", image.as_str()];
    full.extend_from_slice(args);
    ext4kit(&full)
}

/// Runs `ext4kit edit IMAGE ARGS`, panics on failure and returns stdout.
fn edit(image: &Path, args: &[&str]) -> Vec<u8> {
    let output = edit_output(image, args);
    assert!(
        output.status.success(),
        "edit {args:?} failed with {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

/// A small rootfs resembling what OpenShell builds before its fixups.
fn rootfs(root: &Path) {
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(
        root.join("etc/passwd"),
        "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534::/:/sbin/nologin\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::write(root.join("usr/bin/bash"), "#!elf\n").unwrap();
    fs::create_dir_all(root.join("opt/openshell/bin")).unwrap();
    fs::write(root.join("opt/openshell/bin/openshell-sandbox"), "#!elf\n").unwrap();
    fs::write(root.join("opt/openshell/.initialized"), "").unwrap();
    fs::create_dir_all(root.join("sandbox/work")).unwrap();
    fs::write(root.join("sandbox/work/notes.txt"), "notes\n").unwrap();
}

/// Creates an image from [`rootfs`] with every entry owned by root and
/// returns the temporary directory holding it and the image path.
fn rootfs_image() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("rootfs");
    rootfs(&src);
    let out = tmp.path().join("rootfs.img");
    // Import the contents of rootfs at / by adding each top-level entry.
    let mut args = vec!["--chown".to_owned(), "0:0".to_owned()];
    for entry in ["etc", "usr", "opt", "sandbox"] {
        args.push("--add".to_owned());
        args.push(arg(&src.join(entry)));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    create(&args, &out);
    (tmp, out)
}

#[test]
fn openshell_style_fixups_produce_a_clean_image() {
    let (tmp, img) = rootfs_image();

    edit(
        &img,
        &["chmod", "755", "/opt/openshell/bin/openshell-sandbox"],
    );
    for dir in ["/sandbox", "/image-cache", "/lower", "/overlay", "/newroot"] {
        edit(&img, &["mkdir", "-p", dir]);
        edit(&img, &["chmod", "0755", dir]);
    }
    edit(&img, &["rm", "-f", "/opt/openshell/.initialized"]);
    edit(&img, &["rm", "-f", "/opt/openshell/.initialized"]);
    edit(&img, &["ln", "-s", "usr/bin", "/bin"]);

    // Read the account file out, change it on the host, write it back.
    let passwd = String::from_utf8(edit(&img, &["cat", "/etc/passwd"])).unwrap();
    let updated = format!("{passwd}sandbox:x:1000:1000::/sandbox:/bin/bash\n");
    let host_passwd = tmp.path().join("passwd");
    fs::write(&host_passwd, &updated).unwrap();
    edit(&img, &["write", &arg(&host_passwd), "/etc/passwd"]);
    edit(&img, &["chown", "-R", "1000:1000", "/sandbox"]);

    // The rootfs validation OpenShell runs before booting. A file added from
    // Windows has no execute bit until it is set explicitly.
    let bash_executable = || {
        edit_output(&img, &["test", "-x", "/bin/bash"])
            .status
            .code()
    };
    if cfg!(windows) {
        assert_eq!(bash_executable(), Some(1));
    }
    edit(&img, &["chmod", "755", "/bin/bash"]);
    assert_eq!(bash_executable(), Some(0));
    assert!(
        edit_output(&img, &["test", "-f", "/usr/bin/bash"])
            .status
            .success()
    );
    assert_eq!(
        edit_output(&img, &["test", "-e", "/bin/sed"]).status.code(),
        Some(1)
    );

    let Some(image) = Image::check(&img) else {
        return;
    };
    assert_eq!(image.mode("/opt/openshell/bin/openshell-sandbox"), 0o755);
    let mut root = image.list("/");
    root.sort();
    assert_eq!(
        root,
        [
            "bin",
            "etc",
            "image-cache",
            "lost+found",
            "lower",
            "newroot",
            "opt",
            "overlay",
            "sandbox",
            "usr"
        ]
    );
    assert!(
        !image
            .list("/opt/openshell")
            .contains(&".initialized".to_owned())
    );
    assert_eq!(image.cat("/etc/passwd"), updated);
    for path in ["/sandbox", "/sandbox/work", "/sandbox/work/notes.txt"] {
        assert_eq!(image.owner(path), (1000, 1000), "{path}");
    }
    assert_eq!(image.owner("/etc/passwd"), (0, 0));
    assert_eq!(image.mode("/image-cache"), 0o755);
}

#[test]
fn remove_and_recreate_in_separate_calls() {
    let (tmp, img) = rootfs_image();
    let host = tmp.path().join("host");
    fs::create_dir_all(host.join("bin")).unwrap();
    fs::write(host.join("bin/openshell-vm-init"), "v2\n").unwrap();

    edit(&img, &["rm", "-r", "/opt/openshell/bin"]);
    edit(&img, &["test", "-e", "/opt/openshell"]);
    assert_eq!(
        edit_output(&img, &["test", "-e", "/opt/openshell/bin"])
            .status
            .code(),
        Some(1)
    );
    edit(
        &img,
        &[
            "add",
            "--chown",
            "0:0",
            &format!("{}:/opt/openshell", arg(&host.join("bin"))),
        ],
    );
    edit(
        &img,
        &["chmod", "755", "/opt/openshell/bin/openshell-vm-init"],
    );
    edit(
        &img,
        &[
            "mkdir",
            "--mode",
            "700",
            "--owner",
            "1000",
            "/opt/openshell/state",
        ],
    );
    edit(&img, &["ln", "/usr/bin/bash", "/usr/bin/bash-hardlink"]);

    let Some(image) = Image::check(&img) else {
        return;
    };
    assert_eq!(image.list("/opt/openshell/bin"), ["openshell-vm-init"]);
    assert_eq!(image.cat("/opt/openshell/bin/openshell-vm-init"), "v2\n");
    assert_eq!(image.mode("/opt/openshell/bin/openshell-vm-init"), 0o755);
    assert_eq!(image.mode("/opt/openshell/state"), 0o700);
    assert_eq!(image.owner("/opt/openshell/state"), (1000, 1000));
    assert!(image.stat("/usr/bin/bash").contains("Links: 2"));
}

#[test]
fn failed_edits_report_errors_and_leave_a_clean_image() {
    let (tmp, img) = rootfs_image();
    let big = tmp.path().join("big.bin");
    fs::write(&big, vec![0u8; 64 * 1024 * 1024]).unwrap();

    for args in [
        &["rm", "/missing"][..],
        &["rm", "/etc"],
        &["mkdir", "/etc"],
        &["mkdir", "/no/parent"],
        &["chmod", "755", "/missing"],
        &["write", &arg(&big), "/big.bin"],
        &["add", &arg(&big)],
        &["ln", "-s", "x", "/etc/passwd"],
        // e2fsck reports a zero-length symlink as invalid.
        &["ln", "-s", "", "/empty-link"],
    ] {
        let output = edit_output(&img, args);
        assert!(!output.status.success(), "edit {args:?} succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Error"),
            "edit {args:?}"
        );
    }
    // A broken image path is a failed check, not a false predicate.
    let missing = tmp.path().join("missing.img");
    assert_eq!(
        edit_output(&missing, &["test", "-e", "/"]).status.code(),
        Some(2)
    );

    let Some(image) = Image::check(&img) else {
        return;
    };
    assert!(!image.list("/").contains(&"big.bin".to_owned()));
    assert!(!image.list("/").contains(&"no".to_owned()));
    assert!(!image.list("/").contains(&"empty-link".to_owned()));
}

#[test]
fn filling_lost_and_found_leaves_a_clean_image() {
    // lost+found is the only multi-block linear directory.  Filling it past
    // its ~1022-record capacity used to commit the overflowing file before
    // add_dir_entry failed, so the image saved despite the error carried an
    // unattached inode that e2fsck reported.  The directory-growth preflight
    // now rejects the add before anything is committed, so this test does
    // not reach rollback_created_inode; unit tests cover that path.
    let (tmp, img) = rootfs_image();
    let lost = tmp.path().join("lost+found");
    fs::create_dir(&lost).unwrap();
    for i in 0..2000 {
        fs::write(lost.join(format!("f{i:04}")), b"").unwrap();
    }

    let output = edit_output(&img, &["add", &arg(&lost)]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("multi-block linear directory"),
        "stderr: {stderr}"
    );

    // `edit` saves the image even after a failed action, so the failed add
    // must have left every bitmap and free count untouched.
    let Some(image) = Image::check(&img) else {
        return;
    };
    assert!(
        image.list("/lost+found").len() > 1000,
        "the entries added before the failure must survive"
    );
}

#[test]
fn cat_and_test_work_on_a_read_only_image() {
    let (_tmp, img) = rootfs_image();
    let mut permissions = fs::metadata(&img).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&img, permissions.clone()).unwrap();

    let passwd = edit(&img, &["cat", "/etc/passwd"]);
    assert!(String::from_utf8(passwd).unwrap().starts_with("root:x:0:0"));
    edit(&img, &["test", "-d", "/sandbox"]);
    assert!(!edit_output(&img, &["mkdir", "/new"]).status.success());

    #[allow(clippy::permissions_set_readonly_false)] // Lets TempDir delete the image.
    permissions.set_readonly(false);
    fs::set_permissions(&img, permissions).unwrap();
}

#[test]
fn edit_rejects_images_not_made_by_ext4kit() {
    let tmp = TempDir::new().unwrap();
    let img = tmp.path().join("zeros.img");
    fs::write(&img, vec![0u8; 1024 * 1024]).unwrap();
    let output = edit_output(&img, &["mkdir", "/x"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not an ext4 image"));
    // Nothing is written to a file that could not be opened as an image.
    assert!(fs::read(&img).unwrap().iter().all(|&byte| byte == 0));
}
