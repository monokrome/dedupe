mod cli;
mod deduplicator;
mod hasher;
mod platform;
mod reporter;
mod scanner;

use anyhow::{Context, Result};
use clap::Parser;
use cli::Args;
use deduplicator::{DedupeMode, Deduplicator};
use hasher::{group_by_size, refine_by_blake3, refine_by_xxhash, FileRef};
use reporter::Reporter;
use scanner::{scan_paths, ScanOptions};
use std::io::{self, Write};
use std::sync::Arc;

fn main() -> Result<()> {
    let args = Args::parse();

    if args.keep_paths.is_empty() && args.delete.is_empty() {
        eprintln!("Error: No paths specified");
        eprintln!("Usage: dedupe [keep_paths...] [--delete delete_paths...]");
        eprintln!("   or: dedupe --delete [paths...]");
        std::process::exit(1);
    }

    let num_threads = if args.threads == 0 {
        num_cpus::get()
    } else {
        args.threads
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build_global()
        .expect("Failed to build thread pool");

    let mut reporter = Reporter::new(args.quiet, args.verbose, args.progress);

    let min_size = args.min_size
        .as_ref()
        .map(|s| Args::parse_size(s))
        .transpose()
        .context("Invalid min-size format")?;

    let max_size = args.max_size
        .as_ref()
        .map(|s| Args::parse_size(s))
        .transpose()
        .context("Invalid max-size format")?;

    let scan_options = ScanOptions {
        min_size,
        max_size,
        pattern: args.pattern.clone(),
        exclude: args.exclude.clone(),
    };

    let mode = if args.is_delete_only_mode() {
        if !args.dry_run {
            confirm_delete_mode()?;
        }
        DedupeMode::DeleteOnly {
            paths: args.delete.clone(),
        }
    } else {
        if !args.delete.is_empty() && !args.dry_run {
            confirm_mixed_mode()?;
        }
        DedupeMode::HardLinkWithDelete {
            keep_paths: args.keep_paths.clone(),
            delete_paths: args.delete.clone(),
        }
    };

    let all_paths: Vec<_> = match &mode {
        DedupeMode::HardLinkWithDelete { keep_paths, delete_paths } => {
            keep_paths.iter().chain(delete_paths.iter()).collect()
        }
        DedupeMode::DeleteOnly { paths } => paths.iter().collect(),
    };

    reporter.log("Scanning files...");
    let files = scan_paths(&all_paths, &scan_options)?;
    reporter.files_scanned = files.len();
    reporter.log(&format!("Found {} files", files.len()));

    reporter.log("Grouping by size...");
    let size_groups = group_by_size(files);
    let size_matches: Vec<_> = size_groups.into_values().collect();

    if size_matches.is_empty() {
        reporter.log("No duplicates found");
        return Ok(());
    }

    reporter.log("Computing xxHash for potential duplicates...");
    let total_xxhash: u64 = size_matches.iter().map(|g| g.len() as u64).sum();
    let xxhash_progress = reporter.create_progress_bar(total_xxhash, "xxHash");

    let mut xxhash_groups = Vec::new();
    for group in size_matches {
        let arc_group: Vec<FileRef> = group.into_iter().map(Arc::new).collect();
        let groups = refine_by_xxhash(arc_group, xxhash_progress.as_ref())?;
        xxhash_groups.extend(groups.into_values());
    }

    if let Some(pb) = xxhash_progress {
        pb.finish_and_clear();
    }

    if xxhash_groups.is_empty() {
        reporter.log("No duplicates found");
        return Ok(());
    }

    reporter.log("Computing BLAKE3 for final verification...");
    let total_blake3: u64 = xxhash_groups.iter().map(|g| g.len() as u64).sum();
    let blake3_progress = reporter.create_progress_bar(total_blake3, "BLAKE3");

    let mut blake3_groups = Vec::new();
    for group in xxhash_groups {
        let groups = refine_by_blake3(group, blake3_progress.as_ref())?;
        blake3_groups.extend(groups.into_values());
    }

    if let Some(pb) = blake3_progress {
        pb.finish_and_clear();
    }

    if blake3_groups.is_empty() {
        reporter.log("No duplicates found");
        return Ok(());
    }

    reporter.log(&format!("Found {} groups of duplicates", blake3_groups.len()));

    if args.dry_run {
        print_dry_run_summary(&blake3_groups, &mode);
    }

    let deduplicator = Deduplicator::new(mode, args.dry_run);
    deduplicator.deduplicate(blake3_groups, &mut reporter)?;

    reporter.print_summary(args.dry_run);

    Ok(())
}

fn confirm_delete_mode() -> Result<()> {
    print!("WARNING: Delete-only mode will permanently delete duplicate files. Continue? (y/N): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    if !response.trim().eq_ignore_ascii_case("y") {
        println!("Aborted.");
        std::process::exit(0);
    }

    Ok(())
}

fn confirm_mixed_mode() -> Result<()> {
    print!("WARNING: This will delete files from delete paths. Continue? (y/N): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    if !response.trim().eq_ignore_ascii_case("y") {
        println!("Aborted.");
        std::process::exit(0);
    }

    Ok(())
}

fn print_dry_run_summary(groups: &[Vec<FileRef>], mode: &DedupeMode) {
    println!("\n{}", "=".repeat(50));
    println!("DRY RUN - No changes will be made");
    println!("{}", "=".repeat(50));

    match mode {
        DedupeMode::HardLinkWithDelete { keep_paths, delete_paths } => {
            println!("Mode: Hard link (keep) + Delete");
            println!("Keep paths: {:?}", keep_paths);
            println!("Delete paths: {:?}", delete_paths);
        }
        DedupeMode::DeleteOnly { paths } => {
            println!("Mode: Delete-only (priority-based)");
            println!("Paths (in priority order): {:?}", paths);
        }
    }

    println!("\nDuplicate groups:");
    for (i, group) in groups.iter().enumerate() {
        println!("\nGroup {} ({} files, {} bytes each):", i + 1, group.len(), group[0].size);
        for file in group {
            println!("  - {}", file.path.display());
        }
    }

    println!("\n{}", "=".repeat(50));
}
