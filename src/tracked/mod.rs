//! The tracked-folder layer: turning the state of an ext4 image into git
//! commits, so an agent's work inside a VM can be reviewed and applied
//! with ordinary git commands.
//!
//! The layer is read-only on the image: everything flows through
//! [`crate::ext4::reader::ImageReader`], never the builder. Names read
//! from the image are agent-controlled bytes and pass through
//! [`names::validate_component`] before they become git paths; see the
//! snapshot plan's threat model.

pub mod commit;
pub mod create;
pub mod names;
pub mod scan;
pub mod sidecar;

use std::path::Path;

/// Whether a mapping's host source is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingKind {
    Dir,
    File,
}

/// One `--add` mapping whose imported content is tracked: the host source
/// and the image-side target it was imported to, plus where the source
/// sits inside its git repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// The host source, normalized by [`normalize_host_path`].
    pub host: String,
    /// The image-side target, e.g. `/opt/src` (the `Addition::target()` of
    /// the create call).
    pub image: String,
    pub kind: MappingKind,
    /// The root of the repository containing the host source, normalized,
    /// or `None` when the source is not inside a repository (untracked).
    pub repo: Option<String>,
    /// The host source's path relative to its repository root,
    /// `/`-separated with no leading or trailing separator; empty when the
    /// source is the repository root.
    pub repo_relative: String,
}

impl Mapping {
    /// The repo-relative git path of the entry scanned at `rel_path` (a
    /// path relative to the mapping's image target).
    pub fn repo_path(&self, rel_path: &str) -> String {
        if self.repo_relative.is_empty() {
            rel_path.to_owned()
        } else if rel_path.is_empty() {
            self.repo_relative.clone()
        } else {
            format!("{}/{}", self.repo_relative, rel_path)
        }
    }
}

/// Normalizes a host path for the sidecar and ref bookkeeping, so the
/// same directory always serializes to one string: on Windows, `/`
/// separators, an upper-case drive letter and no `\\?\` prefix; on every
/// platform, no trailing separator. Elsewhere `\` is a legal filename
/// byte, so none of the Windows rewriting may run there.
pub fn normalize_host_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        text = text.replace('\\', "/");
        if let Some(rest) = text
            .strip_prefix("//?/")
            .filter(|rest| rest.len() >= 2 && rest.as_bytes()[1] == b':')
        {
            text = rest.to_owned();
        }
        // Upper-case the drive letter, like the sidecar examples in the
        // plan.
        if text.len() >= 2 && text.as_bytes()[1] == b':' && text.as_bytes()[0].is_ascii_alphabetic()
        {
            let drive = text[..1].to_uppercase();
            text = format!("{drive}{}", &text[1..]);
        }
    }
    while text.len() > 3 && text.ends_with('/') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    fn repo_paths_prepend_the_mapping_prefix() {
        let root = Mapping {
            host: "C:/src".to_owned(),
            image: "/opt/src".to_owned(),
            kind: MappingKind::Dir,
            repo: Some("C:/src".to_owned()),
            repo_relative: String::new(),
        };
        assert_eq!(root.repo_path("lib/a.rs"), "lib/a.rs");
        let nested = Mapping {
            host: "C:/work/repo".to_owned(),
            image: "/opt/repo".to_owned(),
            kind: MappingKind::Dir,
            repo: Some("C:/work".to_owned()),
            repo_relative: "repo".to_owned(),
        };
        assert_eq!(nested.repo_path("lib/a.rs"), "repo/lib/a.rs");
        assert_eq!(nested.repo_path(""), "repo");
    }

    #[test]
    fn host_paths_normalize_to_forward_slashes() {
        // The Windows rewrites are Windows-only.
        #[cfg(windows)]
        {
            assert_eq!(
                normalize_host_path(&PathBuf::from(r"C:\work\src")),
                "C:/work/src"
            );
            assert_eq!(
                normalize_host_path(&PathBuf::from(r"\\?\C:\work\src")),
                "C:/work/src"
            );
            // A drive root keeps its separator.
            assert_eq!(normalize_host_path(Path::new("C:/")), "C:/");
        }
        assert_eq!(normalize_host_path(Path::new("/tmp/x/")), "/tmp/x");
        // A backslash is a legal filename byte everywhere else.
        #[cfg(unix)]
        assert_eq!(normalize_host_path(Path::new("a\\b")), "a\\b");
    }
}
