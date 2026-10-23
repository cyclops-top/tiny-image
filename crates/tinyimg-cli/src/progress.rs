//! Terminal-only active-image rows; rendering failures never interrupt image writes.
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    sync::Mutex,
    time::{Duration, Instant},
};
use tinyimg_core::{ProgressEvent, imaging::ImageProgress};

pub(crate) struct Progress {
    display: Option<Mutex<Display>>,
}

fn enabled(total: usize, quiet: bool, json: bool, stdout_tty: bool, stderr_tty: bool) -> bool {
    total > 0 && !quiet && !json && stdout_tty && stderr_tty
}

impl Progress {
    // Construct only after the user approves directory replacement.
    pub(crate) fn new(total: usize, quiet: bool, json: bool, verbose: bool, analyze: bool) -> Self {
        let display = enabled(
            total,
            quiet,
            json,
            io::stdout().is_terminal(),
            io::stderr().is_terminal(),
        )
        .then(|| {
            let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));
            if multi.is_hidden() {
                return None;
            }
            let (height, width) = console::Term::stderr().size();
            Some(Mutex::new(Display::new(
                multi, total, height, width, verbose, analyze,
            )))
        })
        .flatten();
        Self { display }
    }

    pub(crate) fn update(&self, event: ProgressEvent<'_>) {
        if let Some(display) = &self.display
            && let Ok(mut display) = display.lock()
        {
            display.update(event);
        }
    }

    pub(crate) fn streamed_results(&self) -> bool {
        self.display
            .as_ref()
            .is_some_and(|display| display.lock().is_ok_and(|display| !display.output_failed))
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(display) = &mut self.display {
            display
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .finish();
        }
    }
}

struct Active {
    name: String,
    phase: ImageProgress,
    started: Instant,
    bar: Option<ProgressBar>,
}

struct Display {
    multi: MultiProgress,
    summary: ProgressBar,
    active: BTreeMap<usize, Active>,
    completed: Vec<bool>,
    done: usize,
    row_limit: usize,
    narrow: bool,
    verbose: bool,
    analyze: bool,
    output_failed: bool,
    record_writer: Box<dyn Write + Send>,
}

impl Display {
    fn new(
        multi: MultiProgress,
        total: usize,
        height: u16,
        width: u16,
        verbose: bool,
        analyze: bool,
    ) -> Self {
        let summary = multi.add(ProgressBar::new(total as u64));
        summary.set_style(
            ProgressStyle::with_template("{pos}/{len} [{elapsed_precise}] {wide_msg}")
                .expect("static summary template"),
        );
        summary.enable_steady_tick(Duration::from_millis(100));
        summary.set_message("active 0");
        Self {
            multi,
            summary,
            active: BTreeMap::new(),
            completed: vec![false; total],
            done: 0,
            // Keep the summary and permanent records readable, even with --jobs 64.
            row_limit: usize::from(height.saturating_sub(4)).clamp(1, 8),
            narrow: width < 60,
            verbose,
            analyze,
            output_failed: false,
            record_writer: Box::new(io::stderr()),
        }
    }

    fn update(&mut self, event: ProgressEvent<'_>) {
        match event {
            ProgressEvent::Phase {
                file_index,
                source,
                progress,
            } => {
                if self.completed[file_index] {
                    return;
                }
                let active = self.active.entry(file_index).or_insert_with(|| Active {
                    name: sanitize(
                        &source
                            .file_name()
                            .unwrap_or(source.as_os_str())
                            .to_string_lossy(),
                    ),
                    phase: progress,
                    started: Instant::now(),
                    bar: None,
                });
                active.phase = progress;
            }
            ProgressEvent::Complete { file_index, report } => {
                if self.completed[file_index] {
                    return;
                }
                self.completed[file_index] = true;
                self.done += 1;
                if let Some(active) = self.active.remove(&file_index)
                    && let Some(bar) = active.bar
                {
                    bar.finish_and_clear();
                    self.multi.remove(&bar);
                }
                // Preserve result, skip/error reasons, and actionable warnings once.
                let record = if self.verbose || self.analyze {
                    report.text(self.verbose, self.analyze)
                } else {
                    completion_record(file_index, report)
                };
                // indicatif::println pads to terminal width and relies on soft wraps.
                // Real newlines keep copied/pasted completion records separate.
                let writer = &mut self.record_writer;
                if self
                    .multi
                    .suspend(|| write_records(writer, &record))
                    .is_err()
                {
                    self.output_failed = true;
                }
                self.summary.set_position(self.done as u64);
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        let mut shown = self
            .active
            .values()
            .filter(|active| active.bar.is_some())
            .count();
        for (index, active) in &mut self.active {
            if active.bar.is_none() && shown < self.row_limit {
                let bar = self
                    .multi
                    .insert_before(&self.summary, ProgressBar::new_spinner());
                let template = if self.narrow {
                    "{spinner} [{elapsed_precise}] {wide_msg}"
                } else {
                    "{spinner} {wide_msg} [{elapsed_precise}]"
                };
                bar.set_style(
                    ProgressStyle::with_template(template)
                        .expect("static image template")
                        .tick_strings(&["|", "/", "-", "\\"]),
                );
                bar.set_elapsed(active.started.elapsed());
                bar.enable_steady_tick(Duration::from_millis(100));
                active.bar = Some(bar);
                shown += 1;
            }
            if let Some(bar) = &active.bar {
                let name =
                    console::truncate_str(&active.name, if self.narrow { 12 } else { 24 }, "...");
                let attempt = if active.phase.attempt > 0 {
                    format!(" try {}", active.phase.attempt)
                } else {
                    String::new()
                };
                bar.set_message(format!(
                    "#{} {} {}{}",
                    index + 1,
                    name,
                    active.phase.phase,
                    attempt
                ));
            }
        }
        let hidden = self.active.len().saturating_sub(shown);
        self.summary.set_message(if hidden > 0 {
            format!("active {} (+{} hidden)", self.active.len(), hidden)
        } else {
            format!("active {}", self.active.len())
        });
    }

    fn finish(&mut self) {
        for active in self.active.values() {
            if let Some(bar) = &active.bar {
                bar.finish_and_clear();
            }
        }
        self.summary.finish_and_clear();
        let _ = self.multi.clear();
    }
}

fn write_records(writer: &mut dyn Write, record: &str) -> io::Result<()> {
    for line in record.trim_end().lines() {
        writeln!(writer, "{}", sanitize(line))?;
    }
    writer.flush()
}

fn completion_record(index: usize, report: &tinyimg_core::FileReport) -> String {
    let name = report
        .source
        .file_name()
        .unwrap_or(report.source.as_os_str())
        .to_string_lossy();
    let label = format!("#{} {}", index + 1, name);
    let mut record = match report.status.as_str() {
        "skipped" => format!(
            "SKIP {label}: {}\n",
            report.reason.as_deref().unwrap_or("no improvement")
        ),
        "error" => format!(
            "ERROR {label}: {}\n",
            report.reason.as_deref().unwrap_or("unknown error")
        ),
        _ => format!(
            "{} {label}: {} -> {} bytes (-{})\n",
            report.status.to_uppercase(),
            report.original_bytes,
            report.optimized_bytes.unwrap_or(report.original_bytes),
            report.saving_bytes
        ),
    };
    for warning in &report.warnings {
        record.push_str(&format!("  warning: {warning}\n"));
    }
    record
}

// Filenames/reasons must never inject cursor movement into the live terminal UI.
fn sanitize(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_normal_interactive_output_enables_progress() {
        for total in [0, 1, 20] {
            for quiet in [false, true] {
                for json in [false, true] {
                    for stdout in [false, true] {
                        for stderr in [false, true] {
                            assert_eq!(
                                enabled(total, quiet, json, stdout, stderr),
                                total > 0 && !quiet && !json && stdout && stderr
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn active_rows_are_bounded_and_phases_are_per_file() {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let mut display = Display::new(multi, 4, 6, 30, false, false);
        for file_index in [2, 0, 1] {
            display.update(ProgressEvent::Phase {
                file_index,
                source: Path::new("file.png"),
                progress: ImageProgress {
                    phase: "encoding",
                    attempt: file_index + 1,
                },
            });
        }
        assert_eq!(display.active.len(), 3);
        assert_eq!(
            display.active.values().filter(|a| a.bar.is_some()).count(),
            2
        );
        assert_eq!(display.active[&2].phase.attempt, 3);
        assert_eq!(display.summary.message(), "active 3 (+1 hidden)");
        display.finish();
        assert!(display.summary.is_finished());
        assert!(
            display
                .active
                .values()
                .filter_map(|a| a.bar.as_ref())
                .all(ProgressBar::is_finished)
        );
    }

    #[test]
    fn completions_remove_rows_promote_hidden_work_and_count_once() {
        use tinyimg_core::{
            Mode, RunOptions,
            config::{Config, SettingsPatch},
        };
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("small.png"), b"tiny").unwrap();
        let report = tinyimg_core::run(&RunOptions {
            inputs: vec![directory.path().to_path_buf()],
            config: Config::default(),
            cli_settings: SettingsPatch::default(),
            output: None,
            backup: None,
            mode: Mode::DryRun,
            jobs: 1,
            use_cache: false,
        })
        .unwrap();
        let mut display = Display::new(
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden()),
            3,
            5,
            80,
            false,
            false,
        );
        display.record_writer = Box::new(io::sink());
        for file_index in 0..3 {
            display.update(ProgressEvent::Phase {
                file_index,
                source: Path::new("image.png"),
                progress: ImageProgress {
                    phase: "encoding",
                    attempt: 1,
                },
            });
        }
        assert!(display.active[&0].bar.is_some());
        assert!(display.active[&1].bar.is_none());
        display.update(ProgressEvent::Complete {
            file_index: 0,
            report: &report.files[0],
        });
        assert!(!display.active.contains_key(&0));
        assert!(display.active[&1].bar.is_some());
        assert_eq!(display.done, 1);
        display.update(ProgressEvent::Complete {
            file_index: 0,
            report: &report.files[0],
        });
        assert_eq!(display.done, 1);
        for file_index in [2, 1] {
            display.update(ProgressEvent::Complete {
                file_index,
                report: &report.files[0],
            });
        }
        assert!(display.active.is_empty());
        assert_eq!(display.done, 3);
        assert_eq!(display.summary.position(), 3);
        assert_eq!(display.summary.message(), "active 0");
        display.finish();
    }

    #[test]
    fn permanent_records_have_explicit_newlines_without_terminal_padding() {
        let record = "SKIP #16 猪-少年.png: PNG EXIF orientation is not normalized; original retained to avoid rotation changes\nSKIP #23 猴-幼年笑.png: original retained\n";
        let mut output = Vec::new();
        write_records(&mut output, record).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), record);
    }

    #[test]
    fn record_write_failures_are_returned_for_report_fallback() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_records(&mut Broken, "SKIP image.png\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn terminal_controls_in_names_and_messages_are_escaped() {
        assert_eq!(sanitize("photo\n\x1b[2J.png"), "photo\\n\\u{1b}[2J.png");
        assert_eq!(sanitize("照片.png"), "照片.png");
    }
}
