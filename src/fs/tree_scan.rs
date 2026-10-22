//! The authoritative check of a quarantined worktree before it is deleted.
//! Assessment never walks the tree for safety; this does, once, after the
//! directory has been moved out of everyone's way.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::dirstate::is_special;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("contains a nested repository or worktree: {}", .0.display())]
    NestedRepository(PathBuf),
    #[error("contains a mounted filesystem: {}", .0.display())]
    Mount(PathBuf),
    #[error("contains a special file: {}", .0.display())]
    SpecialFile(PathBuf),
    #[error("changed after assessment: {}", .0.display())]
    ChangedSince(PathBuf),
    #[error("cannot read {}: {error}", .path.display())]
    Unreadable { path: PathBuf, error: String },
}

/// Ctime as nanoseconds since the epoch; the watermark uses the same unit.
fn ctime_nanos(meta: &fs::Metadata) -> i128 {
    i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
}

pub fn system_time_nanos(time: SystemTime) -> i128 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    }
}

/// Refuses `root` if anything below it is a repository (any `.git` but the
/// root's own, or a bare repository), a mount, a special file, or has a
/// ctime at or after `watermark` (nanoseconds). Ctime catches edits to
/// ignored files, which status evidence does not hash; the root itself is
/// exempt because moving it into quarantine updates its ctime.
pub fn scan(root: &Path, watermark: i128) -> Result<(), Refusal> {
    let unreadable = |path: &Path, error: std::io::Error| Refusal::Unreadable {
        path: path.to_path_buf(),
        error: error.to_string(),
    };
    let root_meta = fs::symlink_metadata(root).map_err(|error| unreadable(root, error))?;
    let device = root_meta.dev();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| unreadable(&directory, error))?;
        if directory != root && is_bare_repository(&directory) {
            return Err(Refusal::NestedRepository(directory));
        }
        for entry in entries {
            let entry = entry.map_err(|error| unreadable(&directory, error))?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|error| unreadable(&path, error))?;
            let at_root = directory == root;
            if entry.file_name() == ".git" && !(at_root && meta.is_file()) {
                return Err(Refusal::NestedRepository(path));
            }
            if ctime_nanos(&meta) >= watermark {
                return Err(Refusal::ChangedSince(path));
            }
            let file_type = meta.file_type();
            if is_special(&file_type) {
                return Err(Refusal::SpecialFile(path));
            }
            if file_type.is_dir() {
                if meta.dev() != device {
                    return Err(Refusal::Mount(path));
                }
                directories.push(path);
            }
        }
    }
    Ok(())
}

fn is_bare_repository(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && gix::discover::is_git(dir).is_ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".git"), "gitdir: x\n").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a"), "a").unwrap();
        dir
    }

    fn later() -> i128 {
        system_time_nanos(SystemTime::now() + Duration::from_secs(60))
    }

    #[test]
    fn plain_tree_passes_and_edits_after_the_watermark_do_not() {
        let dir = tree();
        assert_eq!(scan(dir.path(), later()), Ok(()));
        let watermark = system_time_nanos(SystemTime::now());
        std::thread::sleep(Duration::from_millis(20));
        fs::write(dir.path().join("src/ignored.log"), "new").unwrap();
        assert!(matches!(
            scan(dir.path(), watermark),
            Err(Refusal::ChangedSince(_))
        ));
    }

    #[test]
    fn nested_repositories_and_special_files_are_refused() {
        let dir = tree();
        fs::create_dir_all(dir.path().join("src/vendor/.git")).unwrap();
        assert!(matches!(
            scan(dir.path(), later()),
            Err(Refusal::NestedRepository(_))
        ));

        let dir = tree();
        let fifo =
            std::ffi::CString::new(dir.path().join("src/pipe").as_os_str().as_encoded_bytes())
                .unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            scan(dir.path(), later()),
            Err(Refusal::SpecialFile(_))
        ));
    }
}
