//! Streaming import of OCI filesystem layers.

pub mod local;
pub mod remote;

use crate::{
    ext4::format::EXT4_ROOT_INO,
    ext4::format::xattr::parse_xattr_name,
    ext4::{Ext4ImageBuilder, PosixMetadata},
};
use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use std::{
    collections::HashSet,
    io::{self, Read},
};
use tar::{Archive, EntryType};

pub const OCI_SIZE_MULTIPLIER: u64 = 3;
pub const IMAGE_SIZE_BUFFER_BYTES: u64 = 512 * 1024 * 1024;

/// Converts an estimated payload length to the ext4 image size, rounding up
/// so the caller can pass it directly to `Ext4ImageBuilder::new_file`.
pub fn image_size_mib(payload_bytes: u64, multiplier: u64) -> Result<usize> {
    let bytes = payload_bytes
        .checked_mul(multiplier)
        .and_then(|n| n.checked_add(IMAGE_SIZE_BUFFER_BYTES))
        .ok_or_else(|| anyhow::anyhow!("image size calculation overflow"))?;
    usize::try_from(bytes.div_ceil(1024 * 1024))
        .map_err(|_| anyhow::anyhow!("image is too large for this platform"))
}

/// Rejects images that are not linux/amd64.
pub(crate) fn verify_platform(
    os: Option<&str>,
    architecture: Option<&str>,
    image: &str,
) -> Result<()> {
    let os = os.ok_or_else(|| anyhow::anyhow!("image {image:?} does not report an OS"))?;
    let architecture = architecture
        .ok_or_else(|| anyhow::anyhow!("image {image:?} does not report an architecture"))?;
    if os != "linux" || !matches!(architecture, "amd64" | "x86_64") {
        anyhow::bail!("image {image:?} is {os}/{architecture}; ext4kit supports linux/amd64 images")
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayerCompression {
    Raw,
    Gzip,
    Zstd,
}

/// Maps the OCI and Docker filesystem layer media types to their compression.
pub(crate) fn layer_compression(media_type: &str) -> Result<LayerCompression> {
    Ok(match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => LayerCompression::Raw,
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => LayerCompression::Gzip,
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => LayerCompression::Zstd,
        other => anyhow::bail!("unsupported OCI layer media type {other:?}"),
    })
}

/// Decompresses and applies one layer, then consumes the rest of the stream
/// so the compression trailer (and its checksum) is read and verified.
pub(crate) fn apply_compressed_layer(
    builder: &mut Ext4ImageBuilder,
    reader: impl Read,
    compression: LayerCompression,
) -> Result<()> {
    // Gzip layers may consist of several members (eStargz writes one per
    // file); like Go's gzip.Reader, decode all of them.
    let mut input: Box<dyn Read + '_> = match compression {
        LayerCompression::Raw => Box::new(reader),
        LayerCompression::Gzip => Box::new(MultiGzDecoder::new(reader)),
        LayerCompression::Zstd => Box::new(
            zstd::stream::read::Decoder::new(reader).context("cannot initialize zstd decoder")?,
        ),
    };
    apply_layer(builder, &mut input)?;
    io::copy(&mut input, &mut io::sink()).context("cannot read layer trailer")?;
    Ok(())
}

/// Applies one uncompressed tar layer. Kept separate so extraction is unit-testable.
pub fn apply_layer(builder: &mut Ext4ImageBuilder, reader: impl Read) -> Result<()> {
    let mut archive = Archive::new(reader);
    // Entries this layer wrote, keyed by (parent inode, name).  Whiteouts
    // only hide lower layers, so these survive whiteouts in the same layer.
    let mut written = HashSet::<(u32, String)>::new();
    for entry in archive.entries().context("cannot read tar entries")? {
        let mut entry = entry.context("cannot read tar entry")?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let path = split_tar_path(&entry.path_bytes())?;
        let Some((name, parents)) = path.split_last() else {
            // The layer root ("./") carries the metadata of "/".
            if kind == EntryType::Directory {
                let metadata = entry_metadata(&mut entry)?;
                builder.set_inode_metadata(EXT4_ROOT_INO, &metadata)?;
            }
            continue;
        };
        let parents = parents.join("/");

        if name == ".wh..wh..opq" {
            // A whiteout is control data: it must not manufacture a parent
            // that did not exist in the lower layers.
            if let Some(directory) = builder.resolve_directory(&parents)? {
                remove_lower_entries(builder, directory, &written)?;
            }
            continue;
        }
        if let Some(hidden) = name.strip_prefix(".wh.") {
            if let Some(directory) = builder.resolve_directory(&parents)?
                && !written.contains(&(directory, hidden.to_owned()))
            {
                builder.remove_dir_entry(directory, hidden)?;
            }
            continue;
        }

        let parent = builder.ensure_directory(&parents, true)?;
        let metadata = entry_metadata(&mut entry)?;
        match kind {
            EntryType::Directory => match builder.lookup_dir_entry(parent, name)? {
                // An existing directory keeps its children.
                Some(inode) if builder.is_directory(inode) => {
                    builder.set_inode_metadata(inode, &metadata)?;
                }
                existing => {
                    if existing.is_some() {
                        builder.remove_dir_entry(parent, name)?;
                    }
                    builder.mkdir(parent, name, Some(&metadata))?;
                }
            },
            EntryType::Regular | EntryType::Continuous => {
                builder.remove_dir_entry(parent, name)?;
                // entry.size() honours a PAX size override (files >= 8 GiB).
                let size = entry.size();
                builder.add_file_from_reader(parent, name, &mut entry, size, &metadata)?;
            }
            EntryType::Symlink => {
                let target = link_name(&entry)?;
                let target = String::from_utf8(target)
                    .map_err(|_| anyhow::anyhow!("symlink target of {name:?} is not UTF-8"))?;
                builder.remove_dir_entry(parent, name)?;
                builder
                    .add_symlink(parent, name, &target, &metadata)
                    .with_context(|| format!("cannot create symlink {}", path.join("/")))?;
            }
            EntryType::Link => {
                // Hard-link names are rooted at the archive root, and tar
                // writers always emit the target before any link to it.
                let target = split_tar_path(&link_name(&entry)?)?.join("/");
                let inode = builder
                    .resolve_entry(&target)?
                    .ok_or_else(|| anyhow::anyhow!("hard-link target {target} does not exist"))?;
                builder.remove_dir_entry(parent, name)?;
                builder.add_hard_link(parent, name, inode)?;
            }
            EntryType::Char | EntryType::Block => {
                let major = device(&entry, true)?;
                let minor = device(&entry, false)?;
                builder.remove_dir_entry(parent, name)?;
                if kind == EntryType::Char {
                    builder.add_char_device(parent, name, major, minor, &metadata)?;
                } else {
                    builder.add_block_device(parent, name, major, minor, &metadata)?;
                }
            }
            EntryType::Fifo => {
                builder.remove_dir_entry(parent, name)?;
                builder.add_fifo(parent, name, &metadata)?;
            }
            other if other.as_byte() == b's' => {
                builder.remove_dir_entry(parent, name)?;
                builder.add_socket(parent, name, &metadata)?;
            }
            other => anyhow::bail!(
                "unsupported tar entry type {other:?} for {}",
                path.join("/")
            ),
        }
        written.insert((parent, name.clone()));
    }
    Ok(())
}

/// Applies an opaque whiteout: removes everything below `directory` that
/// this layer did not write itself.
fn remove_lower_entries(
    builder: &mut Ext4ImageBuilder,
    directory: u32,
    written: &HashSet<(u32, String)>,
) -> Result<()> {
    for (name, inode) in builder.directory_entries(directory)? {
        if !written.contains(&(directory, name.clone())) {
            builder.remove_dir_entry(directory, &name)?;
        } else if builder.is_directory(inode) {
            remove_lower_entries(builder, inode, written)?;
        }
    }
    Ok(())
}

/// Splits a tar path on '/' only: tar names are POSIX paths even when
/// ext4kit runs on Windows, where `\` is an ordinary filename character.
/// Leading '/' and "." components are dropped; ".." is rejected.
fn split_tar_path(path: &[u8]) -> Result<Vec<String>> {
    path.split(|&byte| byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .map(|component| {
            if component == b".." {
                anyhow::bail!(
                    "tar path must not contain '..': {}",
                    String::from_utf8_lossy(path)
                );
            }
            String::from_utf8(component.to_vec()).map_err(|_| {
                anyhow::anyhow!("tar path is not UTF-8: {}", String::from_utf8_lossy(path))
            })
        })
        .collect()
}

fn link_name<R: Read>(entry: &tar::Entry<'_, R>) -> Result<Vec<u8>> {
    entry
        .link_name_bytes()
        .map(|name| name.into_owned())
        .ok_or_else(|| anyhow::anyhow!("link entry has no target"))
}

fn entry_metadata<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<PosixMetadata> {
    let header = entry.header();
    let mtime = header
        .mtime()
        .context("invalid tar mtime")?
        .min(u64::from(u32::MAX)) as u32;
    let mut metadata = PosixMetadata {
        mode: Some(header.mode().context("invalid tar mode")? as u16),
        uid: header.uid().context("invalid tar uid")? as u32,
        gid: header.gid().context("invalid tar gid")? as u32,
        // Layers record only mtime; use it rather than the 1970 epoch.
        atime: mtime,
        ctime: mtime,
        mtime,
        ..Default::default()
    };
    if let Some(extensions) = entry.pax_extensions().context("invalid PAX extensions")? {
        for extension in extensions {
            let extension = extension.context("invalid PAX extension")?;
            let Some(name) = extension
                .key()
                .ok()
                .and_then(|key| key.strip_prefix("SCHILY.xattr."))
            else {
                continue;
            };
            if parse_xattr_name(name).is_err() {
                eprintln!("warning: skipping unsupported xattr {name:?}");
                continue;
            }
            metadata
                .xattrs
                .insert(name.to_owned(), extension.value_bytes().to_vec());
        }
    }
    Ok(metadata)
}

fn device<R: Read>(entry: &tar::Entry<'_, R>, major: bool) -> Result<u32> {
    let value = if major {
        entry.header().device_major()
    } else {
        entry.header().device_minor()
    };
    value
        .context("invalid tar device number")?
        .ok_or_else(|| anyhow::anyhow!("device entry lacks a major/minor number"))
}

#[cfg(test)]
mod tests {
    use super::{
        LayerCompression, apply_compressed_layer, apply_layer, layer_compression, split_tar_path,
        verify_platform,
    };
    use crate::{
        ext4::format::{EXT4_ROOT_INO, S_IFBLK},
        ext4::{Ext4ImageBuilder, PosixMetadata},
    };
    use flate2::{Compression, write::GzEncoder};
    use std::io::{Cursor, Write};
    use tar::{Builder, EntryType, Header};

    fn layer(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut tar = Builder::new(&mut bytes);
            for (path, kind, contents) in entries {
                let mut header = Header::new_gnu();
                header.set_entry_type(*kind);
                header.set_mode(if *kind == EntryType::Directory {
                    0o755
                } else {
                    0o644
                });
                header.set_uid(12);
                header.set_gid(34);
                header.set_mtime(56);
                // Write names verbatim: Builder::append_data would turn `\`
                // into `/` on Windows, hiding exactly what tests exercise.
                header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                let data: &[u8] = if matches!(kind, EntryType::Link | EntryType::Symlink) {
                    header.as_old_mut().linkname[..contents.len()].copy_from_slice(contents);
                    &[]
                } else {
                    contents
                };
                header.set_size(data.len() as u64);
                header.set_cksum();
                tar.append(&header, data).unwrap();
            }
            tar.finish().unwrap();
        }
        bytes
    }

    fn builder() -> Ext4ImageBuilder {
        let mut builder = Ext4ImageBuilder::new(16);
        builder.create_root_dir().unwrap();
        builder.reserve_inodes();
        builder
    }

    fn names(builder: &Ext4ImageBuilder, directory: u32) -> Vec<String> {
        let mut names: Vec<_> = builder
            .directory_entries(directory)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn applies_whiteouts_and_replaces_files() {
        let mut builder = builder();
        apply_layer(
            &mut builder,
            Cursor::new(layer(&[
                ("etc/", EntryType::Directory, b""),
                ("etc/old", EntryType::Regular, b"old"),
                ("etc/keep", EntryType::Regular, b"first"),
            ])),
        )
        .unwrap();
        let etc = builder
            .lookup_dir_entry(EXT4_ROOT_INO, "etc")
            .unwrap()
            .unwrap();
        let first_keep = builder.lookup_dir_entry(etc, "keep").unwrap().unwrap();

        apply_layer(
            &mut builder,
            Cursor::new(layer(&[
                ("etc/.wh.old", EntryType::Regular, b""),
                ("etc/keep", EntryType::Regular, b"second"),
            ])),
        )
        .unwrap();
        assert_eq!(builder.lookup_dir_entry(etc, "old").unwrap(), None);
        assert_ne!(
            builder.lookup_dir_entry(etc, "keep").unwrap(),
            Some(first_keep)
        );
    }

    #[test]
    fn opaque_whiteout_clears_only_lower_layer_children() {
        let mut builder = builder();
        apply_layer(
            &mut builder,
            Cursor::new(layer(&[
                ("var/", EntryType::Directory, b""),
                ("var/a", EntryType::Regular, b"a"),
                ("var/b", EntryType::Regular, b"b"),
                ("var/sub/", EntryType::Directory, b""),
                ("var/sub/lower", EntryType::Regular, b"lower"),
            ])),
        )
        .unwrap();
        // The opaque marker comes after siblings written by the same layer;
        // they must survive, while lower content (also below "sub") goes.
        apply_layer(
            &mut builder,
            Cursor::new(layer(&[
                ("var/c", EntryType::Regular, b"c"),
                ("var/sub/", EntryType::Directory, b""),
                ("var/sub/upper", EntryType::Regular, b"upper"),
                ("var/.wh..wh..opq", EntryType::Regular, b""),
                ("var/.wh.c", EntryType::Regular, b""),
            ])),
        )
        .unwrap();
        let var = builder.resolve_directory("/var").unwrap().unwrap();
        assert_eq!(names(&builder, var), ["c", "sub"]);
        let sub = builder.resolve_directory("/var/sub").unwrap().unwrap();
        assert_eq!(names(&builder, sub), ["upper"]);
    }

    #[test]
    fn whiteout_removes_block_devices() {
        let mut builder = builder();
        let dev = builder.mkdir(EXT4_ROOT_INO, "dev", None).unwrap();
        let sda = builder
            .add_block_device(dev, "sda", 8, 0, &PosixMetadata::default())
            .unwrap();
        assert_eq!(builder.inode(sda).file_type(), S_IFBLK);
        apply_layer(
            &mut builder,
            Cursor::new(layer(&[("dev/.wh.sda", EntryType::Regular, b"")])),
        )
        .unwrap();
        assert_eq!(builder.lookup_dir_entry(dev, "sda").unwrap(), None);
        assert!(!builder.is_inode_allocated(sda));
    }

    #[test]
    fn hard_links_and_symlinked_parents_resolve() {
        let mut builder = builder();
        apply_layer(
            &mut builder,
            Cursor::new(layer(&[
                ("./", EntryType::Directory, b""),
                ("usr/lib/", EntryType::Directory, b""),
                ("lib", EntryType::Symlink, b"usr/lib"),
                ("usr/lib/a", EntryType::Regular, b"a"),
                ("usr/lib/b", EntryType::Link, b"usr/lib/a"),
                ("lib/c", EntryType::Regular, b"c"),
                ("usr/lib/name\\with\\backslash", EntryType::Regular, b"x"),
            ])),
        )
        .unwrap();
        let lib = builder.resolve_directory("/usr/lib").unwrap().unwrap();
        assert_eq!(
            names(&builder, lib),
            ["a", "b", "c", "name\\with\\backslash"]
        );
        let a = builder.lookup_dir_entry(lib, "a").unwrap().unwrap();
        assert_eq!(builder.lookup_dir_entry(lib, "b").unwrap(), Some(a));
        assert_eq!({ builder.inode(a).i_links_count }, 2);
        assert_eq!({ builder.inode(EXT4_ROOT_INO).i_mode } & 0o7777, 0o755);
        assert!(
            apply_layer(
                &mut builder,
                Cursor::new(layer(&[("x", EntryType::Link, b"missing")])),
            )
            .is_err()
        );
    }

    #[test]
    fn empty_symlink_targets_are_rejected() {
        // A plain header cannot carry an empty link name: tar reads it as
        // missing.
        let plain = apply_layer(
            &mut builder(),
            Cursor::new(layer(&[("etc/empty", EntryType::Symlink, b"")])),
        )
        .unwrap_err();
        assert!(plain.to_string().contains("has no target"), "{plain:#}");

        // A PAX linkpath record can, overriding the header's link name.  A
        // record is "<length> <key>=<value>\n", its own length included.
        let record = b"13 linkpath=\n";
        let mut pax = Header::new_ustar();
        pax.set_entry_type(EntryType::XHeader);
        pax.set_path("PaxHeaders/empty").unwrap();
        pax.set_size(record.len() as u64);
        pax.set_cksum();
        let mut bytes = pax.as_bytes().to_vec();
        bytes.extend(record);
        bytes.resize(1024, 0);
        bytes.extend(layer(&[("etc/empty", EntryType::Symlink, b"placeholder")]));
        let error = apply_layer(&mut builder(), Cursor::new(bytes)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("etc/empty"), "{message}");
        assert!(message.contains("must not be empty"), "{message}");
    }

    #[test]
    fn multi_member_gzip_layers_are_fully_applied() {
        let tar = layer(&[
            ("first", EntryType::Regular, b"1"),
            ("second", EntryType::Regular, b"2"),
        ]);
        let (head, tail) = tar.split_at(512 * 2);
        let mut compressed = Vec::new();
        for part in [head, tail] {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(part).unwrap();
            compressed.extend(encoder.finish().unwrap());
        }
        let mut builder = builder();
        apply_compressed_layer(
            &mut builder,
            Cursor::new(compressed),
            LayerCompression::Gzip,
        )
        .unwrap();
        assert_eq!(names(&builder, EXT4_ROOT_INO), ["first", "second"]);
    }

    #[test]
    fn tar_paths_split_on_slashes_only() {
        assert_eq!(split_tar_path(b"/./usr//a\\b/").unwrap(), ["usr", "a\\b"]);
        assert!(split_tar_path(b"usr/../etc").is_err());
    }

    #[test]
    fn layer_media_types_are_matched_exactly() {
        assert_eq!(
            layer_compression("application/vnd.oci.image.layer.v1.tar+zstd").unwrap(),
            LayerCompression::Zstd
        );
        assert!(
            layer_compression("application/vnd.oci.image.layer.v1.tar+gzip+encrypted").is_err()
        );
        assert!(layer_compression("application/vnd.cncf.helm.chart.content.v1.tar+gzip").is_err());
    }

    #[test]
    fn accepts_linux_amd64() {
        assert!(verify_platform(Some("linux"), Some("amd64"), "test").is_ok());
    }

    #[test]
    fn rejects_other_platforms() {
        assert!(verify_platform(Some("windows"), Some("amd64"), "test").is_err());
        assert!(verify_platform(Some("linux"), Some("arm64"), "test").is_err());
    }
}
