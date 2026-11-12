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
pub enum E2fsprogs {
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
        // The probes retry WSL transport failures like every other call: the
        // result is cached for the whole run, so one transient -1 here would
        // otherwise silently skip every e2fsck check.
        let resolve = |distro: &str| -> Option<String> {
            let output = try_output_with_retry(
                Command::new("wsl").args(["-d", distro, "-e", "sh", "-c", "command -v e2fsck"]),
                true,
            )
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
        let listing = try_output_with_retry(Command::new("wsl").args(["-l", "-q"]), true).ok()?;
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

    /// Spawns one e2fsprogs tool (`mkfs.ext4`, `dumpe2fs`, ...).
    pub fn command(&self, program: &str) -> Command {
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
    pub fn target(&self, image: &Path) -> String {
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

    /// The e2fsprogs directory as a shell prefix: `"/usr/sbin/"` when the
    /// tools live in a known directory, `""` when they are only on PATH.
    fn bin_prefix(&self) -> String {
        match self {
            Self::Native { sbin: Some(dir) } => format!("{}/", dir.display()),
            Self::Native { sbin: None } => String::new(),
            Self::Wsl { sbin, .. } => format!("{sbin}/"),
        }
    }
}

/// Runs `command`, retrying WSL transport failures. Under parallel test load
/// the WSL service intermittently fails a client outright with exit code -1
/// (`Wsl/Service/E_UNEXPECTED`, `0x8007274c` timeouts); the e2fsprogs tools
/// never exit -1, so the signature is unambiguous. Native runs pass `false`
/// and never retry.
fn output_with_retry(command: &mut Command, retry_wsl_failures: bool) -> std::process::Output {
    try_output_with_retry(command, retry_wsl_failures).expect("failed to run e2fsprogs")
}

/// [`output_with_retry`] for probes, where a program that cannot be spawned
/// at all (no `wsl` on PATH) is an answer rather than a test failure.
fn try_output_with_retry(
    command: &mut Command,
    retry_wsl_failures: bool,
) -> std::io::Result<std::process::Output> {
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 0..3 {
        let output = command.output()?;
        if !retry_wsl_failures || output.status.code() != Some(-1) || attempt == 2 {
            return Ok(output);
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

/// The raw e2fsprogs tool access, for tests that need more than
/// [`Image`]: `command(program)` spawns a tool, `target(path)` converts a
/// host path into the tool's view. Returns `None` like [`Image::check`]
/// when e2fsprogs is unavailable.
pub fn e2fsprogs_tools() -> Option<&'static E2fsprogs> {
    e2fsprogs()
}

/// Runs one e2fsprogs tool with `args`, retrying WSL transport failures
/// like [`Image::check`] does. Panics if the tool cannot run at all.
pub fn run_tool(tools: &E2fsprogs, program: &str, args: &[&str]) -> std::process::Output {
    let mut command = tools.command(program);
    command.args(args);
    output_with_retry(&mut command, tools.is_wsl())
}

/// Runs `sh -c SCRIPT` as root, for fixtures that need a loop mount (the
/// mounted / frozen / synced journal states only the kernel produces).
/// `${BIN}mkfs.ext4` inside the script expands to the located e2fsprogs
/// tool (slash-terminated prefix, or empty when the tools are only on
/// PATH); commands like `fsfreeze` come from util-linux, not e2fsprogs, so
/// they are resolved by the shell's PATH like `mount` and `sync` — never
/// prefixed with `${BIN}`.
///
/// On WSL this runs `wsl -u root`. Natively it runs the shell directly when
/// already root, otherwise through passwordless `sudo -n`, so CI runners
/// (GitHub Actions) run the fixtures too. A native machine with neither
/// returns `None` with a note, so `cargo test` stays green for developers
/// without passwordless sudo, unless `EXT4KIT_REQUIRE_E2FSPROGS` is set, in
/// which case the missing root access fails the test instead.
pub fn root_shell(tools: &E2fsprogs, script: &str) -> Option<std::process::Output> {
    let script = script.replace("${BIN}", &tools.bin_prefix());
    let mut command = match tools {
        E2fsprogs::Wsl { distro, .. } => {
            let mut command = Command::new("wsl");
            command.args(["-d", distro, "-u", "root", "-e", "sh", "-c"]);
            command
        }
        E2fsprogs::Native { .. } => {
            let succeeds = |program: &str, args: &[&str]| {
                Command::new(program)
                    .args(args)
                    .output()
                    .is_ok_and(|output| output.status.success())
            };
            if succeeds("sh", &["-c", "test \"$(id -u)\" = 0"]) {
                let mut command = Command::new("sh");
                command.arg("-c");
                command
            } else if succeeds("sudo", &["-n", "true"]) {
                let mut command = Command::new("sudo");
                command.args(["-n", "sh", "-c"]);
                command
            } else {
                if std::env::var_os("EXT4KIT_REQUIRE_E2FSPROGS").is_some() {
                    panic!(
                        "EXT4KIT_REQUIRE_E2FSPROGS is set but root fixtures need root \
                         or passwordless sudo"
                    );
                }
                eprintln!("note: no root or passwordless sudo; skipping loop-mount fixtures");
                return None;
            }
        }
    };
    command.arg(&script);
    Some(output_with_retry(&mut command, tools.is_wsl()))
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
