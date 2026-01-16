use anyhow::Result;
use indicatif::ProgressBar;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const BUFFER_SIZE: usize = 64 * 1024;

pub type FileRef = Arc<FileIdentity>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub size: u64,
    pub xxhash: Option<u64>,
    pub blake3: Option<blake3::Hash>,
}

impl FileIdentity {
    pub fn new(path: PathBuf, size: u64) -> Self {
        Self {
            path,
            size,
            xxhash: None,
            blake3: None,
        }
    }
}

pub fn compute_xxhash(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BUFFER_SIZE, file);
    let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
    let mut buffer = vec![0; BUFFER_SIZE];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(hasher.digest())
}

pub fn compute_blake3(path: &Path) -> Result<blake3::Hash> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BUFFER_SIZE, file);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BUFFER_SIZE];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(hasher.finalize())
}

pub fn group_by_size(files: Vec<FileIdentity>) -> HashMap<u64, Vec<FileIdentity>> {
    let mut groups: HashMap<u64, Vec<FileIdentity>> = HashMap::new();

    for file in files {
        groups.entry(file.size).or_default().push(file);
    }

    groups.retain(|_, v| v.len() > 1);
    groups
}

pub fn refine_by_xxhash(files: Vec<FileRef>, progress: Option<&ProgressBar>) -> Result<HashMap<u64, Vec<FileRef>>> {
    let hashed_files: Vec<(u64, FileRef)> = files
        .into_par_iter()
        .map(|file| {
            let hash = compute_xxhash(&file.path)?;
            let updated = Arc::new(FileIdentity {
                path: file.path.clone(),
                size: file.size,
                xxhash: Some(hash),
                blake3: file.blake3,
            });
            if let Some(pb) = progress {
                pb.inc(1);
            }
            Ok((hash, updated))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut groups: HashMap<u64, Vec<FileRef>> = HashMap::new();
    for (hash, file) in hashed_files {
        groups.entry(hash).or_default().push(file);
    }

    groups.retain(|_, v| v.len() > 1);
    Ok(groups)
}

pub fn refine_by_blake3(files: Vec<FileRef>, progress: Option<&ProgressBar>) -> Result<HashMap<blake3::Hash, Vec<FileRef>>> {
    let hashed_files: Vec<(blake3::Hash, FileRef)> = files
        .into_par_iter()
        .map(|file| {
            let hash = compute_blake3(&file.path)?;
            let updated = Arc::new(FileIdentity {
                path: file.path.clone(),
                size: file.size,
                xxhash: file.xxhash,
                blake3: Some(hash),
            });
            if let Some(pb) = progress {
                pb.inc(1);
            }
            Ok((hash, updated))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut groups: HashMap<blake3::Hash, Vec<FileRef>> = HashMap::new();
    for (hash, file) in hashed_files {
        groups.entry(hash).or_default().push(file);
    }

    groups.retain(|_, v| v.len() > 1);
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_hash_consistency() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"test content").unwrap();
        file.flush().unwrap();

        let path = file.path();
        let hash1 = compute_xxhash(path).unwrap();
        let hash2 = compute_xxhash(path).unwrap();
        assert_eq!(hash1, hash2);

        let blake1 = compute_blake3(path).unwrap();
        let blake2 = compute_blake3(path).unwrap();
        assert_eq!(blake1, blake2);
    }

    #[test]
    fn test_different_content_different_hash() {
        let mut file1 = NamedTempFile::new().unwrap();
        let mut file2 = NamedTempFile::new().unwrap();

        file1.write_all(b"content1").unwrap();
        file2.write_all(b"content2").unwrap();
        file1.flush().unwrap();
        file2.flush().unwrap();

        let hash1 = compute_xxhash(file1.path()).unwrap();
        let hash2 = compute_xxhash(file2.path()).unwrap();
        assert_ne!(hash1, hash2);

        let blake1 = compute_blake3(file1.path()).unwrap();
        let blake2 = compute_blake3(file2.path()).unwrap();
        assert_ne!(blake1, blake2);
    }

    #[test]
    fn test_group_by_size() {
        let files = vec![
            FileIdentity::new(PathBuf::from("a"), 100),
            FileIdentity::new(PathBuf::from("b"), 100),
            FileIdentity::new(PathBuf::from("c"), 200),
        ];

        let groups = group_by_size(files);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.get(&100).unwrap().len(), 2);
    }
}
