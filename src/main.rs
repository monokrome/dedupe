mod cli;
mod deduplicator;
mod hasher;
mod platform;
mod reporter;
mod scanner;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use cli::Args;
use deduplicator::{DedupeMode, Deduplicator};
use hasher::{group_by_size, refine_by_blake3, refine_by_xxhash, FileIdentity, FileRef};
use reporter::Reporter;
use scanner::{scan_paths, ScanOptions};
use state::{
    create_checkpoint, delete_state_file, files_to_serializable, groups_to_serializable,
    load_checkpoint, refs_to_serializable, save_checkpoint, serializable_to_identities,
    serializable_to_ref_groups, Stage, StageData,
};
use std::io::{self, Write};
use std::path::PathBuf;
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

    let min_size = args
        .min_size
        .as_ref()
        .map(|s| Args::parse_size(s))
        .transpose()
        .context("Invalid min-size format")?;

    let max_size = args
        .max_size
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
            confirm_action("Delete-only mode will permanently delete duplicate files.")?;
        }
        DedupeMode::DeleteOnly {
            paths: args.delete.clone(),
        }
    } else {
        if !args.delete.is_empty() && !args.dry_run {
            confirm_action("This will delete files from delete paths.")?;
        }
        DedupeMode::HardLinkWithDelete {
            keep_paths: args.keep_paths.clone(),
            delete_paths: args.delete.clone(),
        }
    };

    let all_paths: Vec<PathBuf> = match &mode {
        DedupeMode::HardLinkWithDelete {
            keep_paths,
            delete_paths,
        } => keep_paths
            .iter()
            .chain(delete_paths.iter())
            .cloned()
            .collect(),
        DedupeMode::DeleteOnly { paths } => paths.clone(),
    };

    let resumed_stage = try_resume(&args, &all_paths, &scan_options, &mut reporter)?;

    match resumed_stage {
        Some((Stage::Blake3Verified, StageData::Blake3(groups))) => {
            let blake3_groups = serializable_to_ref_groups(&groups)?;
            run_deduplication(blake3_groups, &mode, &args, &mut reporter)?;
        }
        Some((Stage::XxhashDone, StageData::Xxhash(files))) => {
            let identities = serializable_to_identities(&files)?;
            let file_refs: Vec<FileRef> = identities.into_iter().map(Arc::new).collect();
            let blake3_groups =
                run_blake3_stage(file_refs, &args, &all_paths, &scan_options, &mut reporter)?;
            run_deduplication(blake3_groups, &mode, &args, &mut reporter)?;
        }
        Some((Stage::SizeGrouped, StageData::SizeGroup(files))) => {
            let identities = serializable_to_identities(&files)?;
            let xxhash_files =
                run_xxhash_stage(identities, &args, &all_paths, &scan_options, &mut reporter)?;
            let blake3_groups = run_blake3_stage(
                xxhash_files,
                &args,
                &all_paths,
                &scan_options,
                &mut reporter,
            )?;
            run_deduplication(blake3_groups, &mode, &args, &mut reporter)?;
        }
        _ => {
            run_full_pipeline(&args, &all_paths, &scan_options, &mode, &mut reporter)?;
        }
    }

    if args.resume && !args.dry_run {
        delete_state_file(&args.state_file)?;
    }

    reporter.print_summary(args.dry_run);

    Ok(())
}

fn try_resume(
    args: &Args,
    all_paths: &[PathBuf],
    scan_options: &ScanOptions,
    reporter: &mut Reporter,
) -> Result<Option<(Stage, StageData)>> {
    if !args.resume {
        return Ok(None);
    }

    let checkpoint = match load_checkpoint(&args.state_file)? {
        Some(c) => c,
        None => {
            reporter.log("No existing state file found, starting fresh");
            return Ok(None);
        }
    };

    if !state::validate_checkpoint(&checkpoint, all_paths, scan_options)? {
        reporter.log("State file validation failed, starting fresh");
        return Ok(None);
    }

    reporter.log(&format!("Resuming from {} stage", checkpoint.stage.label()));

    Ok(Some((checkpoint.stage, checkpoint.data)))
}

fn run_full_pipeline(
    args: &Args,
    all_paths: &[PathBuf],
    scan_options: &ScanOptions,
    mode: &DedupeMode,
    reporter: &mut Reporter,
) -> Result<()> {
    reporter.log("Scanning files...");
    let files = scan_paths(all_paths, scan_options)?;
    reporter.files_scanned = files.len();
    reporter.log(&format!("Found {} files", files.len()));

    reporter.log("Grouping by size...");
    let size_groups = group_by_size(files);
    let size_matched: Vec<FileIdentity> = size_groups.into_values().flatten().collect();

    if size_matched.is_empty() {
        reporter.log("No duplicates found");
        return Ok(());
    }

    if args.resume {
        let checkpoint = create_checkpoint(
            Stage::SizeGrouped,
            all_paths,
            scan_options,
            StageData::SizeGroup(files_to_serializable(&size_matched)),
        );
        save_checkpoint(&args.state_file, &checkpoint)?;
        reporter.log("Saved checkpoint: size grouping complete");
    }

    let xxhash_files = run_xxhash_stage(size_matched, args, all_paths, scan_options, reporter)?;

    let blake3_groups = run_blake3_stage(xxhash_files, args, all_paths, scan_options, reporter)?;

    run_deduplication(blake3_groups, mode, args, reporter)?;

    Ok(())
}

fn run_xxhash_stage(
    size_matched: Vec<FileIdentity>,
    args: &Args,
    all_paths: &[PathBuf],
    scan_options: &ScanOptions,
    reporter: &mut Reporter,
) -> Result<Vec<FileRef>> {
    reporter.log("Computing xxHash for potential duplicates...");
    let total_xxhash: u64 = size_matched.len() as u64;
    let xxhash_progress = reporter.create_progress_bar(total_xxhash, "xxHash");

    let arc_files: Vec<FileRef> = size_matched.into_iter().map(Arc::new).collect();
    let xxhash_result = refine_by_xxhash(arc_files, xxhash_progress.as_ref())?;

    if let Some(pb) = xxhash_progress {
        pb.finish_and_clear();
    }

    let xxhash_files: Vec<FileRef> = xxhash_result.into_values().flatten().collect();

    if xxhash_files.is_empty() {
        reporter.log("No duplicates found after xxHash");
        return Ok(vec![]);
    }

    if args.resume {
        let checkpoint = create_checkpoint(
            Stage::XxhashDone,
            all_paths,
            scan_options,
            StageData::Xxhash(refs_to_serializable(&xxhash_files)),
        );
        save_checkpoint(&args.state_file, &checkpoint)?;
        reporter.log("Saved checkpoint: xxHash complete");
    }

    Ok(xxhash_files)
}

fn run_blake3_stage(
    xxhash_files: Vec<FileRef>,
    args: &Args,
    all_paths: &[PathBuf],
    scan_options: &ScanOptions,
    reporter: &mut Reporter,
) -> Result<Vec<Vec<FileRef>>> {
    if xxhash_files.is_empty() {
        return Ok(vec![]);
    }

    reporter.log("Computing BLAKE3 for final verification...");
    let total_blake3: u64 = xxhash_files.len() as u64;
    let blake3_progress = reporter.create_progress_bar(total_blake3, "BLAKE3");

    let blake3_result = refine_by_blake3(xxhash_files, blake3_progress.as_ref())?;

    if let Some(pb) = blake3_progress {
        pb.finish_and_clear();
    }

    let blake3_groups: Vec<Vec<FileRef>> = blake3_result.into_values().collect();

    if blake3_groups.is_empty() {
        reporter.log("No duplicates found after BLAKE3");
        return Ok(vec![]);
    }

    reporter.log(&format!(
        "Found {} groups of duplicates",
        blake3_groups.len()
    ));

    if args.resume {
        let checkpoint = create_checkpoint(
            Stage::Blake3Verified,
            all_paths,
            scan_options,
            StageData::Blake3(groups_to_serializable(&blake3_groups)),
        );
        save_checkpoint(&args.state_file, &checkpoint)?;
        reporter.log("Saved checkpoint: BLAKE3 complete");
    }

    Ok(blake3_groups)
}

fn run_deduplication(
    blake3_groups: Vec<Vec<FileRef>>,
    mode: &DedupeMode,
    args: &Args,
    reporter: &mut Reporter,
) -> Result<()> {
    if blake3_groups.is_empty() {
        reporter.log("No duplicates found");
        return Ok(());
    }

    if args.dry_run {
        print_dry_run_summary(&blake3_groups, mode);
    }

    let deduplicator = Deduplicator::new_ref(mode, args.dry_run);
    deduplicator.deduplicate(blake3_groups, reporter)?;

    Ok(())
}

fn confirm_action(warning: &str) -> Result<()> {
    print!("WARNING: {warning} Continue? (y/N): ");
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
        DedupeMode::HardLinkWithDelete {
            keep_paths,
            delete_paths,
        } => {
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
        println!(
            "\nGroup {} ({} files, {} bytes each):",
            i + 1,
            group.len(),
            group[0].size
        );
        for file in group {
            println!("  - {}", file.path.display());
        }
    }

    println!("\n{}", "=".repeat(50));
}
