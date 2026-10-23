//! Batch orchestration. Read-only modes never create cache, output, or report files.
use crate::{
    config::{Config, SettingsPatch},
    imaging::{self, CandidateSummary},
    scanner::{self, AndroidResource, ReferenceHit, Rename, ScanOptions},
    writer::{self, SourceSnapshot},
};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Debug, Clone, Copy)]
pub enum ProgressEvent<'a> {
    Phase {
        file_index: usize,
        source: &'a Path,
        progress: imaging::ImageProgress,
    },
    Complete {
        file_index: usize,
        report: &'a FileReport,
    },
}

const PRESET_NOTICE: &str = "Preset scores (high=88, balanced=80, small=70) are experimental, not visually calibrated. Review important UI/text assets before replacement.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Write,
    DryRun,
    Analyze,
    Check,
}
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub inputs: Vec<PathBuf>,
    pub config: Config,
    pub cli_settings: SettingsPatch,
    pub output: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub mode: Mode,
    pub jobs: usize,
    pub use_cache: bool,
}
#[derive(Debug, Serialize)]
pub struct FileReport {
    #[serde(serialize_with = "crate::paths::serialize_path")]
    pub source: PathBuf,
    #[serde(serialize_with = "crate::paths::serialize_optional_path")]
    pub output: Option<PathBuf>,
    pub status: String,
    pub cache_hit: bool,
    pub reason: Option<String>,
    pub original_bytes: u64,
    pub optimized_bytes: Option<u64>,
    pub saving_bytes: u64,
    pub score: Option<f64>,
    pub score_basis: Option<String>,
    pub encoder: Option<String>,
    pub format: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub has_alpha: Option<bool>,
    pub class: Option<String>,
    pub android: Option<AndroidResource>,
    pub candidates: Vec<CandidateSummary>,
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub version: String,
    pub mode: String,
    pub presets_calibrated: bool,
    pub cache_enabled: bool,
    pub files: Vec<FileReport>,
    pub summary: Summary,
    pub renames: Vec<Rename>,
    pub reference_warnings: Vec<ReferenceHit>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub scanned: usize,
    pub optimized: usize,
    pub skipped: usize,
    pub cache_hits: usize,
    pub errors: usize,
    pub original_bytes: u64,
    pub optimized_bytes: u64,
    pub saving_bytes: u64,
    pub elapsed_ms: u64,
}

/// Format decimal byte units with one fractional digit, capped at GB.
/// Round before selecting the unit so boundaries never display `1000.0 KB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u128, &str); 4] = [
        (1, "B"),
        (1_000, "KB"),
        (1_000_000, "MB"),
        (1_000_000_000, "GB"),
    ];
    for (divisor, unit) in UNITS {
        let tenths = (u128::from(bytes) * 10 + divisor / 2) / divisor;
        if tenths < 10_000 || unit == "GB" {
            return format!("{}.{:01} {unit}", tenths / 10, tenths % 10);
        }
    }
    unreachable!("GB is the final, uncapped unit")
}

/// Format the saved percentage to one decimal using unrounded byte counts.
pub fn format_saved_percentage(original: u64, saved: u64) -> String {
    let percent_tenths = if original == 0 {
        0
    } else {
        (u128::from(saved) * 1_000 + u128::from(original) / 2) / u128::from(original)
    };
    format!("{}.{}%", percent_tenths / 10, percent_tenths % 10)
}

/// Show original/result sizes and savings, calculated from unrounded bytes.
pub fn format_savings(original: u64, result: u64, saved: u64) -> String {
    format!(
        "{} -> {}, saved {} ({})",
        format_size(original),
        format_size(result),
        format_saved_percentage(original, saved),
        format_size(saved)
    )
}

impl Report {
    pub fn text(&self, verbose: bool) -> String {
        let mut text = String::new();
        for file in &self.files {
            text.push_str(&file.text(verbose, self.mode == "analyze"));
        }
        text.push_str(&self.summary_text(verbose));
        text
    }

    /// Summary for an interactive UI which already printed every file result.
    pub fn summary_text(&self, verbose: bool) -> String {
        let mut text = String::new();

        text.push_str(&format!(
            "\n{} files; {} optimized, {} skipped, {} errors; {}; {} ms\n",
            self.summary.scanned,
            self.summary.optimized,
            self.summary.skipped,
            self.summary.errors,
            format_savings(
                self.summary.original_bytes,
                self.summary.optimized_bytes,
                self.summary.saving_bytes,
            ),
            self.summary.elapsed_ms
        ));
        for r in &self.reference_warnings {
            text.push_str(&format!(
                "REFERENCE {}:{}: {}\n",
                r.file.display(),
                r.line,
                r.literal
            ));
        }
        for warning in &self.warnings {
            if !verbose && warning == PRESET_NOTICE {
                continue;
            }
            text.push_str(&format!("warning: {warning}\n"));
        }
        text
    }
}
impl FileReport {
    pub fn text(&self, verbose: bool, analyze: bool) -> String {
        let file = self;
        let mut text = String::new();
        if file.status == "skipped" {
            text.push_str(&format!(
                "SKIP {}: {}\n",
                file.source.display(),
                file.reason.as_deref().unwrap_or("no improvement")
            ));
        } else if file.status == "error" {
            text.push_str(&format!(
                "ERROR {}: {}\n",
                file.source.display(),
                file.reason.as_deref().unwrap_or("unknown error")
            ));
        } else {
            text.push_str(&format!(
                "{} {} -> {}  {}, score {}\n",
                file.status.to_uppercase(),
                file.source.display(),
                file.output.as_deref().unwrap_or(&file.source).display(),
                format_savings(
                    file.original_bytes,
                    file.optimized_bytes.unwrap_or(file.original_bytes),
                    file.saving_bytes,
                ),
                file.score
                    .map(|n| format!(
                        "{n:.2}{}",
                        if file.score_basis.as_deref() == Some("exact decoded-pixel identity") {
                            " (pixel-identical)"
                        } else {
                            ""
                        }
                    ))
                    .unwrap_or_else(|| "lossless (pixel verified)".into())
            ));
        }
        if verbose || analyze {
            for c in &file.candidates {
                text.push_str(&format!(
                    "  {} {:?}: {}, score {}, accepted={}{}\n",
                    c.encoder,
                    c.quality,
                    format_size(c.size as u64),
                    c.score
                        .map(|n| format!("{n:.2}"))
                        .unwrap_or_else(|| "lossless/N/A".into()),
                    c.accepted,
                    c.note
                        .as_ref()
                        .map(|s| format!(", {s}"))
                        .unwrap_or_default()
                ));
            }
        }
        for warning in &file.warnings {
            text.push_str(&format!("  warning: {warning}\n"));
        }
        text
    }
}
fn base_report(item: &scanner::ScanItem) -> FileReport {
    FileReport {
        source: item.source.clone(),
        output: None,
        status: "skipped".into(),
        cache_hit: false,
        reason: None,
        original_bytes: std::fs::symlink_metadata(&item.source)
            .map(|m| m.len())
            .unwrap_or(0),
        optimized_bytes: None,
        saving_bytes: 0,
        score: None,
        score_basis: None,
        encoder: None,
        format: None,
        width: None,
        height: None,
        has_alpha: None,
        class: None,
        android: item.android.clone(),
        candidates: vec![],
        warnings: vec![],
        elapsed_ms: 0,
    }
}

/// Read-only preflight with a fixed file list. Executing consumes this snapshot,
/// so files discovered after an interactive confirmation are never added.
#[derive(Debug)]
pub struct PreparedRun {
    options: RunOptions,
    scan: scanner::ScanResult,
    selected_roots: Vec<PathBuf>,
    reference_roots: Vec<PathBuf>,
    excluded_dirs: Vec<PathBuf>,
    has_directory_inputs: bool,
    eligible_images: usize,
}
impl PreparedRun {
    /// Supported, unprotected images selected by the filters and minimum size.
    /// Decoding, cache checks, and candidate evaluation may still skip these.
    pub fn eligible_images(&self) -> usize {
        self.eligible_images
    }

    /// Known skips retained in the scan; directory entries omitted by filters
    /// are not included in this count.
    pub fn known_skipped(&self) -> usize {
        self.scan.items.len() - self.eligible_images
    }

    pub fn requires_confirmation(&self) -> bool {
        self.options.mode == Mode::Write
            && self.options.output.is_none()
            && self.has_directory_inputs
            && self.eligible_images > 0
    }

    pub fn run(self) -> Result<Report> {
        self.run_with_progress(|_| {})
    }

    /// Report image phases and one completion per scan item (including skips/errors).
    /// Called concurrently by image workers; callbacks should be brief and must not panic.
    pub fn run_with_progress(
        self,
        on_progress: impl Fn(ProgressEvent<'_>) + Sync,
    ) -> Result<Report> {
        run_prepared(self, &on_progress)
    }
}

pub fn run(options: &RunOptions) -> Result<Report> {
    prepare(options)?.run()
}

/// Validate and select inputs without creating files, caches, or directories.
pub fn prepare(options: &RunOptions) -> Result<PreparedRun> {
    let mut normalized = options.clone();
    normalized.output = options
        .output
        .as_deref()
        .map(crate::paths::resolve_selected_root)
        .transpose()?;
    normalized.backup = options
        .backup
        .as_deref()
        .map(crate::paths::resolve_selected_root)
        .transpose()?;
    let options = &normalized;
    if options.inputs.is_empty() {
        bail!("at least one input path is required");
    }
    if options.jobs == 0 || options.jobs > 64 {
        bail!("jobs must be between 1 and 64");
    }
    if options.output.is_some() && options.backup.is_some() {
        bail!("--backup is only valid for in-place processing, not --output");
    }
    options.config.settings.validate()?;
    options.cli_settings.validate()?;
    let mut global = options.config.settings.clone();
    global.overlay(&options.cli_settings);
    let excluded_dirs: Vec<_> = options
        .output
        .iter()
        .chain(options.backup.iter())
        .cloned()
        .collect();
    for destination in &excluded_dirs {
        let destination = absolute_normalized(destination)?;
        for input in &options.inputs {
            let input = std::fs::canonicalize(input)
                .with_context(|| format!("cannot resolve input {}", input.display()))?;
            if input.starts_with(&destination) {
                bail!(
                    "output/backup directory {} must not equal or contain input {}",
                    destination.display(),
                    input.display()
                );
            }
        }
    }
    let reference_roots = scanner::reference_roots(&options.inputs);
    let mut has_directory_inputs = false;
    let selected_roots = options
        .inputs
        .iter()
        .map(|input| {
            let resolved = std::fs::canonicalize(input)?;
            Ok(if resolved.is_dir() {
                has_directory_inputs = true;
                resolved
            } else {
                resolved
                    .parent()
                    .context("input file has no parent")?
                    .to_path_buf()
            })
        })
        .collect::<Result<Vec<PathBuf>>>()?;
    let mut scan = scanner::scan(
        &options.inputs,
        &ScanOptions {
            include: global.include.unwrap_or_default(),
            exclude: global.exclude.unwrap_or_default(),
            excluded_dirs: excluded_dirs.clone(),
        },
    )?;
    // Validate every effective configuration before the first possible write.
    for item in &mut scan.items {
        let settings = options
            .config
            .effective(&item.relative_path, &options.cli_settings)?;
        settings.optimize_options(item.is_asset)?;
        let min_bytes = settings.min_bytes.unwrap_or(2048);
        // Freeze minimum-size exclusions too: a file growing while the user is
        // deciding must not become an unconfirmed write when the count was zero.
        if item.skip_reason.is_none()
            && std::fs::symlink_metadata(&item.source)
                .is_ok_and(|metadata| metadata.len() < min_bytes)
        {
            item.skip_reason = Some(format!("smaller than min_bytes ({min_bytes})"));
        }
    }
    let eligible_images = scan
        .items
        .iter()
        .filter(|item| item.skip_reason.is_none())
        .count();
    Ok(PreparedRun {
        options: normalized,
        scan,
        selected_roots,
        reference_roots,
        excluded_dirs,
        has_directory_inputs,
        eligible_images,
    })
}

fn run_prepared(
    prepared: PreparedRun,
    on_progress: &(impl Fn(ProgressEvent<'_>) + Sync),
) -> Result<Report> {
    let start = Instant::now();
    let PreparedRun {
        options,
        scan,
        selected_roots,
        reference_roots,
        excluded_dirs,
        ..
    } = prepared;
    let options = &options;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs)
        .build()
        .context("create image worker pool")?;
    let files = pool.install(|| {
        scan.items.par_iter().enumerate().map(|(file_index, item)| {
            let started = Instant::now();
            let mut report = base_report(item);
            if let Some(reason) = &item.skip_reason {
                report.reason = Some(reason.clone());
                return (file_index, report);
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process(item, options, &selected_roots, &mut report, &|progress| {
                    on_progress(ProgressEvent::Phase { file_index, source: &item.source, progress });
                })
            }));
            match outcome {
                Ok(Ok(())) => {},
                Ok(Err(error)) => {
                    report.status = "error".into();
                    report.reason = Some(format!("{error:#}"));
                    report.saving_bytes = 0;
                },
                Err(_) => {
                    report.status = "error".into();
                    report.reason = Some("encoder panicked; inspect the file and any recovery message before retrying".into());
                    report.saving_bytes = 0;
                },
            }
            report.elapsed_ms = started.elapsed().as_millis() as u64;
            (file_index, report)
        }).map(|(file_index, report)| {
            on_progress(ProgressEvent::Complete { file_index, report: &report });
            report
        }).collect::<Vec<_>>()
    });
    let renames: Vec<_> = files
        .iter()
        .filter(|f| f.status == "written" || f.status == "planned")
        .filter_map(|f| {
            f.output
                .as_ref()
                .filter(|out| out.extension() != f.source.extension())
                .map(|out| Rename {
                    old: f.source.clone(),
                    new: out.clone(),
                })
        })
        .collect();
    let mut warnings = vec![PRESET_NOTICE.into()];
    let reference_warnings =
        match scanner::find_rename_references(&reference_roots, &renames, &excluded_dirs) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("Reference scan incomplete: {e:#}"));
                vec![]
            }
        };
    let mut summary = Summary {
        scanned: files.len(),
        elapsed_ms: start.elapsed().as_millis() as u64,
        ..Default::default()
    };
    for f in &files {
        summary.original_bytes += f.original_bytes;
        summary.cache_hits += usize::from(f.cache_hit);
        summary.saving_bytes += f.saving_bytes;
        summary.optimized_bytes += f.original_bytes - f.saving_bytes;
        match f.status.as_str() {
            "written" | "planned" => summary.optimized += 1,
            "error" => summary.errors += 1,
            _ => summary.skipped += 1,
        }
    }
    Ok(Report {
        version: env!("CARGO_PKG_VERSION").into(),
        mode: match options.mode {
            Mode::Write => "write",
            Mode::DryRun => "dry-run",
            Mode::Analyze => "analyze",
            Mode::Check => "check",
        }
        .into(),
        presets_calibrated: false,
        cache_enabled: options.use_cache && options.mode != Mode::Analyze,
        files,
        summary,
        renames,
        reference_warnings,
        warnings,
    })
}
fn process(
    item: &scanner::ScanItem,
    options: &RunOptions,
    selected_roots: &[PathBuf],
    report: &mut FileReport,
    on_progress: &dyn Fn(imaging::ImageProgress),
) -> Result<()> {
    on_progress(imaging::ImageProgress {
        phase: "reading",
        attempt: 0,
    });
    let snapshot = SourceSnapshot::capture(&item.source)?;
    report.original_bytes = snapshot.bytes().len() as u64;
    let settings = options
        .config
        .effective(&item.relative_path, &options.cli_settings)?;
    let min_bytes = settings.min_bytes.unwrap_or(2048);
    let image_options = settings.optimize_options(item.is_asset)?;
    let selected_root = selected_roots
        .iter()
        .filter(|root| item.source.starts_with(root))
        .max_by_key(|root| root.components().count());
    if options.use_cache && options.mode != Mode::Analyze {
        let key = crate::cache::OutputKey::new(
            snapshot.hash(),
            report.original_bytes,
            &image_options,
            min_bytes,
        )?;
        for root in crate::cache::lookup_roots(&item.source, selected_root.map(PathBuf::as_path)) {
            let cached = crate::cache::ProcessedCache::new(&root, crate::cache::Access::ReadOnly)
                .and_then(|cache| cache.contains(&key));
            match cached {
                Ok(true) => {
                    report.cache_hit = true;
                    report.reason=Some("already processed: exact output bytes and effective settings match a local marker (not a new quality measurement)".into());
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => report
                    .warnings
                    .push(format!("cache lookup ignored: {error:#}")),
            }
        }
    }
    if report.original_bytes < min_bytes {
        report.reason = Some(format!("smaller than min_bytes ({min_bytes})"));
        return Ok(());
    }
    if item.is_asset {
        report.warnings.push(
            "assets path: format conversion disabled to protect filename-based access".into(),
        );
    }
    let optimized = match imaging::optimize_bytes_with_progress(
        snapshot.bytes(),
        &image_options,
        on_progress,
    ) {
        Ok(x) => x,
        Err(e) => {
            report.reason = Some(format!("image skipped: {e}"));
            return Ok(());
        }
    };
    if let Some(error) = optimized.runtime_error {
        bail!("{error}");
    }
    report.width = Some(optimized.width);
    report.height = Some(optimized.height);
    report.has_alpha = Some(optimized.has_alpha);
    report.class = Some(optimized.class);
    report.candidates = optimized.candidates;
    report.warnings.extend(optimized.warnings);
    let Some(candidate) = optimized.selected else {
        report.reason = optimized.skip_reason.or_else(|| {
            Some("no smaller verified candidate meeting the quality/format rules".into())
        });
        return Ok(());
    };
    let mut target = if let Some(output) = &options.output {
        output.join(&item.relative_path)
    } else {
        item.source.clone()
    };
    if candidate.summary.format != optimized.source_format {
        target.set_extension(candidate.summary.format.extension());
    }
    let backup = options
        .backup
        .as_ref()
        .map(|root| root.join(&item.relative_path));
    report.optimized_bytes = Some(candidate.bytes.len() as u64);
    report.saving_bytes = report.original_bytes - candidate.bytes.len() as u64;
    report.score = candidate.summary.score;
    report.score_basis = Some(
        if candidate.summary.lossless {
            "exact decoded-pixel identity"
        } else {
            "measured SSIMULACRA2"
        }
        .into(),
    );
    report.encoder = Some(candidate.summary.encoder.clone());
    report.format = Some(candidate.summary.format.as_str().into());
    report.output = Some(target.clone());
    // A pre-existing target must not be counted as actionable in check or dry-run.
    if target != item.source && target.exists() {
        report.status = "skipped".into();
        report.reason = Some(format!("target already exists: {}", target.display()));
        report.saving_bytes = 0;
        return Ok(());
    }
    writer::preflight_target(
        &item.source,
        &target,
        backup.as_deref(),
        options.output.is_some(),
    )?;
    if options.mode == Mode::Write {
        on_progress(imaging::ImageProgress {
            phase: "saving",
            attempt: report.candidates.len(),
        });
        writer::write_candidate(
            &item.source,
            &target,
            snapshot.hash(),
            &candidate.bytes,
            backup.as_deref(),
            options.output.is_some(),
        )?;
        report.status = "written".into();
        if options.use_cache {
            let root = options.output.as_ref().or(selected_root);
            if let Some(root) = root {
                let recorded = (|| -> Result<()> {
                    let key = crate::cache::OutputKey::new(
                        blake3::hash(&candidate.bytes),
                        candidate.bytes.len() as u64,
                        &image_options,
                        min_bytes,
                    )?;
                    crate::cache::ProcessedCache::new(root, crate::cache::Access::ReadWrite)?
                        .record(&key)
                })();
                if let Err(error) = recorded {
                    report.warnings.push(format!(
                        "image was written, but processed marker could not be saved: {error:#}"
                    ));
                }
            }
        }
    } else {
        report.status = "planned".into();
    }
    Ok(())
}
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(4)
}
pub fn git_warning(inputs: &[PathBuf]) -> Option<String> {
    let first = inputs.first()?;
    let base = if first.is_file() {
        first.parent().unwrap_or(Path::new("."))
    } else {
        first
    };
    let root = std::process::Command::new("git")
        .args(["-C"])
        .arg(base)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok();
    if !root.as_ref().is_some_and(|o| o.status.success()) {
        return Some("In-place processing changes/removes originals and this path is not a verified Git worktree. Use --dry-run, --backup, or -o first.".into());
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok();
    if !status
        .as_ref()
        .is_some_and(|o| o.status.success() && o.stdout.is_empty())
    {
        Some("Git worktree contains uncommitted files (or status could not be verified). In-place processing changes/removes originals; consider --backup or -o.".into())
    } else {
        None
    }
}

fn absolute_normalized(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_decimal_units_and_one_fractional_digit() {
        for (bytes, expected) in [
            (0, "0.0 B"),
            (1, "1.0 B"),
            (999, "999.0 B"),
            (1_000, "1.0 KB"),
            (1_049, "1.0 KB"),
            (1_050, "1.1 KB"),
            (820_400, "820.4 KB"),
            (999_949, "999.9 KB"),
            (999_950, "1.0 MB"),
            (1_000_000, "1.0 MB"),
            (1_500_000, "1.5 MB"),
            (999_949_999, "999.9 MB"),
            (999_950_000, "1.0 GB"),
            (1_000_000_000, "1.0 GB"),
            (1_000_000_000_000, "1000.0 GB"),
            (u64::MAX, "18446744073.7 GB"),
        ] {
            assert_eq!(format_size(bytes), expected, "bytes={bytes}");
        }
    }

    #[test]
    fn savings_use_exact_byte_counts_before_rounding() {
        assert_eq!(
            format_savings(1_500_000, 820_400, 679_600),
            "1.5 MB -> 820.4 KB, saved 45.3% (679.6 KB)"
        );
        // The percentage uses 1,049 bytes, not the displayed 1.0 KB.
        assert_eq!(
            format_savings(1_049, 999, 50),
            "1.0 KB -> 999.0 B, saved 4.8% (50.0 B)"
        );
        assert_eq!(
            format_savings(2_000, 1_999, 1),
            "2.0 KB -> 2.0 KB, saved 0.1% (1.0 B)"
        );
        assert_eq!(
            format_savings(u64::MAX, 0, u64::MAX),
            "18446744073.7 GB -> 0.0 B, saved 100.0% (18446744073.7 GB)"
        );
    }

    #[test]
    fn savings_handle_empty_and_unchanged_files() {
        assert_eq!(
            format_savings(0, 0, 0),
            "0.0 B -> 0.0 B, saved 0.0% (0.0 B)"
        );
        assert_eq!(
            format_savings(1_500_000, 1_500_000, 0),
            "1.5 MB -> 1.5 MB, saved 0.0% (0.0 B)"
        );
    }

    fn sample_report() -> Report {
        Report {
            version: "test".into(),
            mode: "dry-run".into(),
            presets_calibrated: false,
            cache_enabled: false,
            files: vec![FileReport {
                source: "source.png".into(),
                output: Some("output.png".into()),
                status: "planned".into(),
                cache_hit: false,
                reason: None,
                original_bytes: 1_500_000,
                optimized_bytes: Some(820_400),
                saving_bytes: 679_600,
                score: Some(88.0),
                score_basis: None,
                encoder: Some("test-encoder".into()),
                format: Some("png".into()),
                width: Some(100),
                height: Some(100),
                has_alpha: Some(false),
                class: None,
                android: None,
                candidates: vec![CandidateSummary {
                    encoder: "test-encoder".into(),
                    format: imaging::Format::Png,
                    quality: Some(80),
                    size: 820_400,
                    score: Some(88.0),
                    lossless: false,
                    accepted: true,
                    note: None,
                }],
                warnings: vec![],
                elapsed_ms: 12,
            }],
            summary: Summary {
                scanned: 1,
                optimized: 1,
                original_bytes: 1_500_000,
                optimized_bytes: 820_400,
                saving_bytes: 679_600,
                elapsed_ms: 12,
                ..Default::default()
            },
            renames: vec![],
            reference_warnings: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn text_reports_show_human_readable_file_candidate_and_total_sizes() {
        let report = sample_report();
        let text = report.text(true);
        assert!(text.contains(
            "PLANNED source.png -> output.png  1.5 MB -> 820.4 KB, saved 45.3% (679.6 KB), score 88.00\n"
        ));
        assert!(text.contains("test-encoder Some(80): 820.4 KB, score 88.00, accepted=true\n"));
        assert!(text.contains(
            "1 files; 1 optimized, 0 skipped, 0 errors; 1.5 MB -> 820.4 KB, saved 45.3% (679.6 KB); 12 ms\n"
        ));
        assert!(text.is_ascii());
    }

    #[test]
    fn json_reports_keep_numeric_byte_fields() {
        let json = serde_json::to_value(sample_report()).unwrap();
        for fields in [&json["files"][0], &json["summary"]] {
            assert_eq!(fields["original_bytes"].as_u64(), Some(1_500_000));
            assert_eq!(fields["optimized_bytes"].as_u64(), Some(820_400));
            assert_eq!(fields["saving_bytes"].as_u64(), Some(679_600));
        }
        assert_eq!(
            json["files"][0]["candidates"][0]["size"].as_u64(),
            Some(820_400)
        );
    }
}
