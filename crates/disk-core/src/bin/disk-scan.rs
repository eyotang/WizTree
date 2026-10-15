//! Headless verification and local performance measurement.
use disk_core::{Progress, ScanOptions, ScanStrategy, scan_with_strategy};
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};
fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().unwrap_or_default();
    if mode == "--help" || mode.is_empty() {
        println!(
            "disk-scan --scan PATH [--csv FILE] [--workers N] [--method auto|directory|mft] [--report FILE]\ndisk-scan --benchmark PATH [--workers N] [--method auto|directory|mft] [--report FILE]\nReads file metadata only; never follows symbolic links or reparse points.\nWindows: batched directory metadata; volume roots attempt MFT.\nmacOS: getattrlistbulk, with a POSIX fallback for unsupported filesystems.\nLinux: buffered readdir and directory-fd-relative fstatat.\nExplicit --method mft also supports NTFS subfolders (requires raw-volume permission).\nWithout raw-volume permission it falls back to parallel directory walking."
        );
        return Ok(());
    }
    if mode != "--scan" && mode != "--benchmark" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected --scan or --benchmark",
        ));
    }
    let root = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing scan path"))?,
    );
    let mut csv = None;
    let mut report = None;
    let mut options = ScanOptions::default();
    let mut strategy = ScanStrategy::Auto;
    while let Some(argument) = arguments.next() {
        if argument == "--csv" {
            csv = Some(PathBuf::from(arguments.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Missing CSV path")
            })?));
        } else if argument == "--report" {
            report = Some(PathBuf::from(arguments.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Missing report path")
            })?));
        } else if argument == "--method" {
            strategy = match arguments.next().as_deref().and_then(|s| s.to_str()) {
                Some("auto") => ScanStrategy::Auto,
                Some("directory") => ScanStrategy::Directory,
                Some("mft") => ScanStrategy::NtfsMft,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Method must be auto, directory or mft",
                    ));
                }
            };
        } else if argument == "--workers" {
            options.workers = arguments
                .next()
                .and_then(|v| v.to_string_lossy().parse().ok())
                .filter(|v| *v > 0 && *v <= 64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Workers must be in 1..=64")
                })?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unknown argument: {}", argument.to_string_lossy()),
            ));
        }
    }
    let iterations = if mode == "--benchmark" { 3 } else { 1 };
    println!(
        "platform={}/{} backend={} workers={} root={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        disk_core::directory_backend(),
        options.workers,
        root.display()
    );
    let mut report = report.map(File::create).transpose()?;
    for iteration in 0..iterations {
        let progress = Progress::default();
        let snapshot = scan_with_strategy(
            &root,
            &options,
            &AtomicBool::new(false),
            &progress,
            strategy,
        )?;
        if let Some(reason) = &snapshot.fallback_reason {
            eprintln!("NTFS fallback: {reason}");
            if let Some(output) = &mut report {
                writeln!(output, "fallback={reason}")?;
            }
        }
        let seconds = snapshot.elapsed.as_secs_f64();
        let line = format!(
            "run={} method={} files={} dirs={} errors={} logical={} allocated={} elapsed_ms={:.3} entries_per_second={:.0}",
            iteration + 1,
            snapshot.method,
            snapshot.files,
            snapshot.dirs,
            snapshot.errors,
            snapshot.nodes[0].size,
            snapshot.nodes[0].allocated,
            seconds * 1000.0,
            (snapshot.files + snapshot.dirs) as f64 / seconds.max(0.000001)
        );
        let stats = progress.snapshot();
        let line = format!(
            "{line} mft_bytes={} mft_extents={} mft_cached={}",
            stats.mft_bytes, stats.mft_extents, stats.mft_cached
        );
        let io = stats.directory_io;
        let line = format!(
            "{line} directory_opens={} absolute_opens={} query_calls={} open_worker_ms={:.3} query_worker_ms={:.3} special_files={} special_worker_ms={:.3} portable_fallbacks={}",
            io.opens,
            io.absolute_opens,
            io.queries,
            io.open_ns as f64 / 1e6,
            io.query_ns as f64 / 1e6,
            io.special_files,
            io.special_ns as f64 / 1e6,
            io.portable_fallbacks
        );
        let line = format!(
            "{line} canonicalize_ms={:.3} setup_ms={:.3} pool_ms={:.3} traversal_ms={:.3} aggregate_ms={:.3}",
            io.canonicalize_ns as f64 / 1e6,
            io.setup_ns as f64 / 1e6,
            io.pool_ns as f64 / 1e6,
            io.traversal_ns as f64 / 1e6,
            io.aggregate_ns as f64 / 1e6
        );
        println!("{line}");
        if let Some(output) = &mut report {
            writeln!(output, "{line}")?;
            output.flush()?;
        }
        for issue in &snapshot.issues {
            eprintln!("{}: {}", issue.path.display(), issue.message);
        }
        if iteration + 1 == iterations
            && let Some(path) = &csv
        {
            snapshot.write_csv(BufWriter::new(File::create(path)?))?;
            println!("csv={}", path.display());
        }
    }
    Ok(())
}
