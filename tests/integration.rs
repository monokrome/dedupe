use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

fn create_test_file(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn get_binary_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push("dedupe");
    path
}

fn run_dedupe(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(get_binary_path())
        .args(args)
        .output()
        .expect("Failed to execute command")
}

fn run_dedupe_stdout(args: &[&std::ffi::OsStr]) -> String {
    let output = run_dedupe(args);
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn test_dry_run_basic() {
    let dir = TempDir::new().unwrap();

    create_test_file(dir.path(), "file1.txt", b"duplicate content");
    create_test_file(dir.path(), "file2.txt", b"duplicate content");
    create_test_file(dir.path(), "file3.txt", b"unique content");

    let stdout = run_dedupe_stdout(&[dir.path().as_os_str(), "--dry-run".as_ref()]);
    assert!(stdout.contains("DRY RUN"));
    assert!(stdout.contains("Duplicate groups"));
}

#[test]
fn test_hard_link_mode() {
    let dir = TempDir::new().unwrap();

    let file1 = create_test_file(dir.path(), "file1.txt", b"duplicate content");
    let file2 = create_test_file(dir.path(), "file2.txt", b"duplicate content");
    let file3 = create_test_file(dir.path(), "file3.txt", b"unique content");

    let meta1_before = fs::metadata(&file1).unwrap();
    let meta2_before = fs::metadata(&file2).unwrap();

    assert_ne!(meta1_before.ino(), meta2_before.ino());

    let output = run_dedupe(&[dir.path().as_os_str()]);
    assert!(output.status.success());

    let meta1_after = fs::metadata(&file1).unwrap();
    let meta2_after = fs::metadata(&file2).unwrap();

    assert_eq!(meta1_after.ino(), meta2_after.ino());
    assert!(file3.exists());
}

#[test]
fn test_mixed_mode_dry_run() {
    let keep_dir = TempDir::new().unwrap();
    let delete_dir = TempDir::new().unwrap();

    create_test_file(keep_dir.path(), "keep1.txt", b"duplicate content");
    create_test_file(keep_dir.path(), "keep2.txt", b"duplicate content");
    create_test_file(delete_dir.path(), "delete1.txt", b"duplicate content");

    let stdout = run_dedupe_stdout(&[
        keep_dir.path().as_os_str(),
        "--delete".as_ref(),
        delete_dir.path().as_os_str(),
        "--dry-run".as_ref(),
    ]);
    assert!(stdout.contains("Hard link (keep) + Delete"));
}

#[test]
fn test_delete_only_mode_dry_run() {
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();

    create_test_file(dir1.path(), "file1.txt", b"duplicate content");
    create_test_file(dir2.path(), "file2.txt", b"duplicate content");

    let stdout = run_dedupe_stdout(&[
        "--delete".as_ref(),
        dir1.path().as_os_str(),
        dir2.path().as_os_str(),
        "--dry-run".as_ref(),
    ]);
    assert!(stdout.contains("Delete-only (priority-based)"));
}

#[test]
fn test_min_size_filter() {
    let dir = TempDir::new().unwrap();

    create_test_file(dir.path(), "small1.txt", b"hi");
    create_test_file(dir.path(), "small2.txt", b"hi");
    create_test_file(
        dir.path(),
        "large1.txt",
        b"this is a much larger file with duplicate content",
    );
    create_test_file(
        dir.path(),
        "large2.txt",
        b"this is a much larger file with duplicate content",
    );

    let stdout = run_dedupe_stdout(&[
        dir.path().as_os_str(),
        "--min-size".as_ref(),
        "20".as_ref(),
        "--dry-run".as_ref(),
    ]);

    assert!(stdout.contains("large"));
    assert!(!stdout.contains("small") || stdout.contains("larger"));
}

#[test]
fn test_exclude_filter() {
    let dir = TempDir::new().unwrap();

    create_test_file(dir.path(), "keep1.txt", b"duplicate");
    create_test_file(dir.path(), "keep2.txt", b"duplicate");
    create_test_file(dir.path(), "exclude1.bak", b"duplicate");
    create_test_file(dir.path(), "exclude2.bak", b"duplicate");

    let stdout = run_dedupe_stdout(&[
        dir.path().as_os_str(),
        "--exclude".as_ref(),
        ".bak".as_ref(),
        "--dry-run".as_ref(),
    ]);

    assert!(stdout.contains("keep"));
    assert!(!stdout.contains(".bak"));
}

#[test]
fn test_no_duplicates() {
    let dir = TempDir::new().unwrap();

    create_test_file(dir.path(), "file1.txt", b"content1");
    create_test_file(dir.path(), "file2.txt", b"content2");
    create_test_file(dir.path(), "file3.txt", b"content3");

    let stdout = run_dedupe_stdout(&[dir.path().as_os_str()]);
    assert!(stdout.contains("No duplicates found"));
}

#[test]
fn test_empty_directory() {
    let dir = TempDir::new().unwrap();

    let stdout = run_dedupe_stdout(&[dir.path().as_os_str()]);
    assert!(stdout.contains("No duplicates found") || stdout.contains("Found 0 files"));
}
