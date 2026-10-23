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
}
#[derive(Debug, Serialize)]
pub struct FileReport {
    #[serde(serialize_with = "crate::paths::serialize_path")]
    pub source: PathBuf,
    #[serde(serialize_with = "crate::paths::serialize_optional_path")]
    pub output: Option<PathBuf>,
    pub status: String,
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
    pub errors: usize,
    pub original_bytes: u64,
    pub optimized_bytes: u64,
    pub saving_bytes: u64,
    pub elapsed_ms: u64,
}
impl Report {
    pub fn text(&self, verbose: bool) -> String {
        let mut text = String::new();
        for file in &self.files {
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
                    "{} {} -> {}  {} -> {} bytes (-{}), score {}\n",
                    file.status.to_uppercase(),
                    file.source.display(),
                    file.output.as_deref().unwrap_or(&file.source).display(),
                    file.original_bytes,
                    file.optimized_bytes.unwrap_or(file.original_bytes),
                    file.saving_bytes,
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
            if verbose || self.mode == "analyze" {
                for c in &file.candidates {
                    text.push_str(&format!(
                        "  {} {:?}: {} bytes, score {}, accepted={}{}\n",
                        c.encoder,
                        c.quality,
                        c.size,
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
        }
        text.push_str(&format!(
            "\n{} files; {} optimized, {} skipped, {} errors; {} bytes saved ({:.1}%); {} ms\n",
            self.summary.scanned,
            self.summary.optimized,
            self.summary.skipped,
            self.summary.errors,
            self.summary.saving_bytes,
            if self.summary.original_bytes > 0 {
                self.summary.saving_bytes as f64 * 100. / self.summary.original_bytes as f64
            } else {
                0.
            },
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
            text.push_str(&format!("warning: {warning}\n"));
        }
        text
    }
}
fn base_report(item: &scanner::ScanItem) -> FileReport {
    FileReport {
        source: item.source.clone(),
        output: None,
        status: "skipped".into(),
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

pub fn run(options: &RunOptions) -> Result<Report> {
    let start = Instant::now();
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
    let scan = scanner::scan(
        &options.inputs,
        &ScanOptions {
            include: global.include.unwrap_or_default(),
            exclude: global.exclude.unwrap_or_default(),
            excluded_dirs: excluded_dirs.clone(),
        },
    )?;
    // Validate every effective configuration before the first possible write.
    for item in &scan.items {
        options
            .config
            .effective(&item.relative_path, &options.cli_settings)?
            .optimize_options(item.is_asset)?;
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs)
        .build()
        .context("create image worker pool")?;
    let files=pool.install(||scan.items.par_iter().map(|item|{
  let started=Instant::now();
  let mut report=base_report(item);
  if let Some(reason)=&item.skip_reason{report.reason=Some(reason.clone());return report;}
  let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||process(item,options,&mut report)));
  match outcome {Ok(Ok(()))=>{},Ok(Err(error))=>{report.status="error".into();report.reason=Some(format!("{error:#}"));report.saving_bytes=0;},Err(_)=>{report.status="error".into();report.reason=Some("encoder panicked; source was not intentionally removed; inspect any recovery message".into());report.saving_bytes=0;}}
  report.elapsed_ms=started.elapsed().as_millis() as u64;report
 }).collect::<Vec<_>>());
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
    let mut warnings=vec!["Preset scores (high=88, balanced=80, small=70) are experimental, not visually calibrated. Review important UI/text assets before replacement.".into()];
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
        cache_enabled: false,
        files,
        summary,
        renames,
        reference_warnings,
        warnings,
    })
}
fn process(item: &scanner::ScanItem, options: &RunOptions, report: &mut FileReport) -> Result<()> {
    let snapshot = SourceSnapshot::capture(&item.source)?;
    report.original_bytes = snapshot.bytes().len() as u64;
    let settings = options
        .config
        .effective(&item.relative_path, &options.cli_settings)?;
    let min_bytes = settings.min_bytes.unwrap_or(2048);
    if report.original_bytes < min_bytes {
        report.reason = Some(format!("smaller than min_bytes ({min_bytes})"));
        return Ok(());
    }
    let image_options = settings.optimize_options(item.is_asset)?;
    if item.is_asset {
        report.warnings.push(
            "assets path: format conversion disabled to protect filename-based access".into(),
        );
    }
    let optimized = match imaging::optimize_bytes(snapshot.bytes(), &image_options) {
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
        writer::write_candidate(
            &item.source,
            &target,
            snapshot.hash(),
            &candidate.bytes,
            backup.as_deref(),
            options.output.is_some(),
        )?;
        report.status = "written".into();
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
