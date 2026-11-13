//! Shared helpers for the end-to-end tests: run the `ext4kit` binary, then
//! judge the image with e2fsprogs, natively or through WSL on Windows.
//!
//! When e2fsprogs is missing the e2fsprogs checks are skipped with a notice,
//! unless `EXT4KIT_REQUIRE_E2FSPROGS` is set (as in CI), which turns the
//! absence into a test failure.

#![allow(dead_code)] // Each test crate uses a different subset of the helpers.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

/// Runs `ext4kit` with `args` and returns its output without judging it.
pub fn ext4kit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ext4kit"))
        .args(args)
        .output()
        .expect("failed to spawn ext4kit")
}

/// Runs `ext4kit create ARGS OUTPUT` and panics with its output on failure.
pub fn create(args: &[&str], output: &Path) {
    let output_arg = arg(output);
    let mut full = vec!["create"];
    full.extend_from_slice(args);
    full.push(&output_arg);
    let result = ext4kit(&full);
    assert!(
        result.status.success(),
        "ext4kit {full:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
}

/// Formats a host path as a CLI argument. Forward slashes keep `--add`
/// destination parsing unambiguous on Windows.
pub fn arg(path: &Path) -> String {
    path.to_str()
        .expect("test paths are UTF-8")
        .replace('\\', "/")
}

/// The e2fsprogs installation used to inspect images.
enum E2fsprogs {
    Native { sbin: Option<PathBuf> },
    Wsl { distro: String, sbin: String },
}

impl E2fsprogs {
    fn locate() -> Option<Self> {
        if let Some(found) = Self::native() {
            return Some(found);
        }
        if cfg!(windows) {
            return Self::wsl();
        }
        None
    }

    /// e2fsck may live in an sbin directory that is not on an unprivileged PATH.
    fn native() -> Option<Self> {
        let candidates = [None, Some("/usr/sbin"), Some("/sbin")];
        candidates.into_iter().find_map(|sbin| {
            let sbin = sbin.map(PathBuf::from);
            let program = match &sbin {
                Some(dir) => dir.join("e2fsck"),
                None => PathBuf::from("e2fsck"),
            };
            Command::new(program)
                .arg("-V")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| Self::Native { sbin })
        })
    }

    fn wsl() -> Option<Self> {
        // Resolve e2fsck through a shell and keep the directory it reports:
        // `wsl -e NAME` resolves bare names with a PATH that can lack
        // /usr/sbin even though the child environment contains it, so
        // later calls exec the absolute path instead.
        let resolve = |distro: &str| -> Option<String> {
            let output = Command::new("wsl")
                .args(["-d", distro, "-e", "sh", "-c", "command -v e2fsck"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            // Keep the directory as a plain string: joining with `PathBuf`
            // on Windows would inject a `\` into the Linux path.
            path.rsplit_once('/')
                .map(|(dir, _)| dir.to_owned())
                .filter(|dir| !dir.is_empty())
        };
        if let Ok(distro) = std::env::var("EXT4KIT_WSL_DISTRO") {
            return resolve(&distro).map(|sbin| Self::Wsl { distro, sbin });
        }
        // `wsl -l` prints UTF-16LE; dropping NUL bytes is enough for distro names.
        let listing = Command::new("wsl").args(["-l", "-q"]).output().ok()?;
        let listing: String = String::from_utf8_lossy(&listing.stdout)
            .chars()
            .filter(|&c| c != '\0')
            .collect();
        listing
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .find_map(|name| {
                resolve(name).map(|sbin| Self::Wsl {
                    distro: name.to_owned(),
                    sbin,
                })
            })
    }

    fn command(&self, program: &str) -> Command {
        match self {
            Self::Native { sbin } => Command::new(match sbin {
                Some(dir) => dir.join(program),
                None => PathBuf::from(program),
            }),
            Self::Wsl { distro, sbin } => {
                let mut command = Command::new("wsl");
                command.args(["-d", distro, "-e"]);
                command.arg(format!("{sbin}/{program}"));
                command
            }
        }
    }

    /// Converts a host path into the path the e2fsprogs tools see.
    fn target(&self, image: &Path) -> String {
        match self {
            Self::Native { .. } => arg(image),
            Self::Wsl { distro, .. } => {
                let mut command = Command::new("wsl");
                command.args(["-d", distro, "-e", "wslpath", "-a", &arg(image)]);
                let output = output_with_retry(&mut command, true);
                assert!(output.status.success(), "wslpath failed: {output:?}");
                String::from_utf8(output.stdout)
                    .expect("wslpath prints UTF-8")
                    .trim()
                    .to_owned()
            }
        }
    }

    fn is_wsl(&self) -> bool {
        matches!(self, Self::Wsl { .. })
    }
}

/// Runs `command`, retrying WSL transport failures. Under parallel test load
/// the WSL service intermittently fails a client outright with exit code -1
/// (`Wsl/Service/E_UNEXPECTED`, `0x8007274c` timeouts); the e2fsprogs tools
/// never exit -1, so the signature is unambiguous. Native runs pass `false`
/// and never retry.
fn output_with_retry(command: &mut Command, retry_wsl_failures: bool) -> std::process::Output {
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 0..3 {
        let output = command.output().expect("failed to run e2fsprogs");
        if !retry_wsl_failures || output.status.code() != Some(-1) || attempt == 2 {
            return output;
        }
        std::thread::sleep(backoff);
        backoff *= 2;
    }
    unreachable!()
}

fn e2fsprogs() -> Option<&'static E2fsprogs> {
    static FOUND: OnceLock<Option<E2fsprogs>> = OnceLock::new();
    let found = FOUND.get_or_init(E2fsprogs::locate).as_ref();
    if found.is_none() {
        if std::env::var_os("EXT4KIT_REQUIRE_E2FSPROGS").is_some() {
            panic!("EXT4KIT_REQUIRE_E2FSPROGS is set but e2fsck was not found");
        }
        eprintln!("note: e2fsprogs not found; skipping e2fsck/debugfs checks");
    }
    found
}

/// An image produced by a test, inspected through e2fsprogs.
pub struct Image {
    tools: Option<&'static E2fsprogs>,
    target: String,
}

impl Image {
    /// Runs `e2fsck -fn` and panics on any finding. Returns `None` when
    /// e2fsprogs is unavailable and the caller should skip its inspections.
    pub fn check(path: &Path) -> Option<Self> {
        let tools = e2fsprogs()?;
        let target = tools.target(path);
        // -f forces a full check of a clean filesystem; -n opens it read-only.
        let mut command = tools.command("e2fsck");
        let output = output_with_retry(command.args(["-fn", &target]), tools.is_wsl());
        assert!(
            output.status.success(),
            "e2fsck reported problems ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Some(Self {
            tools: Some(tools),
            target,
        })
    }

    /// Runs one `debugfs -R` request and returns its stdout. debugfs reports
    /// request errors on stderr with a zero exit code, so those are returned
    /// separately for callers that expect them.
    pub fn debugfs_with_stderr(&self, request: &str) -> (String, String) {
        let tools = self.tools.expect("Image is only built with tools");
        let mut command = tools.command("debugfs");
        let output = output_with_retry(command.args(["-R", request, &self.target]), tools.is_wsl());
        assert!(output.status.success(), "debugfs {request:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| !line.starts_with("debugfs "))
            .collect::<Vec<_>>()
            .join("\n");
        (String::from_utf8_lossy(&output.stdout).into_owned(), stderr)
    }

    /// Runs one `debugfs -R` request and panics if it reports an error.
    pub fn debugfs(&self, request: &str) -> String {
        let (stdout, stderr) = self.debugfs_with_stderr(request);
        assert!(stderr.trim().is_empty(), "debugfs {request:?}: {stderr}");
        stdout
    }

    /// Returns the raw bytes of the file at `path` inside the image.
    pub fn cat(&self, path: &str) -> String {
        self.debugfs(&format!("cat \"{path}\""))
    }

    /// Returns the `debugfs stat` output for `path`.
    pub fn stat(&self, path: &str) -> String {
        self.debugfs(&format!("stat \"{path}\""))
    }

    /// Returns `(uid, gid)` parsed from `debugfs stat`.
    pub fn owner(&self, path: &str) -> (u32, u32) {
        let stat = self.stat(path);
        let field = |name: &str| -> u32 {
            let rest = stat
                .split(name)
                .nth(1)
                .unwrap_or_else(|| panic!("no {name} in stat output:\n{stat}"));
            rest.split_whitespace().next().unwrap().parse().unwrap()
        };
        (field("User:"), field("Group:"))
    }

    /// Returns the octal permission bits parsed from `debugfs stat`.
    pub fn mode(&self, path: &str) -> u32 {
        let stat = self.stat(path);
        let rest = stat.split("Mode:").nth(1).expect("no Mode in stat output");
        u32::from_str_radix(rest.split_whitespace().next().unwrap(), 8).unwrap()
    }

    /// Returns the entry names of directory `path`, excluding `.` and `..`.
    pub fn list(&self, path: &str) -> Vec<String> {
        // `ls -p` prints one `/inode/mode/uid/gid/name/size/` record per line.
        self.debugfs(&format!("ls -p \"{path}\""))
            .lines()
            .filter_map(|line| line.split('/').nth(5).map(str::to_owned))
            .filter(|name| !name.is_empty() && name != "." && name != "..")
            .collect()
    }
}
