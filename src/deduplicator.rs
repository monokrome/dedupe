#[cfg(test)]
use crate::hasher::FileIdentity;
use crate::hasher::FileRef;
use crate::platform::{FileSystemOps, PlatformFileSystem};
use crate::reporter::Reporter;
use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub enum DedupeMode {
    HardLinkWithDelete {
        keep_paths: Vec<PathBuf>,
        delete_paths: Vec<PathBuf>,
    },
    DeleteOnly {
        paths: Vec<PathBuf>,
    },
}

/// Builds a short temp path in the duplicate's directory. The name is fixed
/// size (not derived from the original) so it never exceeds NAME_MAX even when
/// the original filename is at the filesystem limit. Must stay in the same
/// directory so the temp is on the same filesystem as the hard link target.
fn temp_path_for(duplicate: &Path) -> PathBuf {
    let dir = duplicate.parent().unwrap_or_else(|| Path::new("."));
    let name = format!(
        ".dedupe-tmp.{}.{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    dir.join(name)
}

fn path_matches(file_path: &Path, dir_path: &Path) -> bool {
    let file_canonical = fs::canonicalize(file_path).ok();
    let dir_canonical = fs::canonicalize(dir_path).ok();

    match (&file_canonical, &dir_canonical) {
        (Some(file), Some(dir)) => file.starts_with(dir),
        _ => file_path.starts_with(dir_path),
    }
}

pub struct Deduplicator<'a> {
    mode: &'a DedupeMode,
    dry_run: bool,
    unlock_immutable: bool,
}

enum LinkOutcome {
    Linked,
    SkippedImmutable,
}

enum DeleteOutcome {
    Deleted,
    SkippedImmutable,
}

impl<'a> Deduplicator<'a> {
    pub fn new_ref(mode: &'a DedupeMode, dry_run: bool, unlock_immutable: bool) -> Self {
        Self {
            mode,
            dry_run,
            unlock_immutable,
        }
    }

    pub fn deduplicate(
        &self,
        duplicates: Vec<Vec<FileRef>>,
        reporter: &mut Reporter,
    ) -> Result<()> {
        match self.mode {
            DedupeMode::HardLinkWithDelete {
                keep_paths,
                delete_paths,
            } => self.dedupe_hard_link_with_delete(duplicates, keep_paths, delete_paths, reporter),
            DedupeMode::DeleteOnly { paths } => {
                self.dedupe_delete_only(duplicates, paths, reporter)
            }
        }
    }

    fn dedupe_hard_link_with_delete(
        &self,
        duplicates: Vec<Vec<FileRef>>,
        keep_paths: &[PathBuf],
        delete_paths: &[PathBuf],
        reporter: &mut Reporter,
    ) -> Result<()> {
        for group in duplicates {
            let (keep_files, delete_files) = self.partition_files(&group, keep_paths, delete_paths);

            if keep_files.is_empty() {
                reporter.log_verbose(&format!(
                    "Skipping group: no files in keep paths ({})",
                    group.first().map(|f| f.size).unwrap_or(0)
                ));
                continue;
            }

            self.hard_link_keep_files(&keep_files, reporter)?;
            self.delete_duplicate_files(&delete_files, reporter)?;
        }

        Ok(())
    }

    fn dedupe_delete_only(
        &self,
        duplicates: Vec<Vec<FileRef>>,
        paths: &[PathBuf],
        reporter: &mut Reporter,
    ) -> Result<()> {
        for group in duplicates {
            let ordered = self.order_by_priority(&group, paths);

            if ordered.is_empty() {
                continue;
            }

            let to_delete = &ordered[1..];
            self.delete_duplicate_files(to_delete, reporter)?;
        }

        Ok(())
    }

    fn partition_files(
        &self,
        files: &[FileRef],
        keep_paths: &[PathBuf],
        delete_paths: &[PathBuf],
    ) -> (Vec<FileRef>, Vec<FileRef>) {
        let mut keep_files = Vec::new();
        let mut delete_files = Vec::new();

        for file in files {
            if self.is_under_any_path(&file.path, keep_paths) {
                keep_files.push(Arc::clone(file));
            } else if self.is_under_any_path(&file.path, delete_paths) {
                delete_files.push(Arc::clone(file));
            }
        }

        (keep_files, delete_files)
    }

    fn order_by_priority(&self, files: &[FileRef], paths: &[PathBuf]) -> Vec<FileRef> {
        let mut ordered: Vec<FileRef> = files.iter().map(Arc::clone).collect();
        ordered.sort_by_key(|f| {
            paths
                .iter()
                .position(|p| path_matches(&f.path, p))
                .unwrap_or(usize::MAX)
        });
        ordered
    }

    fn is_under_any_path(&self, file_path: &Path, paths: &[PathBuf]) -> bool {
        paths.iter().any(|p| path_matches(file_path, p))
    }

    fn hard_link_keep_files(&self, files: &[FileRef], reporter: &mut Reporter) -> Result<()> {
        if files.len() < 2 {
            return Ok(());
        }

        let canonical = &files[0];
        let duplicates = &files[1..];

        let canonical_metadata = fs::symlink_metadata(&canonical.path)?;

        if canonical_metadata.file_type().is_symlink() {
            reporter.log(&format!(
                "Warning: Canonical file is a symlink, skipping group: {}",
                canonical.path.display()
            ));
            return Ok(());
        }

        let canonical_dev = PlatformFileSystem::get_device_id(&canonical_metadata);

        for dup in duplicates {
            let dup_metadata = match fs::symlink_metadata(&dup.path) {
                Ok(m) => m,
                Err(e) => {
                    reporter.log(&format!(
                        "Warning: Failed to read metadata for {}: {}",
                        dup.path.display(),
                        e
                    ));
                    continue;
                }
            };

            if dup_metadata.file_type().is_symlink() {
                reporter.log(&format!(
                    "Warning: Skipping symlink: {}",
                    dup.path.display()
                ));
                continue;
            }

            let dup_dev = PlatformFileSystem::get_device_id(&dup_metadata);

            if canonical_dev != dup_dev {
                reporter.log(&format!(
                    "Warning: Skipping cross-filesystem hard link: {} (different device)",
                    dup.path.display()
                ));
                reporter.skipped_cross_filesystem += 1;
                continue;
            }

            if self.are_already_linked(&canonical.path, &dup.path)? {
                reporter.log_verbose(&format!("Already linked: {}", dup.path.display()));
                reporter.skipped_already_linked += 1;
                continue;
            }

            reporter.log(&format!(
                "Hard linking {} -> {}",
                dup.path.display(),
                canonical.path.display()
            ));

            if !self.dry_run {
                match self.try_hard_link(&canonical.path, &dup.path, reporter) {
                    Ok(LinkOutcome::Linked) => {}
                    Ok(LinkOutcome::SkippedImmutable) => continue,
                    Err(e) => {
                        reporter.log(&format!(
                            "Warning: Failed to hard link {}: {:#}",
                            dup.path.display(),
                            e
                        ));
                        reporter.errors += 1;
                        continue;
                    }
                }
            }

            reporter.duplicates_found += 1;
            reporter.space_saved += canonical.size;
        }

        Ok(())
    }

    fn delete_duplicate_files(&self, files: &[FileRef], reporter: &mut Reporter) -> Result<()> {
        for file in files {
            reporter.log(&format!("Deleting {}", file.path.display()));

            if !self.dry_run {
                match self.try_delete(&file.path, reporter) {
                    Ok(DeleteOutcome::Deleted) => {}
                    Ok(DeleteOutcome::SkippedImmutable) => continue,
                    Err(e) => {
                        reporter.log(&format!(
                            "Warning: Failed to delete {}: {:#}",
                            file.path.display(),
                            e
                        ));
                        reporter.errors += 1;
                        continue;
                    }
                }
            }

            reporter.duplicates_found += 1;
            reporter.space_saved += file.size;
        }

        Ok(())
    }

    fn try_hard_link(
        &self,
        canonical: &Path,
        duplicate: &Path,
        reporter: &mut Reporter,
    ) -> Result<LinkOutcome> {
        let canonical_imm = PlatformFileSystem::is_immutable(canonical).unwrap_or(false);
        let duplicate_imm = PlatformFileSystem::is_immutable(duplicate).unwrap_or(false);

        if (canonical_imm || duplicate_imm) && !self.unlock_immutable {
            reporter.log(&format!(
                "Warning: Skipping immutable file (use --unlock-immutable): {}",
                duplicate.display()
            ));
            reporter.skipped_immutable += 1;
            return Ok(LinkOutcome::SkippedImmutable);
        }

        if canonical_imm {
            PlatformFileSystem::set_immutable(canonical, false)
                .with_context(|| format!("Failed to clear immutable on {}", canonical.display()))?;
        }
        if duplicate_imm {
            PlatformFileSystem::set_immutable(duplicate, false)
                .with_context(|| format!("Failed to clear immutable on {}", duplicate.display()))?;
        }

        let link_result = self.create_hard_link(canonical, duplicate);

        match link_result {
            Ok(()) => {
                if canonical_imm || duplicate_imm {
                    PlatformFileSystem::set_immutable(canonical, true).with_context(|| {
                        format!("Failed to restore immutable on {}", canonical.display())
                    })?;
                }
                Ok(LinkOutcome::Linked)
            }
            Err(e) => {
                if canonical_imm {
                    let _ = PlatformFileSystem::set_immutable(canonical, true);
                }
                if duplicate_imm && duplicate.exists() {
                    let _ = PlatformFileSystem::set_immutable(duplicate, true);
                }
                Err(e)
            }
        }
    }

    fn try_delete(&self, path: &Path, reporter: &mut Reporter) -> Result<DeleteOutcome> {
        let was_immutable = PlatformFileSystem::is_immutable(path).unwrap_or(false);

        if was_immutable && !self.unlock_immutable {
            reporter.log(&format!(
                "Warning: Skipping immutable file (use --unlock-immutable): {}",
                path.display()
            ));
            reporter.skipped_immutable += 1;
            return Ok(DeleteOutcome::SkippedImmutable);
        }

        if was_immutable {
            PlatformFileSystem::set_immutable(path, false)
                .with_context(|| format!("Failed to clear immutable on {}", path.display()))?;
        }

        let result =
            fs::remove_file(path).with_context(|| format!("Failed to delete {}", path.display()));

        if result.is_err() && was_immutable && path.exists() {
            let _ = PlatformFileSystem::set_immutable(path, true);
        }

        result.map(|_| DeleteOutcome::Deleted)
    }

    fn are_already_linked(&self, path1: &Path, path2: &Path) -> Result<bool> {
        PlatformFileSystem::are_same_file(path1, path2)
    }

    fn create_hard_link(&self, original: &Path, duplicate: &Path) -> Result<()> {
        let temp_path = temp_path_for(duplicate);

        // A previous interrupted run may have left this temp behind.
        match fs::remove_file(&temp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to remove stale temp {}", temp_path.display())
                })
            }
        }

        fs::hard_link(original, &temp_path)
            .with_context(|| format!("Failed to create hard link at {}", temp_path.display()))?;

        if let Err(e) = fs::rename(&temp_path, duplicate) {
            let _ = fs::remove_file(&temp_path);
            return Err(e)
                .with_context(|| format!("Failed to rename temp file to {}", duplicate.display()));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_files() {
        let mode = DedupeMode::HardLinkWithDelete {
            keep_paths: vec![],
            delete_paths: vec![],
        };
        let dedup = Deduplicator::new_ref(&mode, false, false);

        let files: Vec<FileRef> = vec![
            Arc::new(FileIdentity::new(PathBuf::from("/keep/file1"), 100)),
            Arc::new(FileIdentity::new(PathBuf::from("/keep/file2"), 100)),
            Arc::new(FileIdentity::new(PathBuf::from("/delete/file3"), 100)),
        ];

        let keep_paths = vec![PathBuf::from("/keep")];
        let delete_paths = vec![PathBuf::from("/delete")];

        let (keep, delete) = dedup.partition_files(&files, &keep_paths, &delete_paths);

        assert_eq!(keep.len(), 2);
        assert_eq!(delete.len(), 1);
    }

    #[test]
    fn test_order_by_priority() {
        let mode = DedupeMode::DeleteOnly { paths: vec![] };
        let dedup = Deduplicator::new_ref(&mode, false, false);

        let files: Vec<FileRef> = vec![
            Arc::new(FileIdentity::new(PathBuf::from("/c/file"), 100)),
            Arc::new(FileIdentity::new(PathBuf::from("/a/file"), 100)),
            Arc::new(FileIdentity::new(PathBuf::from("/b/file"), 100)),
        ];

        let paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];

        let ordered = dedup.order_by_priority(&files, &paths);

        assert_eq!(ordered[0].path, PathBuf::from("/a/file"));
        assert_eq!(ordered[1].path, PathBuf::from("/b/file"));
        assert_eq!(ordered[2].path, PathBuf::from("/c/file"));
    }

    #[cfg(unix)]
    #[test]
    fn test_hard_link_with_filename_at_name_max() {
        use std::os::unix::fs::MetadataExt;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();

        let original = dir.path().join("orig");
        fs::write(&original, b"shared content").unwrap();

        // 250-byte name: appending ".dedupe_tmp" (old behaviour) would push the
        // component past the 255-byte NAME_MAX and fail with ENAMETOOLONG.
        let long_name = "a".repeat(250);
        let duplicate = dir.path().join(&long_name);
        fs::write(&duplicate, b"shared content").unwrap();

        let mode = DedupeMode::HardLinkWithDelete {
            keep_paths: vec![],
            delete_paths: vec![],
        };
        let dedup = Deduplicator::new_ref(&mode, false, false);

        dedup
            .create_hard_link(&original, &duplicate)
            .expect("link with NAME_MAX filename should succeed");

        let a = fs::metadata(&original).unwrap();
        let b = fs::metadata(&duplicate).unwrap();
        assert_eq!(a.ino(), b.ino(), "files should share an inode");
    }

    #[cfg(unix)]
    #[test]
    fn test_failed_link_does_not_abort_remaining_groups() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();

        let canonical_a = dir.path().join("a_canonical");
        fs::write(&canonical_a, b"group a").unwrap();
        let ro_dir = dir.path().join("readonly");
        fs::create_dir(&ro_dir).unwrap();
        let dup_a = ro_dir.join("dup_a");
        fs::write(&dup_a, b"group a").unwrap();
        // No write permission on the directory -> temp creation fails (EACCES).
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let canonical_b = dir.path().join("b_canonical");
        fs::write(&canonical_b, b"group b").unwrap();
        let dup_b = dir.path().join("b_dup");
        fs::write(&dup_b, b"group b").unwrap();

        let keep = vec![dir.path().to_path_buf()];
        let mode = DedupeMode::HardLinkWithDelete {
            keep_paths: keep,
            delete_paths: vec![],
        };
        let dedup = Deduplicator::new_ref(&mode, false, false);

        let groups = vec![
            vec![
                Arc::new(FileIdentity::new(canonical_a.clone(), 7)),
                Arc::new(FileIdentity::new(dup_a.clone(), 7)),
            ],
            vec![
                Arc::new(FileIdentity::new(canonical_b.clone(), 7)),
                Arc::new(FileIdentity::new(dup_b.clone(), 7)),
            ],
        ];

        let mut reporter = Reporter::new(true, false, false);
        let result = dedup.deduplicate(groups, &mut reporter);

        // Restore perms so the TempDir can be cleaned up.
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_ok(), "one bad file must not abort the run");
        assert_eq!(reporter.errors, 1, "the failed link should be counted");
        assert_eq!(reporter.duplicates_found, 1, "group b should still link");

        use std::os::unix::fs::MetadataExt;
        let cb = fs::metadata(&canonical_b).unwrap();
        let db = fs::metadata(&dup_b).unwrap();
        assert_eq!(cb.ino(), db.ino(), "group b must be linked");
    }
}
