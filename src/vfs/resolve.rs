//! Path resolution inside the image, following symlinks like a chrooted
//! path walk.

use crate::ext4::format::*;
use crate::ext4::{Ext4ImageBuilder, PosixMetadata};
use anyhow::Result;

/// Linux MAXSYMLINKS: the kernel gives up with ELOOP after 40 links.
const MAX_SYMLINK_HOPS: usize = 40;

impl Ext4ImageBuilder {
    /// Returns the target of a symlink inode.
    pub fn read_symlink(&self, ino: u32) -> Result<String> {
        let inode = self.inode(ino);
        if inode.file_type() != S_IFLNK {
            return Err(anyhow::anyhow!("inode {ino} is not a symlink"));
        }
        let size = inode.i_size_lo as usize;
        let target = if inode.i_flags & EXT4_EXTENTS_FL != 0 {
            let block = self
                .get_inode_extents(inode)?
                .first()
                .map(|extent| extent.ee_start_lo)
                .ok_or_else(|| anyhow::anyhow!("symlink inode {ino} has no data block"))?;
            let offset = block as usize * BLOCK_SIZE;
            self.disk_data
                .get(offset..offset + size)
                .filter(|_| size < BLOCK_SIZE)
                .ok_or_else(|| anyhow::anyhow!("symlink inode {ino} has an invalid size"))?
        } else {
            inode
                .i_block
                .get(..size)
                .ok_or_else(|| anyhow::anyhow!("symlink inode {ino} has an invalid size"))?
        };
        String::from_utf8(target.to_vec())
            .map_err(|_| anyhow::anyhow!("symlink inode {ino} target is not UTF-8"))
    }

    /// Walks `path` from the directory chain `stack` (root first), following
    /// every symlink, including a final one.  Returns the resolved chain,
    /// whose last element is the target inode, or `None` when a component is
    /// missing or a non-final component is not a directory.
    fn walk_path(
        &self,
        mut stack: Vec<u32>,
        path: &str,
        hops: &mut usize,
    ) -> Result<Option<Vec<u32>>> {
        if path.starts_with('/') {
            stack.truncate(1);
        }
        for part in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            let current = *stack.last().expect("the walk always keeps the root");
            if !self.is_directory(current) {
                return Ok(None);
            }
            if part == ".." {
                // As in a chroot, ".." at the root stays at the root.
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let Some(child) = self.lookup_dir_entry(current, part)? else {
                return Ok(None);
            };
            if self.inode(child).file_type() != S_IFLNK {
                stack.push(child);
                continue;
            }
            *hops += 1;
            if *hops > MAX_SYMLINK_HOPS {
                return Err(anyhow::anyhow!(
                    "too many levels of symbolic links resolving {path:?}"
                ));
            }
            match self.follow_symlink(stack, child, hops)? {
                Some(resolved) => stack = resolved,
                None => return Ok(None),
            }
        }
        Ok(Some(stack))
    }

    /// Walks the target of the symlink `link` from the directory chain
    /// `stack` that holds it.  Returns the resolved chain, or `None` when
    /// the link dangles.
    fn follow_symlink(
        &self,
        stack: Vec<u32>,
        link: u32,
        hops: &mut usize,
    ) -> Result<Option<Vec<u32>>> {
        let target = self.read_symlink(link)?;
        // An empty target is invalid: e2fsck flags it and recent Linux
        // kernels refuse to load the inode ("invalid fast symlink length 0").
        // Walking it would visit no component and land on the link's own
        // parent, so it dangles.
        if target.is_empty() {
            return Ok(None);
        }
        self.walk_path(stack, &target, hops)
    }

    /// Resolves an absolute image path to an existing directory, following
    /// symlinks (e.g. merged-usr `/lib -> usr/lib`).
    pub fn resolve_directory(&self, path: &str) -> Result<Option<u32>> {
        let resolved = self.walk_path(vec![EXT4_ROOT_INO], path, &mut 0)?;
        Ok(resolved
            .and_then(|chain| chain.last().copied())
            .filter(|&inode| self.is_directory(inode)))
    }

    /// Resolves an absolute image path to an existing entry of any type,
    /// following every symlink including a final one, as stat(2) does inside
    /// a chroot.  Returns `None` when a component is missing or a symlink
    /// dangles.
    pub fn resolve_path(&self, path: &str) -> Result<Option<u32>> {
        let resolved = self.walk_path(vec![EXT4_ROOT_INO], path, &mut 0)?;
        Ok(resolved.and_then(|chain| chain.last().copied()))
    }

    /// Resolves the parent directories of `path` through symlinks and looks
    /// up its final component without following it.
    pub fn resolve_entry(&self, path: &str) -> Result<Option<u32>> {
        let path = path.trim_end_matches('/');
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if name.is_empty() || name == "." || name == ".." {
            return self.resolve_directory(path);
        }
        match self.resolve_directory(parent)? {
            Some(parent) => self.lookup_dir_entry(parent, name),
            None => Ok(None),
        }
    }

    /// Returns the directory at `path`, creating missing components.  Symlinks
    /// to directories are followed, as in containerd's layer applier.  Other
    /// non-directories are replaced by an empty directory when
    /// `replace_non_directories` is set, and are an error otherwise.
    pub fn ensure_directory(&mut self, path: &str, replace_non_directories: bool) -> Result<u32> {
        self.ensure_directory_with(path, replace_non_directories, None)
    }

    /// Like `ensure_directory`, giving missing components `created` metadata
    /// instead of the directory defaults.
    pub(crate) fn ensure_directory_with(
        &mut self,
        path: &str,
        replace_non_directories: bool,
        created: Option<&PosixMetadata>,
    ) -> Result<u32> {
        let mut stack = vec![EXT4_ROOT_INO];
        for part in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            if part == ".." {
                return Err(anyhow::anyhow!("image path {path:?} must not contain '..'"));
            }
            let current = *stack.last().expect("the walk always keeps the root");
            let Some(child) = self.lookup_dir_entry(current, part)? else {
                stack.push(self.mkdir(current, part, created)?);
                continue;
            };
            if self.inode(child).is_dir() {
                stack.push(child);
                continue;
            }
            if self.inode(child).file_type() == S_IFLNK
                && let Some(resolved) = self.follow_symlink(stack.clone(), child, &mut 1)?
                && resolved
                    .last()
                    .is_some_and(|&inode| self.is_directory(inode))
            {
                stack = resolved;
                continue;
            }
            if !replace_non_directories {
                return Err(anyhow::anyhow!(
                    "image path component {part:?} of {path:?} is not a directory"
                ));
            }
            self.remove_dir_entry(current, part)?;
            stack.push(self.mkdir(current, part, created)?);
        }
        Ok(*stack.last().expect("the walk always keeps the root"))
    }

    /// Creates an absolute directory path in a newly created image and returns
    /// its final inode. Existing paths are not looked up by this builder.
    pub fn create_directory_path(&mut self, path: &str) -> Result<u32> {
        if !path.starts_with('/') {
            return Err(anyhow::anyhow!("destination path must be absolute: {path}"));
        }
        let mut parent = EXT4_ROOT_INO;
        for component in path.split('/').filter(|component| !component.is_empty()) {
            Self::validate_name(component)?;
            parent = self.mkdir(parent, component, None)?;
        }
        Ok(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> Result<Ext4ImageBuilder> {
        let mut builder = Ext4ImageBuilder::new(16);
        builder.create_root_dir()?;
        builder.reserve_inodes();
        Ok(builder)
    }

    #[test]
    fn ensure_directory_follows_merged_usr_symlinks() -> Result<()> {
        let mut builder = builder()?;
        let usr = builder.mkdir(EXT4_ROOT_INO, "usr", None)?;
        let usr_lib = builder.mkdir(usr, "lib", None)?;
        builder.add_symlink(EXT4_ROOT_INO, "lib", "usr/lib", &PosixMetadata::default())?;

        let modules = builder.ensure_directory("/lib/modules", false)?;
        assert_eq!(builder.lookup_dir_entry(usr_lib, "modules")?, Some(modules));
        assert_eq!(builder.resolve_directory("/lib/modules")?, Some(modules));
        assert_eq!(builder.resolve_directory("/lib/modules/../..")?, Some(usr));
        assert!(builder.lookup_dir_entry(EXT4_ROOT_INO, "lib")?.is_some());
        Ok(())
    }

    #[test]
    fn slow_symlinks_resolve_and_loops_are_rejected() -> Result<()> {
        let mut builder = builder()?;
        let target = format!("/{}", "d/".repeat(40));
        let mut dir = EXT4_ROOT_INO;
        for _ in 0..40 {
            dir = builder.mkdir(dir, "d", None)?;
        }
        builder.add_symlink(EXT4_ROOT_INO, "deep", &target, &PosixMetadata::default())?;
        assert_eq!(
            builder.read_symlink(builder.lookup_dir_entry(EXT4_ROOT_INO, "deep")?.unwrap())?,
            target
        );
        assert_eq!(builder.resolve_directory("/deep")?, Some(dir));

        builder.add_symlink(EXT4_ROOT_INO, "loop", "loop", &PosixMetadata::default())?;
        assert!(builder.resolve_directory("/loop").is_err());
        assert!(builder.ensure_directory("/loop/x", false).is_err());
        Ok(())
    }

    #[test]
    fn ensure_directory_replaces_files_only_when_asked() -> Result<()> {
        let mut builder = builder()?;
        builder.add_file(EXT4_ROOT_INO, "etc", b"file", &PosixMetadata::default())?;
        assert!(builder.ensure_directory("/etc/app", false).is_err());
        let app = builder.ensure_directory("/etc/app", true)?;
        assert_eq!(builder.resolve_directory("/etc/app")?, Some(app));
        Ok(())
    }

    #[test]
    fn resolve_path_follows_final_symlinks_inside_the_image() -> Result<()> {
        let mut builder = builder()?;
        let meta = PosixMetadata::default();
        let usr = builder.mkdir(EXT4_ROOT_INO, "usr", None)?;
        let usr_bin = builder.mkdir(usr, "bin", None)?;
        let bash = builder.add_file(usr_bin, "bash", b"elf", &meta)?;
        builder.add_symlink(EXT4_ROOT_INO, "bin", "usr/bin", &meta)?;
        // Absolute targets resolve against the image root, as in a chroot.
        let sh = builder.add_symlink(usr_bin, "sh", "/bin/bash", &meta)?;
        builder.add_symlink(usr_bin, "dangling", "missing", &meta)?;

        assert_eq!(builder.resolve_path("/bin/bash")?, Some(bash));
        assert_eq!(builder.resolve_path("/bin/sh")?, Some(bash));
        assert_eq!(builder.resolve_path("/bin")?, Some(usr_bin));
        assert_eq!(builder.resolve_path("/")?, Some(EXT4_ROOT_INO));
        assert_eq!(builder.resolve_path("/bin/dangling")?, None);
        assert_eq!(builder.resolve_path("/bin/bash/x")?, None);
        // resolve_entry keeps the final symlink itself.
        assert_eq!(builder.resolve_entry("/bin/sh")?, Some(sh));
        Ok(())
    }

    #[test]
    fn empty_symlink_target_resolves_to_nothing() -> Result<()> {
        let mut builder = builder()?;
        let dir = builder.mkdir(EXT4_ROOT_INO, "dir", None)?;
        // ext4kit no longer writes empty links, so build the fast symlink an
        // older image may hold: a zero i_size and nothing inline.
        let empty = builder.add_symlink(dir, "empty", "x", &PosixMetadata::default())?;
        let inode = builder.inode_mut(empty);
        inode.i_size_lo = 0;
        inode.i_block = [0; 60];

        // Walking "" would stop at the link's parent; the link dangles.
        assert_eq!(builder.resolve_path("/dir/empty")?, None);
        assert_eq!(builder.resolve_path("/dir/empty/x")?, None);
        assert_eq!(builder.resolve_directory("/dir/empty")?, None);
        assert_eq!(builder.resolve_entry("/dir/empty")?, Some(empty));

        // ensure_directory treats it like any dangling link: an error, or
        // replaced by a directory when replacing is allowed.
        assert!(builder.ensure_directory("/dir/empty/x", false).is_err());
        assert_eq!(builder.lookup_dir_entry(dir, "x")?, None);
        let x = builder.ensure_directory("/dir/empty/x", true)?;
        assert_eq!(builder.resolve_directory("/dir/empty/x")?, Some(x));
        assert_eq!(builder.lookup_dir_entry(dir, "x")?, None);
        Ok(())
    }
}
