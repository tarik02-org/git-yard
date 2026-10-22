//! Path canonicalization that tolerates a missing leaf.

use std::io;
use std::path::{Path, PathBuf};

/// Canonicalizes `path`; when only its last component is missing, the
/// parent is canonicalized and the name appended, so a deleted worktree
/// directory can still be named on the command line.
pub fn canonicalize_allow_missing_leaf(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(error);
            };
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            Ok(parent.canonicalize()?.join(name))
        }
        Err(error) => Err(error),
    }
}

/// Lexically removes `.` and `..` components and duplicate separators,
/// for paths recorded by Git that may not exist any more.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_leaf_resolves_against_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("a")).unwrap();
        std::os::unix::fs::symlink(real.join("a"), real.join("link")).unwrap();
        assert_eq!(
            canonicalize_allow_missing_leaf(&real.join("link/gone")).unwrap(),
            real.join("a/gone")
        );
        assert!(canonicalize_allow_missing_leaf(&real.join("x/y")).is_err());
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }
}
