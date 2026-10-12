//! Renames that never replace an existing destination, made durable.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Atomically renames `from` to `to`, failing if `to` exists. Moving a mount
/// point fails (`EBUSY`/`EXDEV`), which removal relies on. Both parents are
/// synced afterwards. Filesystems without no-replace support fail rather
/// than falling back to a racy check-then-rename.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from_c, to_c) = (c_path(from)?, c_path(to)?);
    #[cfg(target_os = "linux")]
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let result = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    sync_parent(to)?;
    if from.parent() != to.parent() {
        sync_parent(from)?;
    }
    Ok(())
}

pub fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_replace() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let error = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir(&b).unwrap();
        rename_noreplace(&a, &b).unwrap();
        assert!(b.is_dir() && !a.exists());
    }
}
