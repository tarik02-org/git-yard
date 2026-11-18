//! What is at a worktree path, without following symlinks.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use crate::core::facts::{DirState, EntryKind};

pub fn dir_state(path: &Path) -> DirState {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_dir() {
                DirState::Present {
                    dev: meta.dev(),
                    ino: meta.ino(),
                }
            } else if file_type.is_symlink() {
                DirState::NotADirectory {
                    kind: EntryKind::Symlink,
                }
            } else if file_type.is_file() {
                DirState::NotADirectory {
                    kind: EntryKind::File,
                }
            } else {
                DirState::NotADirectory {
                    kind: EntryKind::Special,
                }
            }
        }
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            // The parent may be reached through a symlink; that still counts.
            let parent_present = path
                .parent()
                .is_some_and(|parent| fs::metadata(parent).is_ok_and(|meta| meta.is_dir()));
            DirState::Missing { parent_present }
        }
        Err(_) => DirState::Unreadable,
    }
}

/// Whether `file_type` is something removal refuses to touch.
pub fn is_special(file_type: &fs::FileType) -> bool {
    file_type.is_fifo()
        || file_type.is_socket()
        || file_type.is_block_device()
        || file_type.is_char_device()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("file"), "").unwrap();
        std::os::unix::fs::symlink(root.join("dir"), root.join("link")).unwrap();

        assert!(dir_state(&root.join("dir")).is_present());
        assert_eq!(
            dir_state(&root.join("link")),
            DirState::NotADirectory {
                kind: EntryKind::Symlink
            }
        );
        assert_eq!(
            dir_state(&root.join("file")),
            DirState::NotADirectory {
                kind: EntryKind::File
            }
        );
        assert_eq!(
            dir_state(&root.join("gone")),
            DirState::Missing {
                parent_present: true
            }
        );
        assert_eq!(
            dir_state(&root.join("unmounted/gone")),
            DirState::Missing {
                parent_present: false
            }
        );
        assert_eq!(
            dir_state(&root.join("file/gone")),
            DirState::Missing {
                parent_present: false
            }
        );
    }
}
