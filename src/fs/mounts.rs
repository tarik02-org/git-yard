//! Mount points below a directory, read from the system mount table so
//! nothing has to walk the tree. Deleting a worktree must never reach
//! into another filesystem mounted inside it.

use std::io;
use std::path::{Path, PathBuf};

/// The first mount point strictly below `root`.
pub fn mount_below(root: &Path) -> io::Result<Option<PathBuf>> {
    Ok(mount_points()?
        .into_iter()
        .find(|point| point != root && point.starts_with(root)))
}

#[cfg(target_os = "linux")]
fn mount_points() -> io::Result<Vec<PathBuf>> {
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape)
        .collect())
}

/// Mountinfo escapes space, tab, newline and backslash as `\ooo`.
#[cfg(target_os = "linux")]
fn unescape(field: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|digits| {
            let digits = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(digits, 8).ok()
        });
        match (bytes[i], octal) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

#[cfg(target_os = "macos")]
fn mount_points() -> io::Result<Vec<PathBuf>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buffer: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `buffer` at static storage it owns.
    let count = unsafe { libc::getmntinfo(&mut buffer, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `count` entries were written at `buffer`.
    let entries = unsafe { std::slice::from_raw_parts(buffer, count as usize) };
    Ok(entries
        .iter()
        .map(|entry| {
            // SAFETY: f_mntonname is NUL-terminated.
            let name = unsafe { CStr::from_ptr(entry.f_mntonname.as_ptr()) };
            PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes()))
        })
        .collect())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn decodes_escaped_mount_points() {
        assert_eq!(unescape(r"/mnt/my\040disk"), PathBuf::from("/mnt/my disk"));
        assert_eq!(unescape(r"/a\134b"), PathBuf::from(r"/a\b"));
    }

    #[test]
    fn finds_mounts_below_a_directory() {
        assert!(mount_below(Path::new("/")).unwrap().is_some());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(mount_below(dir.path()).unwrap(), None);
    }
}
