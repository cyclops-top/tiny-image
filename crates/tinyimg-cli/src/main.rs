mod progress;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    path::PathBuf,
    process::ExitCode,
};
use tinyimg_core::{
    Mode, RunOptions,
    config::{self, Config, SettingsPatch},
    scanner,
};

const OPTIMIZE_HELP: &str = r#"Operation:
  PATH accepts one or more files or directories; omitted PATH means current directory.
  Directories are scanned recursively. No -r/--recursive flag is needed.
  Default mode modifies originals and may change extensions/remove old files.
  Directory in-place writes with eligible images require y/yes at [y/N].
  Enter, EOF or any other answer cancels; --backup still requires confirmation.
  Explicit files do not prompt. Use --dry-run, --backup DIR or -o DIR first.
  Output/backup directories are excluded from scanning; existing output/backup files are not overwritten.
  Descendant .git, target, node_modules and .tinyimg-cache directories are pruned.
  Symlinks are not followed; explicit symlink inputs are rejected.

Configuration and limits:
  Listed defaults are built-in values; tinyimg.toml and CLI options may override them.
  Priority: built-in defaults < top-level config < matching overrides (in order) < CLI.
  Static PNG, JPEG and WebP inputs are supported. AVIF output is opt-in; external AVIF
  input is not decoded. Protected Android resources and unsafe/unsupported inputs are skipped.
  Preset scores are experimental, not visually calibrated. Every saved result must be smaller.
  Safety limits: 256 MiB input, 8000 pixels per side, 16 million pixels; AVIF: 4 million pixels.

Terminal and reports:
  Interactive terminals show per-image phases/attempts and total progress on stderr.
  JSON, --quiet, redirected stdout/stderr and TERM=dumb disable progress.
  NO_COLOR disables colors. Human sizes use decimal units (1 KB = 1000 B), one decimal.
  JSON retains integer bytes. Prompts/fatal errors use stderr; per-file results use the report.
  --quiet does not bypass confirmation.

Exit codes:
  0 success or user cancellation; 1 runtime/file error; 2 invalid arguments/configuration;
  3 --check found optimizable files (no files changed).

Examples:
  tinyimg ./images --dry-run
  tinyimg "./图片 资源" -o ./optimized
  tinyimg a.png b.webp --keep-format --formats png,webp --dry-run
  tinyimg ./images --backup ./originals --jobs 4
  tinyimg ./images --include '**/*.png' --exclude '**/vendor/**' --dry-run
  tinyimg ./images --check --report json
  tinyimg analyze ./images --preset high
  tinyimg refs ./app
  tinyimg init
  tinyimg --dry-run -- -leading-dash.png

Use 'tinyimg <COMMAND> --help' for subcommand details."#;

const ANALYZE_HELP: &str = r#"Read-only analysis:
  Recursively evaluate selected images and show all tested candidates; never write images,
  output directories, backups, processed-output markers or report files.
  Processed-output markers are always bypassed. --output/--backup do not write in this mode.
  Do not combine analyze with --dry-run, --check or --report-file; use stdout/redirection.
  Built-in quality/filter defaults and tinyimg.toml precedence match normal optimization.
  Preset scores are experimental, not visually calibrated. AVIF is output-only and opt-in.
  Progress is terminal-only and is disabled by JSON, --quiet or redirected stdout/stderr.
  --quiet suppresses text, not JSON. Exit codes: 0 success, 1 runtime/file error, 2 invalid usage.

Examples:
  tinyimg analyze ./images
  tinyimg analyze "./图片 资源" --formats png,webp --score 90
  tinyimg analyze ./images --report json > analysis.json"#;

#[derive(Parser, Debug)]
#[command(
    name = "tinyimg",
    version,
    about = "Offline perceptual image optimization for Android/App resources",
    after_help = OPTIMIZE_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    args: OptimizeArgs,
}
#[derive(Subcommand, Debug)]
enum Command {
    /// Compare all tested candidates without changing any files.
    #[command(after_help = ANALYZE_HELP)]
    Analyze(Box<OptimizeArgs>),
    /// Find hard-coded image filename references; never edits sources.
    #[command(
        after_help = "Recursively scan supported text/source files for hard-coded image filename references.\nNo images or source files are changed. PATH defaults to the current directory.\nThis is an advisory literal search, not a parser; dynamic/binary references may be missed.\nFinding references is not an error (exit 0).\n\nExamples:\n  tinyimg refs ./app\n  tinyimg refs ./app --report json"
    )]
    Refs(RefsArgs),
    /// Create a configuration template without overwriting an existing file.
    #[command(
        after_help = "The parent directory must already exist. Existing files are never overwritten.\n\nExamples:\n  tinyimg init\n  tinyimg init ./config/tinyimg.toml"
    )]
    Init {
        /// Destination for the new configuration template.
        #[arg(default_value = "tinyimg.toml", value_name = "PATH")]
        path: PathBuf,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReportFormat {
    Text,
    Json,
}
#[derive(Debug, Clone, Args)]
struct OptimizeArgs {
    /// Files/directories to process recursively [default: current directory].
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Quality preset: high=88, balanced=80, small=70 [default: balanced].
    #[arg(long, value_parser=["high","balanced","small"], value_name = "PRESET")]
    preset: Option<String>,
    /// SSIMULACRA2 target, 0..100; overrides the preset in this layer.
    #[arg(long, value_name = "SCORE")]
    score: Option<f64>,
    /// Comma-separated candidate formats: png,webp,jpeg (or jpg),avif [default: png,webp].
    #[arg(long, value_delimiter = ',', value_name = "FORMATS")]
    formats: Option<Vec<String>>,
    /// Restrict candidates to the input format (must also be in --formats); always used for assets.
    #[arg(long)]
    keep_format: bool,
    /// Minimum PNG-to-other-format saving, 0..100 percent [default: 10].
    ///
    /// Compared with the best eligible PNG candidate, or the original PNG if none exists.
    /// Not a universal minimum saving for same-format results; every saved result must be smaller.
    #[arg(long, value_name = "PCT")]
    min_saving: Option<f64>,
    /// Skip inputs smaller than this many bytes [default: 2048].
    #[arg(long, value_name = "BYTES")]
    min_bytes: Option<u64>,
    /// Include matching relative paths; repeat for multiple globs (quote patterns; use / separators).
    ///
    /// CLI patterns replace the config include list. Multiple roots add root-name prefixes to paths.
    #[arg(long, value_name = "GLOB")]
    include: Vec<String>,
    /// Exclude matching relative paths/directories; repeat for multiple globs (quote patterns).
    ///
    /// CLI patterns replace the config exclude list; built-in safety exclusions remain active.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Write optimized files to DIR instead of replacing originals; skipped files are not copied.
    ///
    /// Preserves relative paths; multiple roots get root-name prefixes. Not a complete input mirror.
    /// Existing destinations are never overwritten. Incompatible with --backup.
    #[arg(short = 'o', long, value_name = "DIR", conflicts_with = "backup")]
    output: Option<PathBuf>,
    /// Back up originals before in-place writes; preserve relative paths and never overwrite backups.
    #[arg(long, value_name = "DIR")]
    backup: Option<PathBuf>,
    /// Strip removable metadata; unsafe color/orientation cases still retain the original.
    #[arg(long, conflicts_with = "keep_metadata")]
    strip_all: bool,
    /// Preserve metadata using PNG lossless optimization only; other source formats are skipped.
    #[arg(long)]
    keep_metadata: bool,
    /// Evaluate planned changes without writing images, directories, markers or report files.
    #[arg(long, conflicts_with = "check")]
    dry_run: bool,
    /// Read-only check: exit 3 if any file can be optimized; otherwise 0 (errors still fail).
    #[arg(long)]
    check: bool,
    /// Parallel image workers, 1..64 [default: min(4, CPU parallelism)]; more workers use more memory.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,
    /// Bypass processed-output markers and recompute candidates; never read/write those markers.
    #[arg(long)]
    no_cache: bool,
    /// Report format; JSON retains numeric byte counts and disables terminal progress.
    #[arg(long, value_enum, default_value = "text")]
    report: ReportFormat,
    /// Create a new report file instead of stdout; never overwrite; unavailable in read-only modes.
    ///
    /// Parent directory must exist. Image extensions are rejected; report creation uses path safety checks.
    #[arg(long, value_name = "PATH")]
    report_file: Option<PathBuf>,
    /// Read this TOML config [default: nearest tinyimg.toml found upward from the first input].
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Show configuration, worker count, candidate details and experimental-preset notice.
    #[arg(short = 'v', long, conflicts_with = "quiet")]
    verbose: bool,
    /// Suppress text/progress, not JSON, fatal errors or directory confirmation; conflicts with -v.
    #[arg(short = 'q', long)]
    quiet: bool,
}
#[derive(Debug, Args)]
struct RefsArgs {
    /// Files/directories to scan recursively [default: current directory].
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Print matching source locations and image literals as text or JSON.
    #[arg(long, value_enum, default_value = "text")]
    report: ReportFormat,
}
fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Analyze(args)) => optimize(*args, true),
        Some(Command::Refs(args)) => refs(args),
        Some(Command::Init { path }) => init(path),
        None => optimize(cli.args, false),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err((code, error)) => {
            eprintln!("tinyimg: {error:#}");
            ExitCode::from(code)
        }
    }
}
type CliResult = Result<u8, (u8, anyhow::Error)>;
fn fail(code: u8, message: impl Into<String>) -> (u8, anyhow::Error) {
    (code, anyhow::anyhow!(message.into()))
}
fn optimize(mut args: OptimizeArgs, analyze: bool) -> CliResult {
    let paths = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths.clone()
    };
    let mode = if analyze {
        Mode::Analyze
    } else if args.check {
        Mode::Check
    } else if args.dry_run {
        Mode::DryRun
    } else {
        Mode::Write
    };
    if mode != Mode::Write && args.report_file.is_some() {
        return Err(fail(
            2,
            "--report-file is incompatible with read-only analyze/--dry-run/--check; use stdout or explicit shell redirection",
        ));
    }
    if analyze && (args.check || args.dry_run) {
        return Err(fail(
            2,
            "analyze is already read-only; do not combine with --check or --dry-run",
        ));
    }
    if args
        .output
        .as_ref()
        .is_some_and(|p| p.as_os_str().is_empty())
    {
        return Err(fail(2, "output directory cannot be empty"));
    }
    if let Some(path) = &args.report_file {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|e| (1, e.into()))?;
        let filename = path
            .file_name()
            .ok_or_else(|| fail(2, "report-file must name a file"))?;
        let resolved = parent.join(filename);
        preflight_report(&resolved)?;
        args.report_file = Some(resolved);
    }
    let config_path = args
        .config
        .clone()
        .or_else(|| config::discover_config(&paths));
    let config = if let Some(path) = &config_path {
        Config::read(path).map_err(|e| (2, e))?
    } else {
        Config::default()
    };
    let cli_settings = SettingsPatch {
        preset: args.preset,
        score: args.score,
        formats: args.formats,
        keep_format: args.keep_format.then_some(true),
        min_saving: args.min_saving,
        min_bytes: args.min_bytes,
        include: (!args.include.is_empty()).then_some(args.include),
        exclude: (!args.exclude.is_empty()).then_some(args.exclude),
        strip_all: args.strip_all.then_some(true),
        keep_metadata: args.keep_metadata.then_some(true),
    };
    cli_settings.validate().map_err(|e| (2, e))?;
    let jobs = args.jobs.unwrap_or_else(tinyimg_core::default_jobs);
    if jobs == 0 || jobs > 64 {
        return Err(fail(2, "--jobs must be between 1 and 64"));
    }
    if !args.quiet
        && mode == Mode::Write
        && args.output.is_none()
        && let Some(w) = tinyimg_core::git_warning(&paths)
    {
        eprintln!("warning: {w}");
    }
    if args.verbose {
        if let Some(path) = config_path {
            eprintln!("configuration: {}", path.display());
        }
        eprintln!(
            "workers: {jobs}; processed-output markers: {}",
            if args.no_cache || analyze {
                "disabled"
            } else {
                "enabled"
            }
        );
    }
    let options = RunOptions {
        inputs: paths,
        config,
        cli_settings,
        output: args.output,
        backup: args.backup,
        mode,
        jobs,
        use_cache: !args.no_cache,
    };
    let prepared = tinyimg_core::prepare(&options).map_err(|e| (1, e))?;
    if prepared.requires_confirmation() {
        let eligible_images = prepared.eligible_images();
        let known_skipped = prepared.known_skipped();
        if !confirm_directory_replacement(eligible_images, known_skipped)
            .map_err(|e| (1, e.into()))?
        {
            eprintln!("Cancelled; no files changed.");
            // Cancellation must never create --report-file, but JSON callers
            // still receive machine-readable stdout rather than prompt text.
            if matches!(args.report, ReportFormat::Json) {
                write_stdout(&format!(
                    "{}\n",
                    serde_json::json!({
                        "cancelled": true,
                        "eligible_images": eligible_images,
                        "known_skipped": known_skipped,
                        "reason": "directory replacement not confirmed",
                    })
                ))?;
            }
            return Ok(0);
        }
    }
    let progress = progress::Progress::new(
        prepared.eligible_images() + prepared.known_skipped(),
        args.quiet,
        matches!(args.report, ReportFormat::Json),
        args.verbose,
        analyze,
    );
    let result = prepared.run_with_progress(|event| progress.update(event));
    let streamed = progress.streamed_results();
    drop(progress); // Finish the terminal line before reports or errors.
    let report = result.map_err(|e| (1, e))?;
    let rendered = match args.report {
        ReportFormat::Json => {
            serde_json::to_string_pretty(&report).map_err(|e| (1, e.into()))? + "\n"
        }
        ReportFormat::Text if streamed && args.report_file.is_none() => {
            report.summary_text(args.verbose)
        }
        ReportFormat::Text => report.text(args.verbose),
    };
    if let Some(path) = args.report_file {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                fail(
                    1,
                    format!(
                        "cannot create report {} (existing reports are never overwritten): {e}",
                        path.display()
                    ),
                )
            })?;
        file.write_all(rendered.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| (1, e.into()))?;
    } else if !args.quiet || matches!(args.report, ReportFormat::Json) {
        write_stdout(&rendered)?;
    }
    Ok(if report.summary.errors > 0 {
        1
    } else if mode == Mode::Check && report.summary.optimized > 0 {
        3
    } else {
        0
    })
}
/// An empty line, EOF, or anything other than y/yes is a safe cancellation.
fn confirm_directory_replacement(eligible_images: usize, known_skipped: usize) -> io::Result<bool> {
    let mut stderr = io::stderr().lock();
    write!(
        stderr,
        "Found {eligible_images} eligible image(s) after filtering; {known_skipped} known skipped item(s). Originals may be replaced or removed. Continue in place? [y/N] "
    )?;
    stderr.flush()?;
    let mut response = String::new();
    io::stdin().lock().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn refs(args: RefsArgs) -> CliResult {
    let inputs = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths
    };
    let hits = scanner::refs(&inputs, &[]).map_err(|e| (1, e))?;
    let text = match args.report {
        ReportFormat::Json => {
            serde_json::to_string_pretty(&hits).map_err(|e| (1, e.into()))? + "\n"
        }
        ReportFormat::Text => {
            let mut text = String::new();
            for hit in &hits {
                text.push_str(&format!(
                    "{}:{}: {}\n",
                    hit.file.display(),
                    hit.line,
                    hit.literal
                ));
            }
            text.push_str(&format!(
                "{} hard-coded image references; no files changed.\n",
                hits.len()
            ));
            text
        }
    };
    write_stdout(&text)?;
    Ok(0)
}
fn init(path: PathBuf) -> CliResult {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            fail(
                1,
                format!(
                    "cannot create {} (existing files are never overwritten): {e}",
                    path.display()
                ),
            )
        })?;
    file.write_all(config::TEMPLATE.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| (1, e.into()))?;
    write_stdout(&format!("Created {}\n", path.display()))?;
    Ok(0)
}
fn write_stdout(text: &str) -> Result<(), (u8, anyhow::Error)> {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err((1, e.into())),
    }
}

fn preflight_report(path: &std::path::Path) -> Result<(), (u8, anyhow::Error)> {
    if path.file_name().is_none() {
        return Err(fail(2, "--report-file must name a file"));
    }
    if fs::symlink_metadata(path).is_ok() {
        return Err(fail(
            1,
            format!(
                "report already exists (never overwritten): {}",
                path.display()
            ),
        ));
    }
    if path.extension().and_then(|v| v.to_str()).is_some_and(|e| {
        ["png", "jpg", "jpeg", "webp", "avif", "gif", "bmp", "tiff"]
            .contains(&e.to_ascii_lowercase().as_str())
    }) {
        return Err(fail(2, "report filename must not use an image extension"));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| (1, e.into()))?
            .join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| fail(2, "report file has no parent directory"))?;
    for ancestor in parent.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|e| {
            fail(
                1,
                format!("report parent is unavailable {}: {e}", ancestor.display()),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(fail(
                1,
                format!(
                    "report parent must be an existing non-symlink directory: {}",
                    ancestor.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn every_exposed_option_and_path_has_a_help_description() {
        let command = Cli::command();
        for command in std::iter::once(&command).chain(command.get_subcommands()) {
            for argument in command.get_arguments() {
                assert!(
                    argument.get_help().is_some() || argument.get_long_help().is_some(),
                    "{}: {} has no help",
                    command.get_name(),
                    argument.get_id(),
                );
            }
        }
    }

    #[test]
    fn help_examples_parse_quoted_unicode_globs_and_leading_dash_paths() {
        for arguments in [
            vec!["tinyimg", "./images", "--dry-run"],
            vec!["tinyimg", "./图片 资源", "-o", "./optimized"],
            vec![
                "tinyimg",
                "a.png",
                "b.webp",
                "--keep-format",
                "--formats",
                "png,webp",
                "--dry-run",
            ],
            vec![
                "tinyimg",
                "./images",
                "--backup",
                "./originals",
                "--jobs",
                "4",
            ],
            vec![
                "tinyimg",
                "./images",
                "--include",
                "**/*.png",
                "--exclude",
                "**/vendor/**",
                "--dry-run",
            ],
            vec!["tinyimg", "./images", "--check", "--report", "json"],
            vec!["tinyimg", "analyze", "./images", "--preset", "high"],
            vec![
                "tinyimg",
                "analyze",
                "./图片 资源",
                "--formats",
                "png,webp",
                "--score",
                "90",
            ],
            vec!["tinyimg", "analyze", "./images", "--report", "json"],
            vec!["tinyimg", "refs", "./app", "--report", "json"],
            vec!["tinyimg", "init"],
            vec!["tinyimg", "init", "./config/tinyimg.toml"],
            vec!["tinyimg", "--dry-run", "--", "-leading-dash.png"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_ok(), "{arguments:?}");
        }
        let cli = Cli::try_parse_from(["tinyimg", "--dry-run", "--", "-leading-dash.png"]).unwrap();
        assert_eq!(cli.args.paths, vec![PathBuf::from("-leading-dash.png")]);
    }

    #[test]
    fn help_defaults_remain_documentation_not_cli_overrides() {
        let cli = Cli::try_parse_from(["tinyimg"]).unwrap();
        assert!(cli.args.paths.is_empty());
        assert!(cli.args.preset.is_none());
        assert!(cli.args.score.is_none());
        assert!(cli.args.formats.is_none());
        assert!(cli.args.min_saving.is_none());
        assert!(cli.args.min_bytes.is_none());
        assert!(cli.args.jobs.is_none());
        assert!(cli.args.config.is_none());
        assert!(!cli.args.dry_run && !cli.args.check);
    }
}
