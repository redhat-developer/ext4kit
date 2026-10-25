# Editing images

`ext4kit edit IMAGE ACTION` changes an image that `ext4kit create` wrote, in
place, without Linux, a loop mount, `debugfs` or a VM. Each call runs one
action, so a fix-up is an ordinary script of edit calls that runs in order:
remove something, then recreate it; read a file out, change it on the host,
write it back.

```powershell
ext4kit create --from docker.io/library/alpine:3.20 --add C:\work\bin:/opt/app rootfs.img
ext4kit edit rootfs.img chmod 755 /opt/app/bin/tool
ext4kit edit rootfs.img mkdir -p /sandbox
ext4kit edit rootfs.img chown -R 1000:1000 /sandbox
ext4kit edit rootfs.img test -x /bin/sh
```

## Actions

| Action | Behaviour |
|--------|-----------|
| `rm [-r] [-f] PATH` | Removes a file, symlink or, with `-r`, a directory tree. A final symlink is removed itself, never its target. `-f` succeeds when PATH (or its parent) does not exist. |
| `mkdir [-p] [-m MODE] [--owner UID[:GID]] PATH` | Creates a directory, `0755 root:root` unless given. `-p` creates missing parents with the defaults and leaves an existing directory unchanged; MODE and owner apply only to a directory it creates. |
| `chmod MODE PATH` | Sets octal permission bits (up to `7777`, so setuid/setgid/sticky too), following symlinks. Ownership, timestamps and xattrs are kept. |
| `chown [-R] UID[:GID] PATH` | Sets the numeric owner; a missing GID defaults to the UID. Without `-R` a final symlink is followed. With `-R` no symlink is followed at all (`chown -R -P`): links are re-owned themselves, so the walk never leaves PATH. |
| `add [--chown UID[:GID]] SOURCE[:/DESTINATION]` | Imports a host file or directory exactly like `create --add`: a directory is merged into an existing directory of the same name, anything else is replaced. Fails up front when the image clearly lacks the free blocks or inodes. |
| `write [-m MODE] [--owner UID[:GID]] HOST_FILE PATH` | Writes a host file, or stdin with `-`, to PATH, following symlinks. An existing file keeps its inode, hard links, timestamps and xattrs, and its mode and owner unless given. A new file is `0644 root:root` unless given. |
| `cat PATH` | Prints a file to stdout, following symlinks. |
| `test -e\|-f\|-d\|-x\|-L PATH` | Exits 0 when PATH exists / is a regular file / is a directory / has an execute bit / is itself a symlink, 1 when it does not, and 2 when the check failed (for example, the image cannot be opened). All but `-L` follow symlinks. |
| `ln [-s] TARGET LINK` | Creates a symlink with `-s` (TARGET is stored as given, must not be empty and need not exist), otherwise a hard link to the existing non-directory TARGET. |

Image paths are absolute and split on `/` only. Symlinks are resolved inside
the image, as in a chroot: an absolute target such as `/bin/busybox` means
the image's `/bin/busybox`, and `..` stops at the image root. Parent
directories are resolved through symlinks, so `/bin/x` works when `/bin` is a
merged-usr link to `usr/bin`.

Created entries get zero timestamps, like the destination directories that
`create` makes, and no action changes the timestamps of existing entries, so
the same script produces the same image.

`cat` and `test` open the image read-only and work on a read-only file.

### Git Bash

Git Bash rewrites arguments that look like POSIX paths, so `/etc/passwd`
reaches ext4kit as `C:/Program Files/Git/etc/passwd`. ext4kit rejects such a
path and says so. Set `MSYS_NO_PATHCONV=1`, or run ext4kit from PowerShell or
cmd.

## Which images can be edited

`edit` accepts only images in ext4kit's own format: exactly the features it
writes, 4 KiB blocks, 256-byte inodes, a clean state and free counts that
match the bitmaps. Images made by `mke2fs` (journal, `flex_bg`, `64bit`,
metadata checksums) are rejected before anything is written. An image that a
Linux VM mounted read-write may also be rejected; run `e2fsck -f` on it, or
rebuild it.

The image cannot grow. Create it with enough headroom (`create --size`) for
what later edits add.

## Failures

A failed action exits with status 1 and an `Error:` message. The image stays
consistent: every builder operation validates before it changes anything, and
after an action fails the image metadata is still saved, so blocks already
written by a multi-step action (`add`, `mkdir -p`) are accounted for. Such an
action may therefore leave part of its work behind, like its coreutils
counterpart; the other actions either complete or change nothing.

Work on a copy if the image must survive a crash or power loss in the middle
of an edit: data is written in place and the metadata at the end of each call.
