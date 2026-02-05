use anyhow::Result;
use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub trait FileSystemOps {
    fn get_device_id(metadata: &fs::Metadata) -> u64;
    fn are_same_file(path1: &Path, path2: &Path) -> Result<bool>;
}

#[cfg(unix)]
pub struct UnixFileSystem;

#[cfg(unix)]
impl FileSystemOps for UnixFileSystem {
    fn get_device_id(metadata: &fs::Metadata) -> u64 {
        metadata.dev()
    }

    fn are_same_file(path1: &Path, path2: &Path) -> Result<bool> {
        let meta1 = fs::metadata(path1)?;
        let meta2 = fs::metadata(path2)?;
        Ok(meta1.ino() == meta2.ino() && meta1.dev() == meta2.dev())
    }
}

#[cfg(windows)]
pub struct WindowsFileSystem;

#[cfg(windows)]
impl FileSystemOps for WindowsFileSystem {
    fn get_device_id(_metadata: &fs::Metadata) -> u64 {
        // Windows doesn't expose device ID through stable std API
        // Return 0 to indicate unknown - cross-filesystem check will be skipped
        0
    }

    fn are_same_file(path1: &Path, path2: &Path) -> Result<bool> {
        // Use same_file crate which handles Windows correctly via winapi
        Ok(same_file::is_same_file(path1, path2)?)
    }
}

#[cfg(unix)]
pub type PlatformFileSystem = UnixFileSystem;

#[cfg(windows)]
pub type PlatformFileSystem = WindowsFileSystem;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_same_file_detection() {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "test content").unwrap();
        let path = temp_file.path();

        let result = PlatformFileSystem::are_same_file(path, path).unwrap();
        assert!(result, "Same path should be detected as same file");
    }

    #[test]
    fn test_different_files() {
        let file1 = NamedTempFile::new().unwrap();
        let file2 = NamedTempFile::new().unwrap();

        let result = PlatformFileSystem::are_same_file(file1.path(), file2.path()).unwrap();
        assert!(!result, "Different files should not be detected as same");
    }

    #[test]
    fn test_device_id_extraction() {
        let temp_file = NamedTempFile::new().unwrap();
        let metadata = fs::metadata(temp_file.path()).unwrap();
        let device_id = PlatformFileSystem::get_device_id(&metadata);
        // On Unix, device ID should be > 0; on Windows, we return 0
        assert!(device_id > 0 || cfg!(windows), "Device ID should be valid");
    }
}
