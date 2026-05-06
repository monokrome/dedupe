use clap::Parser;
use std::path::PathBuf;

const DEFAULT_STATE_FILE: &str = ".dedupe-state.json";

#[derive(Parser, Debug)]
#[command(name = "dedupe")]
#[command(about = "Find and deduplicate files using hard links or deletion")]
pub struct Args {
    /// Paths to keep and deduplicate via hard links (if --delete has paths after it)
    /// OR all paths to deduplicate with priority-based deletion (if --delete comes first)
    #[arg(value_name = "KEEP_PATHS")]
    pub keep_paths: Vec<PathBuf>,

    /// Paths to delete duplicates from (when preceded by keep paths)
    /// OR flag indicating all paths use priority-based deletion
    #[arg(long, num_args = 1..)]
    pub delete: Vec<PathBuf>,

    /// Show what would be done without making changes
    #[arg(long)]
    pub dry_run: bool,

    /// Minimum file size to consider (e.g., 1M, 1K, 1G)
    #[arg(long)]
    pub min_size: Option<String>,

    /// Maximum file size to consider
    #[arg(long)]
    pub max_size: Option<String>,

    /// Only include files matching this pattern
    #[arg(long)]
    pub pattern: Option<String>,

    /// Exclude files matching this pattern
    #[arg(long)]
    pub exclude: Option<String>,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Quiet mode (no progress bars)
    #[arg(short, long)]
    pub quiet: bool,

    /// Number of threads for parallel hashing (0 = auto-detect)
    #[arg(long, default_value = "0")]
    pub threads: usize,

    /// Show progress bars
    #[arg(long)]
    pub progress: bool,

    /// Resume from state file if it exists
    #[arg(short, long)]
    pub resume: bool,

    /// Path to state file
    #[arg(short, long, default_value = DEFAULT_STATE_FILE)]
    pub state_file: PathBuf,

    /// Temporarily clear the immutable attribute on files during link/delete,
    /// then restore it afterward. Requires CAP_LINUX_IMMUTABLE (root).
    /// Place this flag before any path arguments — `--delete` consumes
    /// everything after it as paths.
    #[arg(long)]
    pub unlock_immutable: bool,
}

impl Args {
    pub fn parse_size(size_str: &str) -> anyhow::Result<u64> {
        let size_str = size_str.trim().to_uppercase();
        let (num_str, multiplier) = if size_str.ends_with('K') {
            (&size_str[..size_str.len() - 1], 1024u64)
        } else if size_str.ends_with('M') {
            (&size_str[..size_str.len() - 1], 1024 * 1024)
        } else if size_str.ends_with('G') {
            (&size_str[..size_str.len() - 1], 1024 * 1024 * 1024)
        } else {
            (size_str.as_str(), 1)
        };

        let num: u64 = num_str.parse()?;
        Ok(num * multiplier)
    }

    pub fn is_delete_only_mode(&self) -> bool {
        self.keep_paths.is_empty() && !self.delete.is_empty()
    }
}
