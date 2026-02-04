use crate::hasher::FileIdentity;
use anyhow::Result;
use std::path::Path;
use walkdir::WalkDir;

#[derive(Default)]
pub struct ScanOptions {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub pattern: Option<String>,
    pub exclude: Option<String>,
}

pub fn scan_paths(paths: &[impl AsRef<Path>], options: &ScanOptions) -> Result<Vec<FileIdentity>> {
    let mut files = Vec::new();

    for path in paths {
        let path = path.as_ref();

        if !path.exists() {
            eprintln!("Warning: Path does not exist: {}", path.display());
            continue;
        }

        let walker = WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if let Some(exclude) = &options.exclude {
                    let path_str = e.path().to_string_lossy();
                    !path_str.contains(exclude)
                } else {
                    true
                }
            });

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("Warning: Failed to access path: {}", e);
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to read metadata for {}: {}",
                        entry.path().display(),
                        e
                    );
                    continue;
                }
            };

            let size = metadata.len();

            if !passes_filters(entry.path(), size, options) {
                continue;
            }

            files.push(FileIdentity::new(entry.path().to_path_buf(), size));
        }
    }

    Ok(files)
}

fn passes_filters(path: &Path, size: u64, options: &ScanOptions) -> bool {
    if let Some(min) = options.min_size {
        if size < min {
            return false;
        }
    }

    if let Some(max) = options.max_size {
        if size > max {
            return false;
        }
    }

    let path_str = path.to_string_lossy();

    if let Some(pattern) = &options.pattern {
        if !path_str.contains(pattern) {
            return false;
        }
    }

    if let Some(exclude) = &options.exclude {
        if path_str.contains(exclude) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scan_basic() {
        let dir = TempDir::new().unwrap();
        let file1 = dir.path().join("file1.txt");
        let file2 = dir.path().join("file2.txt");

        fs::write(&file1, b"content").unwrap();
        fs::write(&file2, b"content").unwrap();

        let options = ScanOptions::default();
        let files = scan_paths(&[dir.path()], &options).unwrap();

        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_min_size_filter() {
        let dir = TempDir::new().unwrap();
        let small = dir.path().join("small.txt");
        let large = dir.path().join("large.txt");

        fs::write(&small, b"hi").unwrap();
        fs::write(&large, b"hello world this is larger").unwrap();

        let options = ScanOptions {
            min_size: Some(10),
            ..Default::default()
        };

        let files = scan_paths(&[dir.path()], &options).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("large.txt"));
    }

    #[test]
    fn test_exclude_filter() {
        let dir = TempDir::new().unwrap();
        let keep = dir.path().join("keep.txt");
        let exclude = dir.path().join("exclude.bak");

        fs::write(&keep, b"content").unwrap();
        fs::write(&exclude, b"content").unwrap();

        let options = ScanOptions {
            exclude: Some(".bak".to_string()),
            ..Default::default()
        };

        let files = scan_paths(&[dir.path()], &options).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("keep.txt"));
    }
}
