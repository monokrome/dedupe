use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct Reporter {
    quiet: bool,
    verbose: bool,
    pub files_scanned: usize,
    pub duplicates_found: usize,
    pub space_saved: u64,
    pub skipped_cross_filesystem: usize,
    pub skipped_already_linked: usize,
    multi_progress: Option<MultiProgress>,
}

impl Reporter {
    pub fn new(quiet: bool, verbose: bool, progress: bool) -> Self {
        let multi_progress = if progress && !quiet {
            Some(MultiProgress::new())
        } else {
            None
        };

        Self {
            quiet,
            verbose,
            files_scanned: 0,
            duplicates_found: 0,
            space_saved: 0,
            skipped_cross_filesystem: 0,
            skipped_already_linked: 0,
            multi_progress,
        }
    }

    pub fn create_progress_bar(&self, total: u64, stage: &str) -> Option<ProgressBar> {
        self.multi_progress.as_ref().map(|mp| {
            let pb = mp.add(ProgressBar::new(total));
            pb.set_style(
                ProgressStyle::default_bar()
                    .template(&format!("{{spinner:.green}} [{{elapsed_precise}}] [{{bar:40.cyan/blue}}] {{pos}}/{{len}} {stage} ({{eta}})"))
                    .expect("Invalid progress bar template")
                    .progress_chars("#>-"),
            );
            pb
        })
    }

    pub fn log(&self, message: &str) {
        if !self.quiet {
            println!("{}", message);
        }
    }

    pub fn log_verbose(&self, message: &str) {
        if self.verbose {
            println!("{}", message);
        }
    }

    pub fn print_summary(&self, dry_run: bool) {
        if self.quiet {
            return;
        }

        println!("\n{}", "=".repeat(50));
        println!("Summary:");
        println!("  Files scanned: {}", self.files_scanned);
        println!("  Duplicates found: {}", self.duplicates_found);
        println!(
            "  Space {} saved: {}",
            if dry_run { "would be" } else { "" },
            format_size(self.space_saved)
        );

        if self.skipped_cross_filesystem > 0 {
            println!(
                "  Skipped (cross-filesystem): {}",
                self.skipped_cross_filesystem
            );
        }
        if self.skipped_already_linked > 0 {
            println!(
                "  Skipped (already linked): {}",
                self.skipped_already_linked
            );
        }

        println!("{}", "=".repeat(50));
    }
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 bytes");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn test_reporter_tracking() {
        let mut reporter = Reporter::new(true, false, false);
        reporter.files_scanned = 100;
        reporter.duplicates_found = 20;
        reporter.space_saved = 1024 * 1024 * 50;

        assert_eq!(reporter.files_scanned, 100);
        assert_eq!(reporter.duplicates_found, 20);
        assert_eq!(reporter.space_saved, 50 * 1024 * 1024);
    }
}
