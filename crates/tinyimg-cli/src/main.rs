use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};
use tinyimg_core::{
    Mode, RunOptions,
    config::{self, Config, SettingsPatch},
    scanner,
};

#[derive(Parser, Debug)]
#[command(
    name = "tinyimg",
    version,
    about = "Offline perceptual image optimization for Android/App resources",
    after_help = "WARNING: default mode modifies originals. Start with --dry-run or -o out/.\nPreset scores are experimental, not visually calibrated. AVIF is not available in this build."
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
    Analyze(Box<OptimizeArgs>),
    /// Find hard-coded image filename references; never edits sources.
    Refs(RefsArgs),
    /// Create a configuration template without overwriting an existing file.
    Init {
        #[arg(default_value = "tinyimg.toml")]
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
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
    #[arg(long,value_parser=["high","balanced","small"])]
    preset: Option<String>,
    #[arg(long)]
    score: Option<f64>,
    #[arg(long, value_delimiter = ',')]
    formats: Option<Vec<String>>,
    #[arg(long)]
    keep_format: bool,
    #[arg(long, value_name = "PCT")]
    min_saving: Option<f64>,
    /// Skip inputs smaller than this byte count (default 2048).
    #[arg(long)]
    min_bytes: Option<u64>,
    #[arg(long, value_name = "GLOB")]
    include: Vec<String>,
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    #[arg(short = 'o', long, value_name = "DIR", conflicts_with = "backup")]
    output: Option<PathBuf>,
    #[arg(long, value_name = "DIR")]
    backup: Option<PathBuf>,
    #[arg(long, conflicts_with = "keep_metadata")]
    strip_all: bool,
    #[arg(long)]
    keep_metadata: bool,
    #[arg(long, conflicts_with = "check")]
    dry_run: bool,
    #[arg(long)]
    check: bool,
    #[arg(long)]
    jobs: Option<usize>,
    /// Reserved for cache compatibility; this release always recomputes results.
    #[arg(long)]
    no_cache: bool,
    #[arg(long, value_enum, default_value = "text")]
    report: ReportFormat,
    #[arg(long, value_name = "PATH")]
    report_file: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    #[arg(short = 'v', long, conflicts_with = "quiet")]
    verbose: bool,
    #[arg(short = 'q', long)]
    quiet: bool,
}
#[derive(Debug, Args)]
struct RefsArgs {
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
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
        eprintln!("workers: {jobs}; cache: disabled (not implemented)");
    }
    let options = RunOptions {
        inputs: paths,
        config,
        cli_settings,
        output: args.output,
        backup: args.backup,
        mode,
        jobs,
    };
    let report = tinyimg_core::run(&options).map_err(|e| (1, e))?;
    let rendered = match args.report {
        ReportFormat::Json => {
            serde_json::to_string_pretty(&report).map_err(|e| (1, e.into()))? + "\n"
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
