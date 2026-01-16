use crate::hasher::FileRef;
#[cfg(test)]
use crate::hasher::FileIdentity;
use crate::reporter::Reporter;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub enum DedupeMode {
    HardLinkWithDelete {
        keep_paths: Vec<PathBuf>,
        delete_paths: Vec<PathBuf>,
    },
    DeleteOnly {
        paths: Vec<PathBuf>,
    },
}

pub struct Deduplicator {
    mode: DedupeMode,
    dry_run: bool,
}

impl Deduplicator {
    pub fn new(mode: DedupeMode, dry_run: bool) -> Self {
        Self { mode, dry_run }
    }

    pub fn deduplicate(&self, duplicates: Vec<Vec<FileRef>>, reporter: &mut Reporter) -> Result<()> {
        match &self.mode {
            DedupeMode::HardLinkWithDelete { keep_paths, delete_paths } => {
                self.dedupe_hard_link_with_delete(duplicates, keep_paths, delete_paths, reporter)
            }
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
            let file_canonical = fs::canonicalize(&f.path).ok();

            paths
                .iter()
                .position(|p| {
                    let dir_canonical = fs::canonicalize(p).ok();
                    match (&file_canonical, &dir_canonical) {
                        (Some(file), Some(dir)) => file.starts_with(dir),
                        _ => f.path.starts_with(p),
                    }
                })
                .unwrap_or(usize::MAX)
        });
        ordered
    }

    fn is_under_any_path(&self, file_path: &Path, paths: &[PathBuf]) -> bool {
        let file_canonical = fs::canonicalize(file_path).ok();

        paths.iter().any(|p| {
            let dir_canonical = fs::canonicalize(p).ok();

            match (&file_canonical, &dir_canonical) {
                (Some(file), Some(dir)) => file.starts_with(dir),
                _ => file_path.starts_with(p),
            }
        })
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

        let canonical_dev = canonical_metadata.dev();

        for dup in duplicates {
            let dup_metadata = match fs::symlink_metadata(&dup.path) {
                Ok(m) => m,
                Err(e) => {
                    reporter.log(&format!(
                        "Warning: Failed to read metadata for {}: {}",
                        dup.path.display(), e
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

            let dup_dev = dup_metadata.dev();

            if canonical_dev != dup_dev {
                reporter.log(&format!(
                    "Warning: Skipping cross-filesystem hard link: {} (different device)",
                    dup.path.display()
                ));
                reporter.skipped_cross_filesystem += 1;
                continue;
            }

            if self.are_already_linked(&canonical.path, &dup.path)? {
                reporter.log_verbose(&format!(
                    "Already linked: {}",
                    dup.path.display()
                ));
                reporter.skipped_already_linked += 1;
                continue;
            }

            reporter.log(&format!(
                "Hard linking {} -> {}",
                dup.path.display(),
                canonical.path.display()
            ));

            if !self.dry_run {
                self.create_hard_link(&canonical.path, &dup.path)?;
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
                fs::remove_file(&file.path)
                    .with_context(|| format!("Failed to delete {}", file.path.display()))?;
            }

            reporter.duplicates_found += 1;
            reporter.space_saved += file.size;
        }

        Ok(())
    }

    fn are_already_linked(&self, path1: &Path, path2: &Path) -> Result<bool> {
        let meta1 = fs::metadata(path1)?;
        let meta2 = fs::metadata(path2)?;
        Ok(meta1.ino() == meta2.ino() && meta1.dev() == meta2.dev())
    }

    fn create_hard_link(&self, original: &Path, duplicate: &Path) -> Result<()> {
        let temp_path = duplicate.with_extension("dedupe_tmp");

        fs::hard_link(original, &temp_path)
            .with_context(|| format!("Failed to create hard link at {}", temp_path.display()))?;

        fs::rename(&temp_path, duplicate)
            .with_context(|| format!("Failed to rename temp file to {}", duplicate.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_files() {
        let dedup = Deduplicator::new(
            DedupeMode::HardLinkWithDelete {
                keep_paths: vec![],
                delete_paths: vec![],
            },
            false,
        );

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
        let dedup = Deduplicator::new(
            DedupeMode::DeleteOnly { paths: vec![] },
            false,
        );

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
}
