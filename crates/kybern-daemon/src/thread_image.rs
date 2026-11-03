//! The file behind a thread's image reference (`GET /threads/{id}/image`): where the reference points, whether it is a
//! regular file on this computer's own disks, and its bytes, bounded. Everything here blocks (a hung network mount can
//! hold any step); the route runs it on the blocking pool under a timeout.

use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};

/// Enough of a file to tell an image by its content (`image_mime`).
const HEADER_BYTES: usize = 16;

/// Why a reference gave no image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageError {
    /// It names no regular file on this computer's own disks.
    NotFound,
    /// Larger than the limit.
    TooLarge,
    /// Not a PNG, JPEG, GIF, WebP or AVIF by its content.
    NotImage,
    /// The file was found but could not be read.
    Unreadable,
}

#[derive(Debug)]
pub(crate) struct Image {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// The image a thread's reference names, read whole, at most `limit` bytes. `root` is the thread's folder, `home` the
/// user's (`~/`). Blocks.
pub(crate) fn load(root: &Path, home: Option<&Path>, reference: &str, limit: usize) -> Result<Image, ImageError> {
    let (path, meta) = resolve(root, home, reference).ok_or(ImageError::NotFound)?;
    let file = open_regular(&path, &meta).map_err(|_| ImageError::NotFound)?;
    read_image(file, meta.len(), limit)
}

/// A file's bytes when it is an image: the size first (a file over the limit is not read at all), then the header (a
/// file that is not an image is refused before the rest is read), then the rest, never more than the limit.
fn read_image(mut file: impl Read, len: u64, limit: usize) -> Result<Image, ImageError> {
    if len > limit as u64 {
        return Err(ImageError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(HEADER_BYTES);
    (&mut file).take(HEADER_BYTES as u64).read_to_end(&mut bytes).map_err(|_| ImageError::Unreadable)?;
    let mime = image_mime(&bytes).ok_or(ImageError::NotImage)?;
    // The file can have grown since its size was read.
    let rest = (limit + 1).saturating_sub(bytes.len()) as u64;
    file.take(rest).read_to_end(&mut bytes).map_err(|_| ImageError::Unreadable)?;
    if bytes.len() > limit {
        return Err(ImageError::TooLarge);
    }
    Ok(Image { mime, bytes })
}

/// The regular file a thread's image reference names and its metadata, or `None`. `reference` is the path as the
/// message wrote it: relative to the thread's folder (`root`), absolute, `~/…` or `file://…` (percent-encoding is tried
/// when the literal path is missing). Any folder works: an agent in a worktree or another checkout shows its images
/// wherever it saved them (ADE-122). What makes it an image is its content, checked when it is read, with the size
/// limit; the route stays behind the daemon's token and the thread it is asked for.
///
/// Only this computer's own disks: a network share would make the daemon connect out, with the user's credentials,
/// just to look. A path that shows a share (`\\host\share`, a mapped network drive on Windows) is refused before it is
/// touched; the resolved path is checked again, which catches a local link to a share (resolving it has contacted the
/// share by then, but nothing is read from it). Device and kernel folders (`/dev`, `/proc`, `/sys`) are refused too:
/// `/dev/fd/N` on macOS is a file the daemon has open.
pub(crate) fn resolve(root: &Path, home: Option<&Path>, reference: &str) -> Option<(PathBuf, Metadata)> {
    for target in image_targets(reference.trim(), root, home) {
        if !local_path(&target) || !drive_is_local(&target) {
            continue;
        }
        let Ok(path) = std::fs::canonicalize(&target) else { continue };
        if !local_path(&path) || !drive_is_local(&path) || special_path(&path) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_file() && local_mount(&path, &meta) {
            return Some((path, meta));
        }
    }
    None
}

/// A path on this computer's own disks by its form: no UNC share and no device namespace (`\\host\x`, `\\?\UNC\…`,
/// `\\.\…`). Paths have no such prefix off Windows.
pub(crate) fn local_path(path: &Path) -> bool {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)),
        _ => true,
    }
}

/// A path in a device or kernel folder, whose "files" are not files on a disk.
fn special_path(path: &Path) -> bool {
    cfg!(unix) && ["/dev", "/proc", "/sys"].iter().any(|folder| path.starts_with(folder))
}

/// On Windows, a drive letter that is not a network drive (`GetDriveTypeW`, which asks no server). Everywhere else, and
/// for paths without a drive letter, true.
#[cfg(windows)]
fn drive_is_local(path: &Path) -> bool {
    /// `DRIVE_REMOTE` from `GetDriveTypeW`.
    const DRIVE_REMOTE: u32 = 4;
    let Some(Component::Prefix(prefix)) = path.components().next() else { return true };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else { return true };
    let root: Vec<u16> = format!("{}:\\", letter as char).encode_utf16().chain([0]).collect();
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
    let kind = unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) };
    kind != DRIVE_REMOTE
}

#[cfg(not(windows))]
fn drive_is_local(_path: &Path) -> bool {
    true
}

/// On macOS, a file system mounted as local (`MNT_LOCAL`): no NFS, SMB, AFP or WebDAV mount.
#[cfg(target_os = "macos")]
fn local_mount(path: &Path, _meta: &Metadata) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `stats` is a writable `statfs` the call fills in.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: `statfs` returned 0, so it filled `stats` in.
    let stats = unsafe { stats.assume_init() };
    stats.f_flags & libc::MNT_LOCAL as u32 != 0
}

/// On Linux, a file system that is not a known network one (NFS, SMB/CIFS, AFS, Ceph, GlusterFS, and FUSE file
/// systems over the network such as sshfs and rclone), by the mount table entry of the file's device.
#[cfg(target_os = "linux")]
fn local_mount(_path: &Path, meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else { return true };
    let dev = meta.dev();
    mount_type(&table, libc::major(dev), libc::minor(dev)).is_none_or(|kind| !network_file_system(kind))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn local_mount(_path: &Path, _meta: &Metadata) -> bool {
    true
}

/// The file system type of the mount whose device is `major:minor`, from `/proc/self/mountinfo` (`36 35 98:0 /root
/// /mount opts … - ext4 /dev/sda1 rw`).
#[cfg(any(target_os = "linux", test))]
fn mount_type(table: &str, major: u32, minor: u32) -> Option<&str> {
    let device = format!("{major}:{minor}");
    table.lines().find_map(|line| {
        if line.split(' ').nth(2)? != device {
            return None;
        }
        line.split_once(" - ")?.1.split(' ').next()
    })
}

#[cfg(any(target_os = "linux", test))]
fn network_file_system(kind: &str) -> bool {
    const NETWORK: &[&str] = &[
        "cifs",
        "smb3",
        "smbfs",
        "ncpfs",
        "afs",
        "ceph",
        "glusterfs",
        "davfs",
        "fuse.sshfs",
        "fuse.rclone",
        "fuse.s3fs",
        "fuse.gcsfuse",
        "fuse.davfs2",
        "fuse.curlftpfs",
        "fuse.smbnetfs",
    ];
    kind.starts_with("nfs") || NETWORK.contains(&kind)
}

/// Open the file `expected` describes, for reading. On Unix the open does not block (a FIFO swapped in since `expected`
/// was read cannot hold the thread), and the open file must be a regular file with `expected`'s device and inode.
#[cfg(unix)]
fn open_regular(path: &Path, expected: &Metadata) -> std::io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.dev() != expected.dev() || meta.ino() != expected.ino() {
        return Err(std::io::Error::other("not the regular file that was looked at"));
    }
    let fd = file.as_raw_fd();
    // SAFETY: `fd` stays open while `file` lives; F_GETFL and F_SETFL only read and change its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular(path: &Path, _expected: &Metadata) -> std::io::Result<File> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// Where a reference points, without touching the file system: `file://` stripped, `~/` expanded, relative paths
/// joined to the thread's folder, `.` and `..` resolved lexically. The literal form first, then the percent-decoded one.
fn image_targets(reference: &str, root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let literal = reference.strip_prefix("file://localhost").or_else(|| reference.strip_prefix("file://")).unwrap_or(reference);
    let mut targets = Vec::new();
    for raw in [Some(literal.to_string()), percent_decode(literal)].into_iter().flatten() {
        if raw.is_empty() {
            continue;
        }
        let joined = match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            Some(rest) => match home {
                Some(home) => home.join(rest),
                None => continue,
            },
            None => root.join(&raw),
        };
        let mut target = PathBuf::new();
        for part in joined.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    target.pop();
                }
                part => target.push(part),
            }
        }
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[255, 216, 255]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes.get(4..8) == Some(b"ftyp") && matches!(bytes.get(8..12), Some(b"avif" | b"avis")) {
        Some("image/avif")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageError, image_targets, local_path, percent_decode, read_image, resolve, special_path};
    use std::path::{Path, PathBuf};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn image_references_resolve_relative_absolute_home_and_file_urls() {
        let root = Path::new("/work/ade");
        let home = Path::new("/home/me");
        let targets = |reference: &str| image_targets(reference, root, Some(home));
        assert_eq!(targets(".screenshots/a.png"), [root.join(".screenshots/a.png")]);
        assert_eq!(targets("./shots/../a.png"), [root.join("a.png")]);
        assert_eq!(targets("/elsewhere/b.png"), [PathBuf::from("/elsewhere/b.png")]);
        assert_eq!(targets("~/Desktop/c.png"), [home.join("Desktop/c.png")]);
        assert_eq!(targets("file:///tmp/d.png"), [PathBuf::from("/tmp/d.png")]);
        assert_eq!(targets("a%20b.png"), [root.join("a%20b.png"), root.join("a b.png")]);
        assert!(image_targets("~/x.png", root, None).is_empty(), "no home folder, no `~/`");
        assert_eq!(percent_decode("a%20b%C3%A4.png").as_deref(), Some("a bä.png"));
        assert_eq!(percent_decode("plain.png"), None);
    }

    #[test]
    fn only_local_disks_are_looked_at() {
        assert!(local_path(Path::new("/Users/me/a.png")));
        #[cfg(windows)]
        {
            assert!(local_path(Path::new(r"C:\Users\me\a.png")));
            assert!(local_path(Path::new(r"\\?\C:\Users\me\a.png")));
            for share in [r"\\host\share\a.png", r"\\?\UNC\host\share\a.png", r"\\.\pipe\x", "//host/share/a.png"] {
                assert!(!local_path(Path::new(share)), "{share}");
            }
        }
    }

    // macOS: `/dev/fd/3` stats as a regular file and is one the daemon holds open.
    #[test]
    fn device_and_kernel_folders_are_refused() {
        for path in ["/dev/fd/3", "/dev/stdin", "/proc/self/fd/3", "/proc/self/environ", "/sys/kernel/x"] {
            assert_eq!(special_path(Path::new(path)), cfg!(unix), "{path}");
        }
        assert!(!special_path(Path::new("/devices/a.png")));
        assert!(!special_path(Path::new("/Users/me/dev/a.png")));
        #[cfg(unix)]
        {
            // A link that resolves into one is refused too.
            let base = std::env::temp_dir().join(format!("kybern-image-dev-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&base).unwrap();
            std::os::unix::fs::symlink("/dev/null", base.join("null.png")).unwrap();
            assert!(resolve(&base, None, "null.png").is_none());
            assert!(resolve(&base, None, "/dev/fd/0").is_none());
            std::fs::remove_dir_all(base).unwrap();
        }
    }

    // ADE-122: an image is served from wherever it is; what it has to be is a regular file.
    #[test]
    fn an_image_path_is_any_regular_file_inside_or_outside_the_thread_folder() {
        let base = std::env::temp_dir().join(format!("kybern-image-path-{}", uuid::Uuid::now_v7()));
        let (root, outside, home) = (base.join("thread"), base.join("other-checkout"), base.join("home"));
        for folder in [&root, &outside, &home.join("Pictures")] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(root.join("in.png"), "x").unwrap();
        std::fs::write(outside.join("out.png"), "x").unwrap();
        std::fs::write(home.join("Pictures/shot.png"), "x").unwrap();
        let find = |reference: &str| resolve(&root, Some(&home), reference).map(|(path, _)| path);
        let canonical = |path: PathBuf| std::fs::canonicalize(path).unwrap();
        assert_eq!(find("in.png"), Some(canonical(root.join("in.png"))));
        assert_eq!(find("../other-checkout/out.png"), Some(canonical(outside.join("out.png"))));
        assert_eq!(find(&outside.join("out.png").to_string_lossy()), Some(canonical(outside.join("out.png"))));
        assert_eq!(find("~/Pictures/shot.png"), Some(canonical(home.join("Pictures/shot.png"))));
        assert_eq!(find("missing.png"), None);
        assert_eq!(find(&outside.to_string_lossy()), None, "a folder is no image");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_read() {
        struct Unread;
        impl std::io::Read for Unread {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("read a file whose size was already over the limit")
            }
        }
        assert_eq!(read_image(Unread, 101, 100).unwrap_err(), ImageError::TooLarge);
        // One that grew past the limit after its size was read is cut off at the limit.
        let grown = [PNG, &[0; 200]].concat();
        assert_eq!(read_image(&grown[..], 50, 100).unwrap_err(), ImageError::TooLarge);
        assert_eq!(read_image(&b"GIF? no"[..], 7, 100).unwrap_err(), ImageError::NotImage);
        let image = read_image(PNG, PNG.len() as u64, 100).unwrap();
        assert_eq!((image.mime, image.bytes.as_slice()), ("image/png", PNG));
    }

    // A FIFO swapped in for the file between its check and its open must not hold a blocking thread for good.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_place_of_the_checked_file_is_refused_without_blocking() {
        let base = std::env::temp_dir().join(format!("kybern-image-fifo-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        let (file, fifo) = (base.join("shot.png"), base.join("swap.png"));
        std::fs::write(&file, PNG).unwrap();
        let checked = std::fs::metadata(&file).unwrap();
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `name` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(resolve(&base, None, "swap.png").is_none(), "a FIFO is no regular file");
        let (sent, opened) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = sent.send(super::open_regular(&path, &checked).is_err());
        });
        assert_eq!(opened.recv_timeout(std::time::Duration::from_secs(10)), Ok(true), "refused, and at once");
        // The checked file itself opens, and reads normally after the open.
        let (path, meta) = resolve(&base, None, "shot.png").unwrap();
        let image = read_image(super::open_regular(&path, &meta).unwrap(), meta.len(), 100).unwrap();
        assert_eq!(image.bytes, PNG);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn network_mounts_are_known_by_their_mount_table_entry() {
        let table = "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
                     40 22 0:45 / /mnt/nas rw,relatime shared:20 - nfs4 nas:/export rw,vers=4.2\n\
                     41 22 0:46 / /mnt/share rw shared:21 - cifs //host/share rw\n\
                     42 22 0:47 / /home/me/remote rw,nosuid,nodev shared:22 - fuse.sshfs me@host:/ rw\n\
                     43 22 0:48 / /mnt/c rw,noatime - 9p drvfs rw\n";
        let kind = |major, minor| super::mount_type(table, major, minor);
        assert_eq!(kind(8, 2), Some("ext4"));
        assert_eq!(kind(0, 45), Some("nfs4"));
        assert_eq!(kind(9, 9), None);
        for (minor, network) in [(45, true), (46, true), (47, true), (48, false)] {
            assert_eq!(super::network_file_system(kind(0, minor).unwrap()), network, "0:{minor}");
        }
        assert!(!super::network_file_system("ext4"));
    }
}
