use clap::{Args, Parser, Subcommand};
use ext4kit::{
    ext4::Ext4ImageBuilder,
    ext4::format::EXT4_ROOT_INO,
    host::HostTree,
    oci::{
        image_size_mib,
        local::{Engine, LocalImage},
        remote::RemoteImage,
    },
    vfs::edit::{EntryAttributes, PathTest},
};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Debug, Parser)]
#[command(name = "ext4kit", about = "Create and edit ext4 images without Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create an ext4 image containing the included file or directory.
    Create {
        /// Host file or directory, optionally followed by :/image/destination.
        /// Example: C:\\work\\hosts:/etc
        #[arg(
            long,
            value_name = "SOURCE[:/DESTINATION]",
            required_unless_present = "from"
        )]
        add: Vec<String>,

        /// Container image from a local engine when available, otherwise an OCI registry.
        #[arg(long, value_name = "IMAGE")]
        from: Option<String>,

        /// Destination ext4 image file.
        #[arg(value_name = "OUTPUT.IMG")]
        output: PathBuf,

        /// Output image size in MiB. Skips the source-tree sizing pass and
        /// overrides the base-image estimate.
        #[arg(long, value_name = "MIB")]
        size: Option<usize>,

        /// Numeric owner for everything imported with --add and for the
        /// destination directories created for it, as in a Dockerfile
        /// `COPY --chown`. A missing GID defaults to the UID. Entries from
        /// --from keep their image ownership.
        /// Example: --chown 1000:1000
        #[arg(long, value_name = "UID[:GID]", value_parser = parse_owner, requires = "add")]
        chown: Option<(u32, u32)>,

        /// Numeric owner of the filesystem root directory, like
        /// `mke2fs -E root_owner`, so a user can write at the top of the
        /// mounted disk. A missing GID defaults to the UID.
        /// Example: --root-owner 1000:1000
        #[arg(long, value_name = "UID[:GID]", value_parser = parse_owner)]
        root_owner: Option<(u32, u32)>,
    },

    /// Change an image created by ext4kit in place, one operation per call.
    /// Image paths are absolute; parent directories resolve through symlinks
    /// inside the image. A failed operation exits non-zero and leaves the
    /// image consistent.
    Edit {
        /// Image created by ext4kit.
        #[arg(value_name = "IMAGE.IMG")]
        image: PathBuf,

        #[command(subcommand)]
        action: EditAction,
    },
}

#[derive(Debug, Subcommand)]
enum EditAction {
    /// Remove a file or symlink, or a directory tree with -r. A final
    /// symlink is removed itself, never its target.
    Rm {
        /// Remove directories and their contents.
        #[arg(short = 'r', short_alias = 'R', long)]
        recursive: bool,
        /// Succeed when the path does not exist.
        #[arg(short, long)]
        force: bool,
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Create a directory (mode 0755 and root ownership unless given).
    Mkdir {
        /// Create missing parents; an existing directory is left unchanged.
        #[arg(short, long)]
        parents: bool,
        /// Octal permissions of the new directory.
        #[arg(short, long, value_parser = parse_mode)]
        mode: Option<u16>,
        /// Numeric owner of the new directory. A missing GID defaults to the UID.
        #[arg(long, value_name = "UID[:GID]", value_parser = parse_owner)]
        owner: Option<(u32, u32)>,
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Set octal permission bits, following symlinks.
    Chmod {
        #[arg(value_parser = parse_mode)]
        mode: u16,
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Set the numeric owner. Follows a final symlink unless -R is given;
    /// -R follows no symlinks at all, like `chown -R -P`.
    Chown {
        /// Change the whole tree below PATH.
        #[arg(short = 'R', long)]
        recursive: bool,
        /// A missing GID defaults to the UID.
        #[arg(value_name = "UID[:GID]", value_parser = parse_owner)]
        owner: (u32, u32),
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Import a host file or directory like `create --add`: a directory is
    /// merged into an existing one, anything else is replaced.
    Add {
        /// Numeric owner for the imported entries and the destination
        /// directories created for them. A missing GID defaults to the UID.
        #[arg(long, value_name = "UID[:GID]", value_parser = parse_owner)]
        chown: Option<(u32, u32)>,
        #[arg(value_name = "SOURCE[:/DESTINATION]")]
        source: String,
    },
    /// Write a host file (or stdin with -) to an image file. An existing
    /// file keeps its inode, links, timestamps and xattrs, and its mode and
    /// owner unless given; a new file is 0644 root:root unless given.
    Write {
        /// Octal permissions to set.
        #[arg(short, long, value_parser = parse_mode)]
        mode: Option<u16>,
        /// Numeric owner to set. A missing GID defaults to the UID.
        #[arg(long, value_name = "UID[:GID]", value_parser = parse_owner)]
        owner: Option<(u32, u32)>,
        /// Host file, or - for stdin.
        #[arg(value_name = "HOST_FILE")]
        source: PathBuf,
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Print an image file to stdout, following symlinks. Opens the image
    /// read-only.
    Cat {
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Exit 0 when the predicate holds and 1 when it does not, like
    /// test(1); 2 means the check itself failed. Opens the image read-only.
    Test {
        #[command(flatten)]
        predicate: TestPredicate,
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Create a hard link, or a symlink with -s, at LINK.
    Ln {
        /// Create a symlink; TARGET is stored as given and need not exist.
        #[arg(short, long)]
        symbolic: bool,
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(value_name = "LINK")]
        link: String,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct TestPredicate {
    /// PATH exists, following symlinks.
    #[arg(short = 'e')]
    exists: bool,
    /// PATH is a regular file, following symlinks.
    #[arg(short = 'f')]
    file: bool,
    /// PATH is a directory, following symlinks.
    #[arg(short = 'd')]
    directory: bool,
    /// PATH has an execute bit, following symlinks.
    #[arg(short = 'x')]
    executable: bool,
    /// PATH itself is a symlink.
    #[arg(short = 'L')]
    symlink: bool,
}

impl TestPredicate {
    fn path_test(&self) -> PathTest {
        match self {
            Self { exists: true, .. } => PathTest::Exists,
            Self { file: true, .. } => PathTest::File,
            Self {
                directory: true, ..
            } => PathTest::Directory,
            Self {
                executable: true, ..
            } => PathTest::Executable,
            _ => PathTest::Symlink,
        }
    }
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Create {
            add,
            from,
            output,
            size,
            chown,
            root_owner,
        } => run_create(add, from, output, size, chown, root_owner).map(|()| ExitCode::SUCCESS),
        Command::Edit { image, action } => run_edit(&image, action),
    };
    result.unwrap_or_else(|error| {
        eprintln!("Error: {error:?}");
        ExitCode::FAILURE
    })
}

fn run_create(
    add: Vec<String>,
    from: Option<String>,
    output: PathBuf,
    size: Option<usize>,
    chown: Option<(u32, u32)>,
    root_owner: Option<(u32, u32)>,
) -> anyhow::Result<()> {
    let additions: Vec<Addition> = add
        .iter()
        .map(|add| parse_add(add))
        .collect::<Result<_, _>>()?;
    let destination_directories = destination_directories(&additions);
    reject_target_collisions(&additions, &destination_directories)?;
    if size.is_some_and(|size| size < 16) {
        anyhow::bail!("--size must be at least 16 MiB");
    }
    // The runtime only drives engine and registry I/O; image import itself
    // runs on this thread and blocks on it.
    let base = match from.as_deref() {
        Some(image) => {
            let runtime = tokio::runtime::Runtime::new()?;
            let source = runtime.block_on(resolve_image(image))?;
            Some((runtime, source))
        }
        None => None,
    };
    let base_size = base
        .as_ref()
        .map(|(_, source)| source.size_mib())
        .transpose()?;
    let (image_size_mb, host_trees) = match size {
        Some(size) => {
            if let Some(estimate) = base_size.filter(|&estimate| estimate > size) {
                eprintln!(
                    "warning: --size {size} is below the base image estimate of {estimate} MiB"
                );
            }
            (size, None)
        }
        None => {
            let trees: Vec<HostTree> = additions
                .iter()
                .map(|addition| HostTree::scan(&addition.source))
                .collect::<Result<_, _>>()?;
            let host_size = Ext4ImageBuilder::recommended_image_size_mib_for_trees(
                &trees,
                destination_directories.len(),
            )?;
            let size = output_image_size_mib(host_size, base_size, !additions.is_empty())?;
            (size, Some(trees))
        }
    };
    println!(
        "Creating {image_size_mb}MiB ext4 filesystem image from {} addition(s)...",
        additions.len()
    );
    let mut builder = Ext4ImageBuilder::new_file(image_size_mb, &output)?;

    builder.create_root_dir()?;
    builder.create_lost_and_found()?;
    builder.reserve_inodes();
    builder.set_host_owner(chown);

    if let Some((runtime, source)) = &base {
        match source {
            ImageSource::Local(image) => {
                println!("Importing local {} image...", image.engine());
                image.import(&mut builder, runtime.handle())?;
            }
            ImageSource::Remote(image) => {
                println!("Importing OCI image...");
                image.import(&mut builder, runtime.handle())?;
            }
        }
    }

    // Import a directory before anything that needs it as a destination.  This
    // lets `--add b:/a` provide `/a/b` for a later `--add c:/a/b`.
    let mut import_order: Vec<_> = (0..additions.len()).collect();
    import_order.sort_by_key(|&index| additions[index].target().matches('/').count());
    for index in import_order {
        let tree = host_trees.as_ref().map(|trees| &trees[index]);
        import_addition(&mut builder, &additions[index], tree)?;
    }

    // Applied last so a root entry in a --from layer cannot override it.
    if let Some((uid, gid)) = root_owner {
        builder.set_inode_owner(EXT4_ROOT_INO, uid, gid)?;
    }

    builder.write_to_file(output.to_str().ok_or_else(|| {
        anyhow::anyhow!("output path is not valid UTF-8: {}", output.display())
    })?)?;

    println!("Ext4 image successfully generated at: {}", output.display());
    Ok(())
}

/// Imports one addition: a host directory is merged into an existing image
/// directory of the same name, anything else replaces the existing entry.
/// A pre-scanned `tree` is imported without walking the host again.
fn import_addition(
    builder: &mut Ext4ImageBuilder,
    addition: &Addition,
    tree: Option<&HostTree>,
) -> anyhow::Result<()> {
    let destination_parent = builder.ensure_host_destination(&addition.destination)?;
    let name = addition.name();
    match builder.lookup_dir_entry(destination_parent, name)? {
        Some(inode) if addition.is_directory()? && builder.is_directory(inode) => {
            builder.overlay_host_directory(inode, &addition.source)?;
        }
        _ => {
            builder.remove_dir_entry(destination_parent, name)?;
            match tree {
                Some(tree) => builder.import_host_tree(destination_parent, tree)?,
                None => builder.import_host_path_streaming(destination_parent, &addition.source)?,
            };
        }
    }
    Ok(())
}

/// Runs one `edit` action on `image`.  `cat` and `test` open the image
/// read-only.  Every other action saves the image afterwards, also when it
/// failed: builder operations validate before they commit, so the state in
/// memory is consistent even when a multi-step action such as `add` or
/// `mkdir -p` stops partway, and the data and directory blocks it already
/// wrote to the file need their matching bitmaps and inode tables.
fn run_edit(image: &Path, action: EditAction) -> anyhow::Result<ExitCode> {
    match action {
        EditAction::Cat { path } => {
            let builder = Ext4ImageBuilder::open_read_only(image)?;
            let mut stdout = io::BufWriter::new(io::stdout().lock());
            builder.cat_path(&path, &mut stdout)?;
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        EditAction::Test { predicate, path } => {
            let result = Ext4ImageBuilder::open_read_only(image)
                .and_then(|builder| builder.test_path(&path, predicate.path_test()));
            Ok(match result {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(error) => {
                    eprintln!("Error: {error:?}");
                    ExitCode::from(2)
                }
            })
        }
        action => {
            let image_path = image.to_str().ok_or_else(|| {
                anyhow::anyhow!("image path is not valid UTF-8: {}", image.display())
            })?;
            let mut builder = Ext4ImageBuilder::open(image)?;
            let result = apply_edit(&mut builder, action);
            let saved = builder.write_to_file(image_path);
            result?;
            saved?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Applies a mutating `edit` action to an opened image.
fn apply_edit(builder: &mut Ext4ImageBuilder, action: EditAction) -> anyhow::Result<()> {
    match action {
        EditAction::Rm {
            recursive,
            force,
            path,
        } => {
            if !builder.remove_path(&path, recursive)? && !force {
                anyhow::bail!("cannot remove {path}: no such file or directory");
            }
        }
        EditAction::Mkdir {
            parents,
            mode,
            owner,
            path,
        } => {
            builder.make_directory(&path, parents, EntryAttributes { mode, owner })?;
        }
        EditAction::Chmod { mode, path } => builder.chmod_path(&path, mode)?,
        EditAction::Chown {
            recursive,
            owner: (uid, gid),
            path,
        } => {
            builder.chown_path(&path, uid, gid, recursive)?;
        }
        EditAction::Add { chown, source } => {
            let addition = parse_add(&source)?;
            let tree = HostTree::scan(&addition.source)?;
            builder.ensure_room_for_host_tree(&tree)?;
            builder.set_host_owner(chown);
            import_addition(builder, &addition, Some(&tree))?;
        }
        EditAction::Write {
            mode,
            owner,
            source,
            path,
        } => {
            let attributes = EntryAttributes { mode, owner };
            if source.as_os_str() == "-" {
                // The size must be known before the first block is written.
                let mut data = Vec::new();
                io::stdin().lock().read_to_end(&mut data)?;
                builder.write_path(&path, &data[..], data.len() as u64, attributes)?;
            } else {
                let file = fs::File::open(&source).map_err(|error| {
                    anyhow::anyhow!("cannot open {}: {error}", source.display())
                })?;
                let size = file.metadata()?.len();
                builder.write_path(&path, file, size, attributes)?;
            }
        }
        EditAction::Ln {
            symbolic,
            target,
            link,
        } => {
            if symbolic {
                builder.symlink_path(&target, &link, EntryAttributes::default())?;
            } else {
                builder.hard_link_path(&target, &link)?;
            }
        }
        EditAction::Cat { .. } | EditAction::Test { .. } => {
            unreachable!("run_edit handles the read-only actions")
        }
    }
    Ok(())
}

enum ImageSource {
    Local(LocalImage),
    Remote(Box<RemoteImage>),
}

impl ImageSource {
    fn size_mib(&self) -> anyhow::Result<usize> {
        match self {
            Self::Local(image) => local_image_size_mib(image.size()),
            Self::Remote(image) => image.size_mib(),
        }
    }
}

/// Resolves explicit transports first; a plain name favors a locally present
/// image and otherwise uses the registry without ever pulling through an engine.
async fn resolve_image(image: &str) -> anyhow::Result<ImageSource> {
    match requested_transport(image)? {
        RequestedTransport::Local(engine, name) => {
            let image = LocalImage::find(name, &[engine], true)
                .await?
                .expect("strict lookup reports a missing image as an error");
            Ok(ImageSource::Local(image))
        }
        RequestedTransport::Remote => Ok(ImageSource::Remote(Box::new(
            RemoteImage::resolve(image).await?,
        ))),
        RequestedTransport::Auto => {
            match LocalImage::find(image, &[Engine::Podman, Engine::Docker], false).await? {
                Some(local) => Ok(ImageSource::Local(local)),
                None => Ok(ImageSource::Remote(Box::new(
                    RemoteImage::resolve(image).await?,
                ))),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RequestedTransport<'a> {
    Local(Engine, &'a str),
    Remote,
    Auto,
}

fn requested_transport(image: &str) -> anyhow::Result<RequestedTransport<'_>> {
    Ok(if let Some(name) = image.strip_prefix("docker-daemon:") {
        RequestedTransport::Local(Engine::Docker, name)
    } else if let Some(name) = image.strip_prefix("containers-storage:") {
        RequestedTransport::Local(Engine::Podman, name)
    } else if image.starts_with("docker://") {
        RequestedTransport::Remote
    } else if image.starts_with("oci:") {
        // In containers tooling `oci:` names a local OCI layout directory.
        anyhow::bail!("OCI layout directories are not supported; use docker://REFERENCE")
    } else {
        RequestedTransport::Auto
    })
}

fn local_image_size_mib(size: u64) -> anyhow::Result<usize> {
    // Keep the computation integral: floor(size * 1.2) plus the buffer, then
    // round the resulting filesystem allocation up to a MiB.
    let payload = size
        .checked_mul(6)
        .ok_or_else(|| anyhow::anyhow!("local image size calculation overflow"))?
        / 5;
    image_size_mib(payload, 1)
}

/// Combines the independent base-image and host-tree sizing estimates.
fn output_image_size_mib(
    host_size: usize,
    image_size: Option<usize>,
    has_host_additions: bool,
) -> anyhow::Result<usize> {
    match (image_size, has_host_additions) {
        (Some(image_size), true) => host_size
            .checked_add(image_size)
            .ok_or_else(|| anyhow::anyhow!("combined image size overflows usize")),
        (Some(image_size), false) => Ok(image_size),
        (None, _) => Ok(host_size),
    }
}

/// One source entry and the absolute directory where its basename is placed.
#[derive(Debug)]
struct Addition {
    source: PathBuf,
    destination: String,
}

impl Addition {
    /// The source basename, validated while parsing.
    fn name(&self) -> &str {
        self.source
            .file_name()
            .and_then(|name| name.to_str())
            .expect("additions validate their source name while parsing")
    }

    fn target(&self) -> String {
        if self.destination == "/" {
            format!("/{}", self.name())
        } else {
            format!("{}/{}", self.destination, self.name())
        }
    }

    fn is_directory(&self) -> anyhow::Result<bool> {
        Ok(fs::symlink_metadata(&self.source)
            .map_err(|error| {
                anyhow::anyhow!(
                    "cannot inspect add source {}: {error}",
                    self.source.display()
                )
            })?
            .file_type()
            .is_dir())
    }
}

/// Parses `host-path` or `host-path:/absolute/image/path` into one addition.
/// A bare host path targets the image root; a `C:/` Windows drive prefix is
/// not treated as a destination separator.
fn parse_add(add: &str) -> anyhow::Result<Addition> {
    let destination_separator = add
        .rfind(":/")
        .filter(|&index| !is_windows_drive_separator(add, index));
    match destination_separator {
        Some(separator) => {
            let source = &add[..separator];
            let destination = normalize_destination(&add[separator + 1..])?;
            if source.is_empty() {
                anyhow::bail!("add source cannot be empty");
            }
            let source = PathBuf::from(normalize_verbatim_source(source));
            validate_add_source(&source)?;
            Ok(Addition {
                source,
                destination,
            })
        }
        None => {
            let source = PathBuf::from(normalize_verbatim_source(add));
            validate_add_source(&source)?;
            Ok(Addition {
                source,
                destination: "/".to_owned(),
            })
        }
    }
}

/// Rewrites `/` to `\` after a `\\?\` prefix. Verbatim paths reach the
/// filesystem without Win32 normalization, where `/` is an ordinary
/// filename byte, so `\\?\C:/dir` cannot be opened even though the same
/// slashes are fine in `C:/dir` and get normalized in `//?/C:/dir`.
fn normalize_verbatim_source(source: &str) -> String {
    match source.strip_prefix(r"\\?\") {
        Some(rest) if rest.contains('/') => format!(r"\\?\{}", rest.replace('/', "\\")),
        _ => source.to_owned(),
    }
}

/// Parses a `--chown` or `--root-owner` `UID[:GID]`. Only numeric IDs are accepted: the host has no
/// account database for the guest, and names in a --from image's /etc/passwd
/// need not match the users of the VM the image is attached to.
fn parse_owner(value: &str) -> Result<(u32, u32), String> {
    let parse_id = |id: &str, kind: &str| {
        id.parse::<u32>()
            .map_err(|_| format!("{kind} must be a number from 0 to 4294967295, got {id:?}"))
    };
    let (uid, gid) = match value.split_once(':') {
        Some((uid, gid)) => (parse_id(uid, "UID")?, parse_id(gid, "GID")?),
        None => {
            let uid = parse_id(value, "UID")?;
            (uid, uid)
        }
    };
    Ok((uid, gid))
}

/// Parses octal permission bits such as `755` or `0644`, up to `7777`.
fn parse_mode(value: &str) -> Result<u16, String> {
    u16::from_str_radix(value, 8)
        .ok()
        .filter(|&mode| mode <= 0o7777)
        .ok_or_else(|| format!("mode must be octal from 0 to 7777, got {value:?}"))
}

/// Rejects filesystem roots, which have no name to place in the image.
fn validate_add_source(source: &Path) -> anyhow::Result<()> {
    match source.file_name().and_then(|name| name.to_str()) {
        Some(_) => Ok(()),
        None if source.parent().is_none() => anyhow::bail!(
            "add source is a filesystem root and cannot be imported as a named image entry: {}",
            source.display()
        ),
        None => anyhow::bail!(
            "add source has no valid UTF-8 file name: {}",
            source.display()
        ),
    }
}

/// Returns whether `index` is the `:/` portion of a Windows `C:/` prefix.
fn is_windows_drive_separator(value: &str, index: usize) -> bool {
    let Some(&drive) = value.as_bytes().get(index.saturating_sub(1)) else {
        return false;
    };
    if !drive.is_ascii_alphabetic() {
        return false;
    }
    let prefix = &value[..index - 1];
    prefix.is_empty() || prefix.ends_with("\\\\?\\") || prefix.ends_with("//?/")
}

/// Canonicalizes a CLI destination to an absolute slash-separated image path.
fn normalize_destination(destination: &str) -> anyhow::Result<String> {
    if !destination.starts_with('/') {
        anyhow::bail!("destination must be an absolute image path: {destination}");
    }
    let components: Vec<_> = destination
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    if components
        .iter()
        .any(|component| *component == "." || *component == "..")
    {
        anyhow::bail!("destination cannot contain '.' or '..': {destination}");
    }
    Ok(if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    })
}

/// Returns every distinct directory that must be created for `additions`.
fn destination_directories(additions: &[Addition]) -> HashSet<String> {
    additions
        .iter()
        .flat_map(|addition| {
            let mut path = String::new();
            addition
                .destination
                .split('/')
                .filter(|component| !component.is_empty())
                .map(move |component| {
                    path.push('/');
                    path.push_str(component);
                    path.clone()
                })
        })
        .collect()
}

/// Rejects additions whose basenames would map to the same image path.
fn reject_target_collisions(
    additions: &[Addition],
    destination_directories: &HashSet<String>,
) -> anyhow::Result<()> {
    let mut targets = HashSet::new();
    for addition in additions {
        let target = addition.target();
        if !targets.insert(target.clone()) {
            anyhow::bail!("multiple additions target {target}");
        }
        if destination_directories.contains(&target) && !addition.is_directory()? {
            anyhow::bail!(
                "addition targets {target}, which is also needed as another addition's destination directory"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Addition, Cli, Command, EditAction, PathTest, RequestedTransport, destination_directories,
        local_image_size_mib, output_image_size_mib, parse_add, parse_mode, parse_owner,
        reject_target_collisions, requested_transport,
    };
    use clap::Parser;
    use ext4kit::oci::local::Engine;
    use std::{fs, path::PathBuf};

    #[test]
    fn accepts_explicit_image_size() {
        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--add",
            "source",
            "--size",
            "3200",
            "output.img",
        ])
        .unwrap();
        let Command::Create { size, .. } = cli.command else {
            panic!("expected create")
        };
        assert_eq!(size, Some(3200));
    }

    #[test]
    fn owner_accepts_numeric_uid_and_optional_gid() {
        assert_eq!(parse_owner("1000:1001"), Ok((1000, 1001)));
        assert_eq!(parse_owner("1000"), Ok((1000, 1000)));
        assert_eq!(parse_owner("0:4294967295"), Ok((0, u32::MAX)));
        assert!(parse_owner("test").is_err());
        assert!(parse_owner("1000:").is_err());
        assert!(parse_owner(":1000").is_err());
        assert!(parse_owner("-1").is_err());
        assert!(parse_owner("4294967296").is_err());
    }

    #[test]
    fn chown_requires_host_additions() {
        let error = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--from",
            "alpine:latest",
            "--chown",
            "1000",
            "output.img",
        ])
        .unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--add",
            "source",
            "--chown",
            "1000:1000",
            "output.img",
        ])
        .unwrap();
        let Command::Create { chown, .. } = cli.command else {
            panic!("expected create")
        };
        assert_eq!(chown, Some((1000, 1000)));
    }

    #[test]
    fn root_owner_is_accepted_without_host_additions() {
        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--from",
            "alpine:latest",
            "--root-owner",
            "1000",
            "output.img",
        ])
        .unwrap();
        let Command::Create { root_owner, .. } = cli.command else {
            panic!("expected create")
        };
        assert_eq!(root_owner, Some((1000, 1000)));
    }

    #[test]
    fn accepts_oci_image_without_host_additions() {
        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--from",
            "docker.io/library/alpine:latest",
            "--size",
            "64",
            "output.img",
        ])
        .unwrap();
        let Command::Create { from, add, .. } = cli.command else {
            panic!("expected create")
        };
        assert_eq!(from.as_deref(), Some("docker.io/library/alpine:latest"));
        assert!(add.is_empty());
    }

    #[test]
    fn accepts_local_transport_without_host_additions() {
        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--from",
            "docker-daemon:alpine:latest",
            "--size",
            "64",
            "output.img",
        ])
        .unwrap();
        let Command::Create { from, add, .. } = cli.command else {
            panic!("expected create")
        };
        assert_eq!(from.as_deref(), Some("docker-daemon:alpine:latest"));
        assert!(add.is_empty());
    }

    #[test]
    fn accepts_from_and_add_without_an_explicit_size() {
        let cli = Cli::try_parse_from([
            "ext4kit",
            "create",
            "--from",
            "docker://example/image",
            "--add",
            "host:/opt",
            "output.img",
        ])
        .unwrap();
        let Command::Create {
            from, add, size, ..
        } = cli.command
        else {
            panic!("expected create")
        };
        assert_eq!(from.as_deref(), Some("docker://example/image"));
        assert_eq!(add, vec![String::from("host:/opt")]);
        assert_eq!(size, None);
    }

    #[test]
    fn routes_explicit_transports() {
        assert_eq!(
            requested_transport("docker://example/image").unwrap(),
            RequestedTransport::Remote
        );
        assert_eq!(
            requested_transport("docker-daemon:alpine:latest").unwrap(),
            RequestedTransport::Local(Engine::Docker, "alpine:latest")
        );
        assert_eq!(
            requested_transport("containers-storage:alpine").unwrap(),
            RequestedTransport::Local(Engine::Podman, "alpine")
        );
        assert_eq!(
            requested_transport("alpine:latest").unwrap(),
            RequestedTransport::Auto
        );
        assert!(requested_transport("oci:/tmp/layout").is_err());
    }

    #[test]
    fn local_size_adds_twenty_percent_and_a_512_mib_buffer() {
        assert_eq!(local_image_size_mib(100 * 1024 * 1024).unwrap(), 632);
    }

    #[test]
    fn sizing_combines_from_image_and_host_additions_without_explicit_size() {
        assert_eq!(output_image_size_mib(128, Some(640), true).unwrap(), 768);
    }

    #[test]
    fn sizing_uses_only_the_from_image_when_there_are_no_host_additions() {
        assert_eq!(output_image_size_mib(128, Some(640), false).unwrap(), 640);
    }

    #[test]
    fn sizing_uses_the_host_tree_estimate_without_a_from_image() {
        assert_eq!(output_image_size_mib(128, None, true).unwrap(), 128);
    }

    #[test]
    fn add_destination_preserves_windows_drive_prefix() {
        let addition = parse_add(r"C:\\work\\hosts:/etc").unwrap();
        assert_eq!(
            addition.source,
            std::path::PathBuf::from(r"C:\\work\\hosts")
        );
        assert_eq!(addition.destination, "/etc");
    }

    #[cfg(windows)]
    #[test]
    fn add_without_destination_preserves_forward_slash_drive_path() {
        let addition = parse_add("C:/work/hosts").unwrap();
        assert_eq!(addition.source, std::path::PathBuf::from("C:/work/hosts"));
        assert_eq!(addition.destination, "/");
    }

    #[test]
    fn extended_windows_drive_path_is_not_a_destination_separator() {
        let addition = parse_add(r"\\?\C:/work/hosts").unwrap();
        assert_eq!(addition.source, PathBuf::from(r"\\?\C:\work\hosts"));
        assert_eq!(addition.destination, "/");
        let addition = parse_add(r"\\?\C:/work/hosts:/etc").unwrap();
        assert_eq!(addition.source, PathBuf::from(r"\\?\C:\work\hosts"));
        assert_eq!(addition.destination, "/etc");
        let addition = parse_add(r"\\?\C:\work\hosts").unwrap();
        assert_eq!(addition.source, PathBuf::from(r"\\?\C:\work\hosts"));
        assert_eq!(addition.destination, "/");
    }

    #[test]
    fn root_add_source_has_a_specific_error() {
        let error = parse_add("/").unwrap_err();
        assert!(error.to_string().contains("filesystem root"));
    }

    #[test]
    fn directory_target_can_supply_another_destination_path() {
        let base = std::env::temp_dir().join(format!("ext4kit-main-test-{}", std::process::id()));
        let directory = base.join("b");
        fs::create_dir_all(&directory).unwrap();
        let additions = vec![
            Addition {
                source: directory,
                destination: "/a".to_owned(),
            },
            Addition {
                source: PathBuf::from("c"),
                destination: "/a/b".to_owned(),
            },
        ];
        let directories = destination_directories(&additions);
        assert!(directories.contains("/a/b"));
        assert!(reject_target_collisions(&additions, &directories).is_ok());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn parses_octal_modes() {
        assert_eq!(parse_mode("755"), Ok(0o755));
        assert_eq!(parse_mode("0644"), Ok(0o644));
        assert_eq!(parse_mode("4755"), Ok(0o4755));
        for bad in ["", "8", "10000", "rwx", "-1"] {
            assert!(parse_mode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_edit_actions() {
        let cli =
            Cli::try_parse_from(["ext4kit", "edit", "img", "chown", "-R", "1000", "/sandbox"])
                .unwrap();
        let Command::Edit {
            action: EditAction::Chown {
                recursive, owner, ..
            },
            ..
        } = cli.command
        else {
            panic!("expected edit chown");
        };
        assert!(recursive);
        assert_eq!(owner, (1000, 1000));

        let cli = Cli::try_parse_from(["ext4kit", "edit", "img", "test", "-x", "/bin/sh"]).unwrap();
        let Command::Edit {
            action: EditAction::Test { predicate, .. },
            ..
        } = cli.command
        else {
            panic!("expected edit test");
        };
        assert_eq!(predicate.path_test(), PathTest::Executable);
    }

    #[test]
    fn edit_test_needs_exactly_one_predicate() {
        for args in [&["/bin/sh"][..], &["-e", "-x", "/bin/sh"]] {
            let mut full = vec!["ext4kit", "edit", "img", "test"];
            full.extend_from_slice(args);
            assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
        }
    }
}
