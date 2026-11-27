//! The create-time orchestration of folder tracking: the checks that run
//! before the output is truncated (old-image gates, overlap, repository
//! discovery, the dirty check, the label) and the retirement of an old
//! image's tracking. The CLI parses flags and calls in; notes and warnings
//! keep going to stdout/stderr from here.

use super::{
    Mapping, MappingKind, normalize_host_path,
    sidecar::{self, Sidecar},
};
use crate::ext4::reader::ImageReader;
use crate::host::git::{self, Repo};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// One `--add` source whose imported content may be tracked, already
/// classified by the caller: the host source, the image-side target it
/// lands at, and whether it is a directory. A library type, so the
/// library never sees the CLI's own addition parsing.
pub struct TrackedSource {
    pub source: PathBuf,
    pub target: String,
    pub is_dir: bool,
}

/// The create-time tracking switches, bundled to keep the create flow
/// callable.
#[derive(Debug, Default)]
pub struct TrackingFlags {
    /// Whether the user asked for tracking at all. Without it nothing
    /// below the old-image gates runs — no git call, no dirty check, no
    /// base refs, no sidecar — while the gates and the retirement of an
    /// existing sidecar keep working.
    pub track: bool,
    pub label: Option<String>,
    pub include_ignored: bool,
    pub replace_mounted: bool,
    pub discard_unsnapshotted: bool,
    pub verbose: bool,
}

/// The tracking state `create` carries between its pure checks and the
/// base refs written after the image exists.
#[derive(Debug)]
pub struct TrackingPrepared {
    label: String,
    include_ignored: bool,
    /// Every `--add` mapping; untracked ones carry `repo: None`.
    mappings: Vec<Mapping>,
    /// The repositories to write a base ref into, in mapping order.
    repos: Vec<PendingRepo>,
}

impl TrackingPrepared {
    /// The tracking label the image will use.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether files git ignores are part of the baseline.
    pub fn include_ignored(&self) -> bool {
        self.include_ignored
    }

    /// Every `--add` mapping, untracked ones included (`repo: None`).
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// The repositories that need a base ref, in mapping order.
    pub fn repos(&self) -> &[PendingRepo] {
        &self.repos
    }
}

/// One repository discovered through the mappings. Its root is
/// normalized like the mapping host paths, so it compares equal to the
/// `Mapping::repo` that produced it.
#[derive(Debug)]
pub struct PendingRepo {
    root: String,
    dirty_at_create: bool,
}

impl PendingRepo {
    /// The repository root, normalized like the mapping host paths.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Whether the working tree was dirty at create time, which blocks
    /// `cherry-pick` until the user commits or stashes.
    pub fn dirty_at_create(&self) -> bool {
        self.dirty_at_create
    }
}

/// The output's existing sidecar, with whether it actually describes
/// this output's image: a foreign sidecar owns no refs here.
#[derive(Debug)]
pub struct OldTracking {
    sidecar: Sidecar,
    foreign: bool,
}

/// The create-time tracking checks that run before the output is
/// truncated: the two-sided overlap check, repo discovery, the dirty
/// check with the artifact advisory, and the label. A returned `None`
/// means this image is not tracked (no `--add`, no git, overlapping
/// targets, or every mapping outside a repository). The old sidecar is
/// handed back so the caller can retire it after the network work —
/// a registry pull failure must leave the old image exactly as it was,
/// still tracked.
pub fn prepare_tracking(
    sources: &[TrackedSource],
    output: &Path,
    flags: &TrackingFlags,
) -> Result<(Option<TrackingPrepared>, Option<OldTracking>)> {
    let sidecar_path = Sidecar::path_for(output);
    let old_sidecar = if sidecar_path.exists() {
        Some(Sidecar::load(output)?)
    } else {
        None
    };
    // The old image's refusals gate every path from here on, including
    // the ones that cannot track: without this, a missing git would let a
    // live tracked image be truncated with no mounted check at all.
    let ownership = check_old_image_state(output, old_sidecar.as_ref(), flags)?;
    let old_tracking = old_sidecar.map(|sidecar| OldTracking {
        sidecar,
        foreign: matches!(ownership, SidecarOwnership::Foreign { .. }),
    });
    if let SidecarOwnership::Foreign { found, expected } = ownership {
        eprintln!(
            "warning: the sidecar of {} records image {expected} but the image is \
             {found}; its refs are left alone",
            output.display()
        );
    }

    if !flags.track {
        // Tracking is opt-in: without the flag, create only writes the
        // image. The gates above still ran (they need no git), and an
        // existing sidecar is still retired by the caller.
        return Ok((None, old_tracking));
    }
    if sources.is_empty() {
        // No --add content: nothing to track, but an existing sidecar
        // still has to be retired with its refs.
        return Ok((None, old_tracking));
    }
    if !git::git_available() {
        eprintln!("warning: git is not on PATH; folder tracking is disabled for this image");
        return Ok((None, old_tracking));
    }

    // Step 1: overlap — image side and host side. Overlaps disable
    // tracking but do not fail the create.
    let image_overlap = sources.iter().any(|a| {
        sources
            .iter()
            .any(|b| a.target != b.target && under(&a.target, &b.target))
    });
    if image_overlap {
        eprintln!("warning: overlapping --add targets: folder tracking disabled for this image");
        return Ok((None, old_tracking));
    }

    // Step 2: repository discovery per mapping.
    let mut mappings: Vec<Mapping> = Vec::new();
    let mut untracked = Vec::new();
    for source in sources {
        let repo = match git::discover_repo(&source.source)? {
            Some(repo) => Some(repo),
            None => {
                untracked.push(source.source.display().to_string());
                None
            }
        };
        let (repo, repo_relative) = match &repo {
            Some(repo) => match repo_relative_path(&source.source, &repo.root) {
                Some(repo_relative) => (Some(normalize_host_path(&repo.root)), repo_relative),
                // A drive alias (Windows `subst`) that git and
                // `fs::canonicalize` spell differently would otherwise
                // turn the whole host path into a bogus git path.
                None => {
                    eprintln!(
                        "note: cannot place {} under its repository {}; this mapping \
                         is not tracked",
                        source.source.display(),
                        repo.root.display()
                    );
                    (None, String::new())
                }
            },
            None => (None, String::new()),
        };
        mappings.push(Mapping {
            host: normalize_host_path(&source.source),
            image: source.target.clone(),
            kind: if source.is_dir {
                MappingKind::Dir
            } else {
                MappingKind::File
            },
            repo,
            repo_relative,
        });
    }
    for source in untracked {
        eprintln!("note: {source} is not inside a git repository; this mapping is not tracked");
    }
    let host_overlap = mappings.iter().enumerate().any(|(a, first)| {
        mappings.iter().enumerate().any(|(b, second)| {
            a < b
                && first.repo.is_some()
                && first.repo == second.repo
                && (first.repo_relative == second.repo_relative
                    || under(&second.repo_relative, &first.repo_relative)
                    || under(&first.repo_relative, &second.repo_relative))
        })
    });
    if host_overlap {
        eprintln!(
            "warning: overlapping --add host paths in one repository: \
             folder tracking disabled for this image"
        );
        return Ok((None, old_tracking));
    }
    if !mappings.iter().any(|mapping| mapping.repo.is_some()) {
        // Every mapping is outside a repository: nothing to track, and
        // the notes above already said so.
        return Ok((None, old_tracking));
    }

    // Step 3: the dirty check (never writing the index) and the
    // artifact-directory advisory, per repository.
    let mut repos: Vec<PendingRepo> = Vec::new();
    for mapping in &mappings {
        let Some(root) = &mapping.repo else {
            continue;
        };
        if repos.iter().any(|repo| &repo.root == root) {
            continue;
        }
        let repo = Repo::at(root);
        let subtrees: Vec<String> = mappings
            .iter()
            .filter(|m| m.repo.as_deref() == Some(root.as_str()))
            .map(|m| m.repo_relative.clone())
            .filter(|path| !path.is_empty())
            .collect();
        let dirty = !repo.dirty_paths(&subtrees)?.is_empty();
        if dirty {
            eprintln!(
                "warning: uncommitted changes in {root} are part of the baseline. \
                 Commit or stash them before applying a snapshot: cherry-pick \
                 refuses to overwrite local changes, and agent edits to files that \
                 are untracked now need `git apply`. Commit first for the smoothest \
                 review."
            );
        }
        advise_artifact_directories(&repo, &mappings, root);
        repos.push(PendingRepo {
            root: root.clone(),
            dirty_at_create: dirty,
        });
    }

    // Step 4: the label. An explicit --label is validated — the user
    // chose it — while the default is derived by transformation, so an
    // image named "My Repo.img" or "con.img" never fails a create over
    // its own file name. Refs the old sidecar owns are moved away by the
    // retirement below, so they do not collide — but only in the
    // repositories that sidecar actually tracked, and never when the
    // sidecar is foreign: in any other case a same-named ref belongs to a
    // different image and must collide.
    let label = match flags.label.as_deref() {
        Some(label) => sidecar::sanitize_label(label)?,
        None => sidecar::derived_label(&default_label(output)),
    };
    for repo in &repos {
        let handle = Repo::at(&repo.root);
        // A foreign sidecar owns no refs, so it exempts nothing: a
        // same-named ref then belongs to a different image and collides.
        let owned_by_old_sidecar = old_tracking
            .as_ref()
            .filter(|old| !old.foreign)
            .is_some_and(|old| {
                old.sidecar.label == label
                    && old
                        .sidecar
                        .repos
                        .iter()
                        .any(|record| same_repository(&record.root, &repo.root))
            });
        if handle.label_in_use(&label)? && !owned_by_old_sidecar {
            bail!(
                "label {label} is in use in {}; pass --label to choose another",
                handle.root.display()
            );
        }
    }

    Ok((
        Some(TrackingPrepared {
            label,
            include_ignored: flags.include_ignored,
            mappings,
            repos,
        }),
        old_tracking,
    ))
}

/// The label an image gets when `--label` is absent: its file name
/// without the extension, so `out.img` tracks as `out`.
fn default_label(output: &Path) -> String {
    output.file_stem().map_or_else(
        || output.to_string_lossy().into_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// Whether `path` is `dir` or lies under it, for slash-separated paths.
fn under(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.starts_with(&format!("{dir}/"))
}

/// `path` with symlinks and Windows 8.3/drive aliases resolved where the
/// filesystem allows, normalized for comparison; the plain normalized
/// spelling when canonicalization fails.
fn canonical_normalized(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|path| normalize_host_path(&path))
        .unwrap_or_else(|_| normalize_host_path(path))
}

/// Whether two spellings name the same checkout. Git reports canonical
/// paths while a mapping or sidecar written elsewhere may hold an 8.3 or
/// aliased one, so compare the canonicalized, normalized forms.
fn same_repository(left: &str, right: &str) -> bool {
    canonical_normalized(Path::new(left)) == canonical_normalized(Path::new(right))
}

/// The host source's path relative to its repository root, normalized;
/// empty when the source is the root. Both sides are canonicalized
/// first, so a drive alias that git and `fs::canonicalize` spell
/// differently still strips. `None` when the source cannot be placed
/// under the root: the caller then leaves the mapping untracked rather
/// than writing the whole host path as a git path.
fn repo_relative_path(source: &Path, repo_root: &Path) -> Option<String> {
    let source = canonical_normalized(source);
    let root = canonical_normalized(repo_root);
    let relative = source.strip_prefix(&root)?;
    if relative.is_empty() {
        return Some(String::new());
    }
    // The separator check keeps a shared name prefix (`repo2` under
    // `repo`) from becoming a relative path, except at a drive/root
    // whose normalized form already ends in `/`.
    let relative = if root.ends_with('/') {
        relative
    } else {
        relative.strip_prefix('/')?
    };
    Some(relative.to_owned())
}

/// Well-known artifact directories the advisory looks for: display only,
/// because the repository's declared policy is the only policy.
const ARTIFACT_DIRECTORIES: [&str; 6] = [
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
];

fn advise_artifact_directories(repo: &Repo, mappings: &[Mapping], root: &str) {
    for mapping in mappings.iter().filter(|m| m.repo.as_deref() == Some(root)) {
        if mapping.kind != MappingKind::Dir {
            continue;
        }
        for name in ARTIFACT_DIRECTORIES {
            let host_dir = Path::new(&mapping.host).join(name);
            if !host_dir.is_dir() {
                continue;
            }
            let repo_relative = if mapping.repo_relative.is_empty() {
                name.to_owned()
            } else {
                format!("{}/{}", mapping.repo_relative, name)
            };
            let ignored = repo.is_ignored(&repo_relative).unwrap_or(false);
            let tracked = repo.tracked_in_head(&repo_relative).unwrap_or(false);
            if !ignored && !tracked {
                eprintln!(
                    "note: {name}/ is neither ignored nor tracked; it will appear \
                     in every snapshot diff — consider adding it to .gitignore"
                );
            }
        }
    }
}

/// Whether the output's existing sidecar describes this output's image.
#[derive(Debug)]
enum SidecarOwnership {
    /// No sidecar, no image to compare against, or a matching UUID: the
    /// refs it lists belong to this image.
    Owned,
    /// The sidecar records a different image's UUID — a copied sidecar or
    /// a wrapper mix-up. Its refs are not this image's and are never
    /// moved or discarded.
    Foreign { found: String, expected: String },
}

/// The refusals that guard the output's existing tracked state: the
/// mounted check, the UUID ownership check and the unsnapshotted-work
/// refusal. These run before every path through `prepare_tracking` —
/// including the ones that cannot track (no `--add`, no git) — because
/// truncating a mounted image or discarding unsnapshotted work must
/// never happen silently. Needs no git: only the image and the sidecar
/// are read.
fn check_old_image_state(
    output: &Path,
    old_sidecar: Option<&Sidecar>,
    flags: &TrackingFlags,
) -> Result<SidecarOwnership> {
    let Some(sidecar) = old_sidecar else {
        return Ok(SidecarOwnership::Owned);
    };
    // The state check runs before anything is published: taking a final
    // snapshot of a live filesystem first would be reading torn state.
    if output.exists() {
        // A corrupt or busy image fails safe, but the raw reader error
        // never mentions the sidecar that is keeping the gate closed; a
        // user with an unrecoverable image needs the manual way out.
        let (reader, state) = ImageReader::open(output).with_context(|| {
            format!(
                "cannot inspect the old image {} while the sidecar {} still records \
                 its tracked folders; stop the process using the image or restore it, \
                 or delete the sidecar if its tracking is no longer needed",
                output.display(),
                Sidecar::path_for(output).display()
            )
        })?;
        if state.mounted && !flags.replace_mounted {
            bail!(
                "the old image {} is mounted or was not cleanly unmounted; stop the \
                 VM before re-creating (if the VM is known to be dead, run e2fsck \
                 or pass --replace-mounted)",
                output.display()
            );
        }
        // A sidecar whose UUID does not match the image describes another
        // image's tracking: refusing to re-create over it would block on
        // work this image never held, while retiring its refs would
        // destroy another image's history. It is reported and ignored.
        let found = sidecar::uuid_hex(&reader.fs_uuid());
        if found != sidecar.image_uuid {
            return Ok(SidecarOwnership::Foreign {
                found,
                expected: sidecar.image_uuid.clone(),
            });
        }
    }
    // The final snapshot of the old image lands with the `snapshot`
    // command, so until then any unsnapshotted work can only be
    // discarded, never silently.
    if !flags.discard_unsnapshotted {
        bail!(
            "{} has tracked folders; re-creating it would discard any changes \
             that were never snapshotted. Pass --discard-unsnapshotted to discard \
             them, or run `ext4kit snapshot` first once it exists",
            output.display()
        );
    }
    Ok(SidecarOwnership::Owned)
}

/// Retires the output's existing tracked state before the image is
/// truncated: the old refs move to `previous/<unix-ts>/` (when git is
/// available; without it they are left in place with a warning, since
/// nothing else can move them) and the old sidecar is removed. A foreign
/// sidecar describes another image's tracking, so only its file goes —
/// its refs are never moved.
pub fn retire_old_tracking(output: &Path, old: Option<&OldTracking>) -> Result<()> {
    let Some(old) = old else {
        return Ok(());
    };
    if !old.foreign {
        let sidecar = &old.sidecar;
        eprintln!(
            "note: unsnapshotted changes in {} are being discarded",
            output.display()
        );
        if git::git_available() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            for record in &sidecar.repos {
                // A checkout that was deleted or is offline has no refs
                // to move; say so instead of spawning git against it.
                if !Path::new(&record.root).is_dir() {
                    eprintln!(
                        "note: the repository {} of {} no longer exists; its previous \
                         refs cannot be moved",
                        record.root, sidecar.label
                    );
                    continue;
                }
                let repo = Repo::at(&record.root);
                let moved = repo.move_label_refs_to_previous(&sidecar.label, timestamp)?;
                for (reference, sha) in moved {
                    println!(
                        "previous snapshots of {} kept at {reference} ({sha})",
                        sidecar.label
                    );
                }
            }
        } else {
            eprintln!(
                "warning: git is not on PATH; the previous tracking refs of {} were \
                 left in place under refs/ext4kit/{}/",
                output.display(),
                sidecar.label
            );
        }
    }
    fs::remove_file(Sidecar::path_for(output)).map_err(|error| {
        anyhow::anyhow!(
            "cannot remove the old sidecar {}: {error}",
            Sidecar::path_for(output).display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::format::EXT4_FEATURE_INCOMPAT_RECOVER;
    use crate::tracked::sidecar::RepoRecord;

    /// Writes a 16 MiB image at `path` and returns its filesystem UUID.
    /// `mounted` sets the journal's needs-recovery bit, the state a VM
    /// that still has the image open leaves behind.
    fn image(path: &Path, mounted: bool) -> [u8; 16] {
        let mut builder = crate::ext4::test_support::fresh_builder().unwrap();
        if mounted {
            builder.superblock.s_feature_incompat |= EXT4_FEATURE_INCOMPAT_RECOVER;
        }
        let uuid = builder.superblock.s_uuid;
        builder.write_to_file(path.to_str().unwrap()).unwrap();
        uuid
    }

    fn sidecar_of(image_uuid: &[u8; 16], label: &str) -> Sidecar {
        Sidecar {
            tool: "ext4kit test".to_owned(),
            created_unix: 0,
            image_uuid: sidecar::uuid_hex(image_uuid),
            label: label.to_owned(),
            include_ignored: false,
            mappings: Vec::new(),
            repos: Vec::new(),
        }
    }

    fn source(path: &Path, target: &str) -> TrackedSource {
        TrackedSource {
            source: path.to_owned(),
            target: target.to_owned(),
            is_dir: true,
        }
    }

    fn flags() -> TrackingFlags {
        TrackingFlags {
            track: true,
            ..Default::default()
        }
    }

    /// Runs one git command in `repo`; true on success.
    fn git(repo: &Repo, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .output()
            .expect("git runs")
            .status
            .success()
    }

    /// An initialized repository with one commit, or `None` (with a note)
    /// when git is missing, unless `EXT4KIT_REQUIRE_GIT` is set — the
    /// same skip pattern as the other git-backed unit tests.
    fn repo_fixture() -> Option<(tempfile::TempDir, Repo)> {
        if !git::git_available() {
            if std::env::var_os("EXT4KIT_REQUIRE_GIT").is_some() {
                panic!("EXT4KIT_REQUIRE_GIT is set but git was not found");
            }
            eprintln!("note: git not found; skipping git-backed unit tests");
            return None;
        }
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::at(directory.path());
        assert!(git(&repo, &["init", "--quiet"]));
        assert!(git(&repo, &["config", "user.email", "t@example.com"]));
        assert!(git(&repo, &["config", "user.name", "T"]));
        assert!(git(&repo, &["config", "commit.gpgsign", "false"]));
        assert!(git(&repo, &["config", "core.autocrlf", "false"]));
        std::fs::write(directory.path().join("a.txt"), "a\n").unwrap();
        assert!(git(&repo, &["add", "-A"]));
        assert!(git(&repo, &["commit", "--quiet", "-m", "init"]));
        Some((directory, repo))
    }

    fn head(repo: &Repo) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn refs(repo: &Repo) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["for-each-ref", "--format=%(refname)", "refs/ext4kit/"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn tracking_is_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        let src = directory.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let (prepared, old) = prepare_tracking(
            &[source(&src, "/opt/src")],
            &output,
            &TrackingFlags::default(),
        )
        .unwrap();
        assert!(prepared.is_none());
        assert!(old.is_none());
    }

    #[test]
    fn no_sources_prepare_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        let (prepared, old) = prepare_tracking(&[], &output, &flags()).unwrap();
        assert!(prepared.is_none());
        assert!(old.is_none());
    }

    #[test]
    fn unsnapshotted_work_is_refused_until_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        sidecar_of(&image(&output, false), "out")
            .write(&output)
            .unwrap();

        let error = prepare_tracking(&[], &output, &TrackingFlags::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("--discard-unsnapshotted"), "{error}");
        // The refusal left both the image and the sidecar in place.
        assert_eq!(Sidecar::load(&output).unwrap().label, "out");
    }

    #[test]
    fn discarding_unsnapshotted_work_hands_the_sidecar_back() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        sidecar_of(&image(&output, false), "out")
            .write(&output)
            .unwrap();
        let flags = TrackingFlags {
            discard_unsnapshotted: true,
            ..Default::default()
        };
        let (prepared, old) = prepare_tracking(&[], &output, &flags).unwrap();
        assert!(prepared.is_none());
        let old = old.expect("the old sidecar is handed back for retirement");
        assert_eq!(old.sidecar.label, "out");
        assert!(!old.foreign);
    }

    #[test]
    fn a_sidecar_recording_another_image_owns_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        let _uuid = image(&output, false);
        sidecar_of(&[0x11; 16], "other").write(&output).unwrap();

        // The foreign check returns before the unsnapshotted refusal: the
        // work this sidecar describes is not this image's to discard.
        let (prepared, old) = prepare_tracking(&[], &output, &TrackingFlags::default()).unwrap();
        assert!(prepared.is_none());
        let old = old.expect("the foreign sidecar is reported");
        assert!(old.foreign);
        // Retirement removes the file; a foreign sidecar's refs are never
        // moved, so retirement needs no git at all.
        retire_old_tracking(&output, Some(&old)).unwrap();
        assert!(!Sidecar::path_for(&output).exists());
    }

    #[test]
    fn a_mounted_old_image_is_refused_until_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        sidecar_of(&image(&output, true), "out")
            .write(&output)
            .unwrap();

        let error = prepare_tracking(&[], &output, &TrackingFlags::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("mounted or was not cleanly unmounted"),
            "{error}"
        );
        // Both gates pass only when explicitly overridden.
        let flags = TrackingFlags {
            replace_mounted: true,
            discard_unsnapshotted: true,
            ..Default::default()
        };
        let (prepared, old) = prepare_tracking(&[], &output, &flags).unwrap();
        assert!(prepared.is_none());
        assert!(old.is_some());
    }

    #[test]
    fn retiring_without_an_old_sidecar_does_nothing() {
        let directory = tempfile::tempdir().unwrap();
        retire_old_tracking(&directory.path().join("out.img"), None).unwrap();
    }

    #[test]
    fn retiring_an_owned_sidecar_moves_its_refs_and_removes_the_file() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let base = head(&repo);
        assert!(git(&repo, &["update-ref", "refs/ext4kit/out/base", &base]));
        let mut record = sidecar_of(&image(&output, false), "out");
        record.repos.push(RepoRecord {
            root: normalize_host_path(&repo.root),
            base_commit: base,
            dirty_at_create: false,
        });
        record.write(&output).unwrap();

        retire_old_tracking(
            &output,
            Some(&OldTracking {
                sidecar: record,
                foreign: false,
            }),
        )
        .unwrap();
        assert!(!Sidecar::path_for(&output).exists());
        assert!(!repo.label_in_use("out").unwrap());
        assert!(
            refs(&repo).contains("refs/ext4kit/out/previous/"),
            "{}",
            refs(&repo)
        );
    }

    #[test]
    fn a_repository_mapping_is_planned_with_its_dirty_state() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("Out.img");
        let sources = [source(directory.path(), "/opt/src")];
        let (prepared, old) = prepare_tracking(&sources, &output, &flags()).unwrap();
        assert!(old.is_none());
        let prepared = prepared.expect("a mapping inside a repository is tracked");
        // The default label is derived from the image name.
        assert_eq!(prepared.label(), "out");
        assert!(!prepared.include_ignored());
        assert_eq!(prepared.repos().len(), 1);
        // Git and the fixture may spell the same root differently (8.3
        // short names on a Windows runner); compare spellings canonically.
        let expected = normalize_host_path(&repo.root);
        assert!(same_repository(prepared.repos()[0].root(), &expected));
        assert!(!prepared.repos()[0].dirty_at_create());
        assert!(same_repository(
            prepared.mappings()[0].repo.as_deref().unwrap(),
            &expected
        ));

        // An uncommitted file makes the working tree dirty at create.
        std::fs::write(directory.path().join("b.txt"), "b\n").unwrap();
        let (prepared, _) = prepare_tracking(&sources, &output, &flags()).unwrap();
        assert!(prepared.unwrap().repos()[0].dirty_at_create());
    }

    #[test]
    fn a_mapping_outside_a_repository_is_not_tracked() {
        let Some(_) = repo_fixture() else {
            return;
        };
        let outside = tempfile::tempdir().unwrap();
        let output = outside.path().join("out.img");
        let (prepared, old) =
            prepare_tracking(&[source(outside.path(), "/opt/x")], &output, &flags()).unwrap();
        assert!(prepared.is_none());
        assert!(old.is_none());
    }

    #[test]
    fn overlapping_targets_disable_tracking() {
        let Some((directory, _repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let sources = [
            source(directory.path(), "/opt"),
            source(directory.path(), "/opt/sub"),
        ];
        let (prepared, _) = prepare_tracking(&sources, &output, &flags()).unwrap();
        assert!(prepared.is_none());
    }

    #[test]
    fn overlapping_host_paths_disable_tracking() {
        let Some((directory, _repo)) = repo_fixture() else {
            return;
        };
        std::fs::create_dir_all(directory.path().join("sub/deep")).unwrap();
        let output = directory.path().join("out.img");
        let sources = [
            source(&directory.path().join("sub"), "/opt/a"),
            source(&directory.path().join("sub/deep"), "/opt/b"),
        ];
        let (prepared, _) = prepare_tracking(&sources, &output, &flags()).unwrap();
        assert!(prepared.is_none());
    }

    #[test]
    fn a_label_in_use_is_refused() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let base = head(&repo);
        assert!(git(&repo, &["update-ref", "refs/ext4kit/out/base", &base]));
        let output = directory.path().join("out.img");
        let error = prepare_tracking(&[source(directory.path(), "/opt/src")], &output, &flags())
            .unwrap_err()
            .to_string();
        assert!(error.contains("label out is in use"), "{error}");
    }

    #[test]
    fn the_old_sidecar_exempts_its_own_label() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let base = head(&repo);
        assert!(git(&repo, &["update-ref", "refs/ext4kit/out/base", &base]));
        let mut record = sidecar_of(&image(&output, false), "out");
        record.repos.push(RepoRecord {
            root: normalize_host_path(&repo.root),
            base_commit: base,
            dirty_at_create: false,
        });
        record.write(&output).unwrap();
        let flags = TrackingFlags {
            discard_unsnapshotted: true,
            ..flags()
        };
        let (prepared, old) =
            prepare_tracking(&[source(directory.path(), "/opt/src")], &output, &flags).unwrap();
        assert!(prepared.is_some());
        assert!(!old.unwrap().foreign);
    }

    #[test]
    fn a_foreign_sidecar_does_not_exempt_the_label() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let _uuid = image(&output, false);
        let base = head(&repo);
        assert!(git(&repo, &["update-ref", "refs/ext4kit/out/base", &base]));
        let mut record = sidecar_of(&[0x22; 16], "out");
        record.repos.push(RepoRecord {
            root: normalize_host_path(&repo.root),
            base_commit: base,
            dirty_at_create: false,
        });
        record.write(&output).unwrap();
        let error = prepare_tracking(&[source(directory.path(), "/opt/src")], &output, &flags())
            .unwrap_err()
            .to_string();
        assert!(error.contains("label out is in use"), "{error}");
    }

    #[test]
    fn an_explicit_label_is_validated() {
        let Some((directory, _repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let flags = TrackingFlags {
            label: Some("bad/label".to_owned()),
            ..flags()
        };
        let error = prepare_tracking(&[source(directory.path(), "/opt/src")], &output, &flags)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not usable as a git ref component"),
            "{error}"
        );
    }

    #[test]
    fn under_compares_whole_path_components() {
        assert!(under("/opt", ""));
        assert!(under("/opt", "/opt"));
        assert!(under("/opt/sub", "/opt"));
        assert!(!under("/optx", "/opt"));
        assert!(!under("/opt", "/opt/sub"));
    }

    #[test]
    fn default_labels_come_from_the_file_stem() {
        assert_eq!(default_label(Path::new("out.img")), "out");
        assert_eq!(
            sidecar::derived_label(&default_label(Path::new("My Repo.img"))),
            "my-repo"
        );
    }

    #[test]
    fn repo_relative_paths_strip_the_repository_root() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("lib");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            repo_relative_path(&nested, directory.path()),
            Some("lib".to_owned())
        );
        assert_eq!(
            repo_relative_path(directory.path(), directory.path()),
            Some(String::new())
        );

        // A shared name prefix is not a directory boundary.
        let sibling = directory.path().join("lib2");
        std::fs::create_dir_all(&sibling).unwrap();
        assert_eq!(repo_relative_path(&sibling, &nested), None);
        // A path outside the root never becomes a git path.
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(repo_relative_path(outside.path(), directory.path()), None);
    }

    #[test]
    fn repository_spellings_compare_by_their_canonical_form() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let dotted = child.join("..");
        assert!(same_repository(
            &normalize_host_path(directory.path()),
            &normalize_host_path(&dotted)
        ));
        // A sibling with a shared name prefix is a different checkout.
        let sibling = directory.path().join("child2");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!same_repository(
            &normalize_host_path(&child),
            &normalize_host_path(&sibling)
        ));
    }

    #[test]
    fn a_corrupt_image_names_the_sidecar_and_its_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.img");
        sidecar_of(&image(&output, false), "out")
            .write(&output)
            .unwrap();
        std::fs::write(&output, b"not an ext4 image").unwrap();

        let error = prepare_tracking(&[], &output, &TrackingFlags::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains(".ext4kit-tracked"), "{error}");
        assert!(error.contains("delete the sidecar"), "{error}");
        // Failing safe: the sidecar is untouched.
        assert!(Sidecar::path_for(&output).exists());
    }

    #[test]
    fn a_vanished_repository_root_does_not_block_retirement() {
        let Some((directory, repo)) = repo_fixture() else {
            return;
        };
        let output = directory.path().join("out.img");
        let vanished = directory.path().join("deleted-checkout");
        let mut record = sidecar_of(&image(&output, false), "out");
        record.repos.push(RepoRecord {
            root: normalize_host_path(&vanished),
            base_commit: head(&repo),
            dirty_at_create: false,
        });
        record.write(&output).unwrap();

        // Nothing can be moved from a checkout that is gone, but the
        // sidecar is still retired, so a later create is not blocked.
        retire_old_tracking(
            &output,
            Some(&OldTracking {
                sidecar: record,
                foreign: false,
            }),
        )
        .unwrap();
        assert!(!Sidecar::path_for(&output).exists());
    }
}
