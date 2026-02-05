# dedupe

A fast, parallel file deduplication tool for large storage systems. Uses a three-stage hash pipeline (size grouping, xxHash, BLAKE3) to efficiently find duplicates across terabytes of data.

## Installation

Download a binary from the [releases page](https://github.com/monokrome/dedupe/releases), or build from source:

```sh
cargo install --path .
```

## Usage

```sh
dedupe [OPTIONS] <PATHS>... [--delete <DELETE_PATHS>...]
```

### Modes

dedupe has two distinct modes based on how you use the `--delete` flag:

#### Hard Link Mode (default)

When you provide paths without `--delete`, duplicates are replaced with hard links to save space while keeping all files accessible:

```sh
dedupe /path/to/files
```

This finds all duplicates and hard-links them together. No data is removed.

#### Hard Link + Delete Mode

When you provide paths before `--delete`, files in the delete paths are removed if they have duplicates in the keep paths:

```sh
dedupe /keep/these /also/keep --delete /remove/duplicates/here
```

In this example:
- Files in `/keep/these` and `/also/keep` are preserved and hard-linked together
- Files in `/remove/duplicates/here` that match files in the keep paths are **deleted**

#### Delete-Only Mode

When `--delete` comes first (with no paths before it), all provided paths are scanned and duplicates are deleted based on priority order:

```sh
dedupe --delete /path1 /path2 /path3
```

Priority is determined by argument order. In the example above:
- If duplicates exist across all three paths, the copy in `/path1` is kept
- Copies in `/path2` and `/path3` are **permanently deleted**

### Options

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview what would happen without making changes |
| `--min-size <SIZE>` | Only consider files at least this large (e.g., `1M`, `100K`, `1G`) |
| `--max-size <SIZE>` | Only consider files at most this large |
| `--pattern <PATTERN>` | Only include files whose path contains this string |
| `--exclude <PATTERN>` | Exclude files whose path contains this string |
| `-v, --verbose` | Show detailed output |
| `-q, --quiet` | Suppress output |
| `--progress` | Show progress bars |
| `--threads <N>` | Number of hashing threads (0 = auto-detect) |
| `-r, --resume` | Save/restore state for interrupted runs |
| `-s, --state-file <PATH>` | State file path (default: `.dedupe-state.json`) |

### Examples

Preview duplicates without making changes:
```sh
dedupe --dry-run /data
```

Find and hard-link duplicates, skipping small files:
```sh
dedupe --min-size 1M /photos /backups
```

Delete duplicates from a backup drive, keeping originals:
```sh
dedupe /original/data --delete /backup/data
```

Resume an interrupted run on a large dataset:
```sh
dedupe --resume /nas/storage
# If interrupted, run the same command again to continue
```

Pre-compute hashes without acting (useful for large NAS):
```sh
dedupe --dry-run --resume /nas/storage
# Later, run without --dry-run to deduplicate using cached hashes
```

## How It Works

1. **Scan**: Recursively find all files in the provided paths
2. **Size grouping**: Files with unique sizes cannot be duplicates
3. **xxHash**: Fast hash to quickly eliminate false positives
4. **BLAKE3**: Cryptographic hash to confirm true duplicates
5. **Deduplicate**: Hard-link or delete based on mode

The three-stage approach minimizes I/O by reading files only when necessary. Hashing is parallelized across all available cores.

## Resume Support

For large datasets (multi-TB NAS systems), runs can take hours. Use `--resume` to save state after each pipeline stage:

```sh
dedupe --resume /large/dataset
```

If interrupted (Ctrl+C, power loss, etc.), run the same command again to continue from the last completed stage. State is automatically cleaned up after successful completion.

With `--dry-run --resume`, the state file is preserved, allowing you to pre-compute all hashes and then run without `--dry-run` to apply changes instantly.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
