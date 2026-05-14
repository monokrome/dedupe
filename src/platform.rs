use anyhow::Result;
use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub trait FileSystemOps {
    fn get_device_id(metadata: &fs::Metadata) -> u64;
    fn are_same_file(path1: &Path, path2: &Path) -> Result<bool>;
    fn is_immutable(path: &Path) -> Result<bool>;
    fn set_immutable(path: &Path, immutable: bool) -> Result<()>;
}

#[cfg(target_os = "linux")]
mod linux_immutable {
    use anyhow::{Context, Result};
    use std::ffi::{c_int, c_long, c_ulong};
    use std::fs::File;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const FS_IMMUTABLE_FL: c_long = 0x00000010;

    const fn ioc(dir: c_ulong, ty: c_ulong, nr: c_ulong, size: c_ulong) -> c_ulong {
        (dir << 30) | (size << 16) | (ty << 8) | nr
    }

    const LONG_SIZE: c_ulong = std::mem::size_of::<c_long>() as c_ulong;
    const FS_IOC_GETFLAGS: c_ulong = ioc(2, b'f' as c_ulong, 1, LONG_SIZE);
    const FS_IOC_SETFLAGS: c_ulong = ioc(1, b'f' as c_ulong, 2, LONG_SIZE);

    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_long) -> c_int;
    }

    fn open_file(path: &Path) -> Result<File> {
        File::open(path).with_context(|| format!("open({}) failed", path.display()))
    }

    pub fn is_immutable(path: &Path) -> Result<bool> {
        let file = open_file(path)?;
        let mut flags: c_long = 0;
        let rc = unsafe { ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
        if rc < 0 {
            return Ok(false);
        }
        Ok((flags & FS_IMMUTABLE_FL) != 0)
    }

    pub fn set_immutable(path: &Path, immutable: bool) -> Result<()> {
        let file = open_file(path)?;
        let mut flags: c_long = 0;
        let rc = unsafe { ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
        if rc < 0 {
            return Ok(());
        }

        let new_flags = if immutable {
            flags | FS_IMMUTABLE_FL
        } else {
            flags & !FS_IMMUTABLE_FL
        };

        if new_flags == flags {
            return Ok(());
        }

        let mut new_flags_storage = new_flags;
        let rc = unsafe { ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS, &mut new_flags_storage) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "FS_IOC_SETFLAGS failed for {} (CAP_LINUX_IMMUTABLE / root required)",
                    path.display()
                )
            });
        }
        Ok(())
    }
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

    #[cfg(target_os = "linux")]
    fn is_immutable(path: &Path) -> Result<bool> {
        linux_immutable::is_immutable(path)
    }

    #[cfg(target_os = "linux")]
    fn set_immutable(path: &Path, immutable: bool) -> Result<()> {
        linux_immutable::set_immutable(path, immutable)
    }

    #[cfg(not(target_os = "linux"))]
    fn is_immutable(_path: &Path) -> Result<bool> {
        Ok(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn set_immutable(_path: &Path, _immutable: bool) -> Result<()> {
        Ok(())
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

    fn is_immutable(_path: &Path) -> Result<bool> {
        Ok(false)
    }

    fn set_immutable(_path: &Path, _immutable: bool) -> Result<()> {
        Ok(())
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

    #[test]
    fn test_fresh_file_not_immutable() {
        let temp_file = NamedTempFile::new().unwrap();
        let result = PlatformFileSystem::is_immutable(temp_file.path()).unwrap();
        assert!(!result, "Fresh temp file should not be immutable");
    }

    #[test]
    fn test_set_immutable_false_is_idempotent() {
        let temp_file = NamedTempFile::new().unwrap();
        // Clearing immutable on a non-immutable file should always succeed
        // (no actual flag change requested, no privileges needed).
        PlatformFileSystem::set_immutable(temp_file.path(), false).unwrap();
    }
}
