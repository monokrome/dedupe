use crate::hasher::FileIdentity;
use crate::scanner::ScanOptions;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Stage {
    SizeGrouped,
    XxhashDone,
    Blake3Verified,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::SizeGrouped => "size grouping",
            Stage::XxhashDone => "xxHash",
            Stage::Blake3Verified => "BLAKE3",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableFile {
    pub path: PathBuf,
    pub size: u64,
    pub xxhash: Option<u64>,
    pub blake3: Option<String>,
}

impl SerializableFile {
    pub fn from_identity(file: &FileIdentity) -> Self {
        Self {
            path: file.path.clone(),
            size: file.size,
            xxhash: file.xxhash,
            blake3: file.blake3.map(|h| h.to_hex().to_string()),
        }
    }

    pub fn to_identity(&self) -> Result<FileIdentity> {
        let blake3 = match &self.blake3 {
            Some(hex) => {
                let bytes = hex_to_bytes(hex)
                    .with_context(|| format!("Invalid blake3 hex in state file: {hex}"))?;
                let hash = blake3::Hash::from_bytes(bytes);
                Some(hash)
            }
            None => None,
        };

        Ok(FileIdentity {
            path: self.path.clone(),
            size: self.size,
            xxhash: self.xxhash,
            blake3,
        })
    }
}

fn hex_to_bytes(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        bail!("Expected 64 hex chars, got {}", hex.len());
    }

    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("Invalid hex at position {}", i * 2))?;
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StageData {
    SizeGroup(Vec<SerializableFile>),
    Xxhash(Vec<SerializableFile>),
    Blake3(Vec<Vec<SerializableFile>>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableScanOptions {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub pattern: Option<String>,
    pub exclude: Option<String>,
}

impl SerializableScanOptions {
    pub fn from_options(opts: &ScanOptions) -> Self {
        Self {
            min_size: opts.min_size,
            max_size: opts.max_size,
            pattern: opts.pattern.clone(),
            exclude: opts.exclude.clone(),
        }
    }

    pub fn matches(&self, opts: &ScanOptions) -> bool {
        self.min_size == opts.min_size
            && self.max_size == opts.max_size
            && self.pattern == opts.pattern
            && self.exclude == opts.exclude
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    pub version: u32,
    pub stage: Stage,
    pub scan_options: SerializableScanOptions,
    pub paths: Vec<PathBuf>,
    pub timestamp: String,
    pub data: StageData,
}

pub fn save_checkpoint(path: &Path, state: &CheckpointState) -> Result<()> {
    let json =
        serde_json::to_string_pretty(state).context("Failed to serialize checkpoint state")?;

    let tmp_path = path.with_extension("json.tmp");

    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("Failed to create temp state file: {}", tmp_path.display()))?;

    file.write_all(json.as_bytes())
        .with_context(|| format!("Failed to write temp state file: {}", tmp_path.display()))?;

    file.sync_all()
        .with_context(|| format!("Failed to sync temp state file: {}", tmp_path.display()))?;

    drop(file);

    fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename temp state file to {}", path.display()))?;

    Ok(())
}

pub fn load_checkpoint(path: &Path) -> Result<Option<CheckpointState>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read state file: {}", path.display()))?;

    let state: CheckpointState = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse state file: {}", path.display()))?;

    Ok(Some(state))
}

pub fn validate_checkpoint(
    state: &CheckpointState,
    paths: &[PathBuf],
    scan_options: &ScanOptions,
) -> Result<bool> {
    if state.version != STATE_VERSION {
        eprintln!(
            "Warning: State file version {} does not match current version {}",
            state.version, STATE_VERSION
        );
        return Ok(false);
    }

    if state.paths != paths {
        eprintln!("Warning: State file paths do not match current paths");
        eprintln!("  State: {:?}", state.paths);
        eprintln!("  Current: {:?}", paths);
        return Ok(false);
    }

    if !state.scan_options.matches(scan_options) {
        eprintln!("Warning: Scan options do not match state file");
        eprintln!(
            "  State: min={:?} max={:?} pattern={:?} exclude={:?}",
            state.scan_options.min_size,
            state.scan_options.max_size,
            state.scan_options.pattern,
            state.scan_options.exclude
        );
        eprintln!(
            "  Current: min={:?} max={:?} pattern={:?} exclude={:?}",
            scan_options.min_size,
            scan_options.max_size,
            scan_options.pattern,
            scan_options.exclude
        );
        return Ok(false);
    }

    let sample = spot_check_files(state);
    if !sample {
        eprintln!("Warning: Some files from state no longer exist or have changed size");
        return Ok(false);
    }

    Ok(true)
}

fn spot_check_files(state: &CheckpointState) -> bool {
    let files: Vec<&SerializableFile> = match &state.data {
        StageData::SizeGroup(files) | StageData::Xxhash(files) => files.iter().take(10).collect(),
        StageData::Blake3(groups) => groups.iter().flatten().take(10).collect(),
    };

    for file in files {
        match fs::metadata(&file.path) {
            Ok(meta) => {
                if meta.len() != file.size {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }

    true
}

pub fn delete_state_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("Failed to delete state file: {}", path.display()))?;
    }
    Ok(())
}

pub fn create_checkpoint(
    stage: Stage,
    paths: &[PathBuf],
    scan_options: &ScanOptions,
    data: StageData,
) -> CheckpointState {
    CheckpointState {
        version: STATE_VERSION,
        stage,
        scan_options: SerializableScanOptions::from_options(scan_options),
        paths: paths.to_vec(),
        timestamp: chrono_free_timestamp(),
        data,
    }
}

fn chrono_free_timestamp() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

pub fn files_to_serializable(files: &[FileIdentity]) -> Vec<SerializableFile> {
    files.iter().map(SerializableFile::from_identity).collect()
}

pub fn refs_to_serializable(files: &[Arc<FileIdentity>]) -> Vec<SerializableFile> {
    files
        .iter()
        .map(|f| SerializableFile::from_identity(f))
        .collect()
}

pub fn serializable_to_identities(files: &[SerializableFile]) -> Result<Vec<FileIdentity>> {
    files.iter().map(|f| f.to_identity()).collect()
}

pub fn groups_to_serializable(groups: &[Vec<Arc<FileIdentity>>]) -> Vec<Vec<SerializableFile>> {
    groups
        .iter()
        .map(|group| refs_to_serializable(group))
        .collect()
}

pub fn serializable_to_ref_groups(
    groups: &[Vec<SerializableFile>],
) -> Result<Vec<Vec<Arc<FileIdentity>>>> {
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|f| f.to_identity().map(Arc::new))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_identity(path: &str, size: u64) -> FileIdentity {
        FileIdentity::new(PathBuf::from(path), size)
    }

    fn make_identity_with_xxhash(path: &str, size: u64, xxhash: u64) -> FileIdentity {
        FileIdentity {
            path: PathBuf::from(path),
            size,
            xxhash: Some(xxhash),
            blake3: None,
        }
    }

    fn make_checkpoint(
        version: u32,
        paths: Vec<PathBuf>,
        scan_options: SerializableScanOptions,
    ) -> CheckpointState {
        CheckpointState {
            version,
            stage: Stage::SizeGrouped,
            scan_options,
            paths,
            timestamp: "0".to_string(),
            data: StageData::SizeGroup(vec![]),
        }
    }

    fn default_scan_options() -> SerializableScanOptions {
        SerializableScanOptions {
            min_size: None,
            max_size: None,
            pattern: None,
            exclude: None,
        }
    }

    #[test]
    fn test_serializable_file_roundtrip_no_hashes() {
        let identity = make_identity("/tmp/test", 1024);
        let serialized = SerializableFile::from_identity(&identity);
        let restored = serialized.to_identity().unwrap();

        assert_eq!(restored.path, identity.path);
        assert_eq!(restored.size, identity.size);
        assert_eq!(restored.xxhash, None);
        assert_eq!(restored.blake3, None);
    }

    #[test]
    fn test_serializable_file_roundtrip_with_xxhash() {
        let identity = make_identity_with_xxhash("/tmp/test", 2048, 0xDEADBEEF);
        let serialized = SerializableFile::from_identity(&identity);
        let restored = serialized.to_identity().unwrap();

        assert_eq!(restored.xxhash, Some(0xDEADBEEF));
    }

    #[test]
    fn test_serializable_file_roundtrip_with_blake3() {
        let hash = blake3::hash(b"test data");
        let identity = FileIdentity {
            path: PathBuf::from("/tmp/test"),
            size: 9,
            xxhash: Some(42),
            blake3: Some(hash),
        };

        let serialized = SerializableFile::from_identity(&identity);
        let restored = serialized.to_identity().unwrap();

        assert_eq!(restored.blake3.unwrap(), hash);
        assert_eq!(restored.xxhash, Some(42));
    }

    #[test]
    fn test_hex_to_bytes_valid() {
        let hash = blake3::hash(b"test");
        let hex = hash.to_hex().to_string();
        let bytes = hex_to_bytes(&hex).unwrap();
        assert_eq!(blake3::Hash::from_bytes(bytes), hash);
    }

    #[test]
    fn test_hex_to_bytes_invalid_length() {
        assert!(hex_to_bytes("abc").is_err());
    }

    #[test]
    fn test_hex_to_bytes_invalid_chars() {
        let bad_hex = "zz".repeat(32);
        assert!(hex_to_bytes(&bad_hex).is_err());
    }

    #[test]
    fn test_save_and_load_checkpoint() {
        let dir = TempDir::new().unwrap();
        let state_path = dir.path().join("state.json");

        let files = vec![
            SerializableFile {
                path: PathBuf::from("/a/file1"),
                size: 100,
                xxhash: None,
                blake3: None,
            },
            SerializableFile {
                path: PathBuf::from("/b/file2"),
                size: 100,
                xxhash: None,
                blake3: None,
            },
        ];

        let state = CheckpointState {
            version: 1,
            stage: Stage::SizeGrouped,
            scan_options: SerializableScanOptions {
                min_size: Some(1024),
                max_size: None,
                pattern: None,
                exclude: Some(".git".to_string()),
            },
            paths: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            timestamp: "12345".to_string(),
            data: StageData::SizeGroup(files),
        };

        save_checkpoint(&state_path, &state).unwrap();
        let loaded = load_checkpoint(&state_path).unwrap().unwrap();

        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.stage, Stage::SizeGrouped);
        assert_eq!(loaded.paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn test_load_nonexistent_returns_none() {
        let result = load_checkpoint(Path::new("/nonexistent/state.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_version_mismatch() {
        let state = make_checkpoint(999, vec![], default_scan_options());
        let options = ScanOptions::default();
        assert!(!validate_checkpoint(&state, &[], &options).unwrap());
    }

    #[test]
    fn test_validate_path_mismatch() {
        let state = make_checkpoint(1, vec![PathBuf::from("/old/path")], default_scan_options());
        let options = ScanOptions::default();
        assert!(!validate_checkpoint(&state, &[PathBuf::from("/new/path")], &options).unwrap());
    }

    #[test]
    fn test_validate_options_mismatch() {
        let mut opts = default_scan_options();
        opts.min_size = Some(1024);
        let state = make_checkpoint(1, vec![], opts);

        let options = ScanOptions {
            min_size: Some(2048),
            ..Default::default()
        };
        assert!(!validate_checkpoint(&state, &[], &options).unwrap());
    }

    #[test]
    fn test_validate_matching_state() {
        let state = make_checkpoint(1, vec![], default_scan_options());
        let options = ScanOptions::default();
        assert!(validate_checkpoint(&state, &[], &options).unwrap());
    }

    #[test]
    fn test_atomic_write_leaves_no_tmp_on_success() {
        let dir = TempDir::new().unwrap();
        let state_path = dir.path().join("state.json");

        let state = make_checkpoint(1, vec![], default_scan_options());
        save_checkpoint(&state_path, &state).unwrap();

        assert!(state_path.exists());
        assert!(!state_path.with_extension("json.tmp").exists());
    }

    #[test]
    fn test_delete_state_file() {
        let dir = TempDir::new().unwrap();
        let state_path = dir.path().join("state.json");

        fs::write(&state_path, "{}").unwrap();
        assert!(state_path.exists());

        delete_state_file(&state_path).unwrap();
        assert!(!state_path.exists());
    }

    #[test]
    fn test_delete_nonexistent_state_file() {
        let result = delete_state_file(Path::new("/nonexistent/state.json"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_scan_options_matches() {
        let opts = ScanOptions {
            min_size: Some(100),
            max_size: Some(1000),
            pattern: Some("*.txt".to_string()),
            exclude: Some(".git".to_string()),
        };

        let serialized = SerializableScanOptions::from_options(&opts);
        assert!(serialized.matches(&opts));

        let different = ScanOptions {
            min_size: Some(200),
            ..Default::default()
        };
        assert!(!serialized.matches(&different));
    }

    #[test]
    fn test_files_to_serializable() {
        let files = vec![make_identity("/a", 100), make_identity("/b", 200)];

        let serialized = files_to_serializable(&files);
        assert_eq!(serialized.len(), 2);
        assert_eq!(serialized[0].path, PathBuf::from("/a"));
        assert_eq!(serialized[1].size, 200);
    }

    #[test]
    fn test_groups_roundtrip() {
        let hash = blake3::hash(b"content");
        let groups = vec![vec![
            Arc::new(FileIdentity {
                path: PathBuf::from("/a"),
                size: 100,
                xxhash: Some(42),
                blake3: Some(hash),
            }),
            Arc::new(FileIdentity {
                path: PathBuf::from("/b"),
                size: 100,
                xxhash: Some(42),
                blake3: Some(hash),
            }),
        ]];

        let serialized = groups_to_serializable(&groups);
        let restored = serializable_to_ref_groups(&serialized).unwrap();

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].len(), 2);
        assert_eq!(restored[0][0].blake3.unwrap(), hash);
        assert_eq!(restored[0][1].path, PathBuf::from("/b"));
    }

    #[test]
    fn test_stage_labels() {
        assert_eq!(Stage::SizeGrouped.label(), "size grouping");
        assert_eq!(Stage::XxhashDone.label(), "xxHash");
        assert_eq!(Stage::Blake3Verified.label(), "BLAKE3");
    }
}
