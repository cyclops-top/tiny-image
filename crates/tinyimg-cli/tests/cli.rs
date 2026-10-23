use image::{
    ImageEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tinyimg"))
}
fn confirmed(cmd: &mut Command) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    child.wait_with_output().unwrap()
}
fn png(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes: Vec<u8> = (0..64 * 64)
        .flat_map(|i| {
            [
                (i % 64 * 4) as u8,
                (i / 64 * 4) as u8,
                100,
                if i % 7 == 0 { 128 } else { 255 },
            ]
        })
        .collect();
    let f = fs::File::create(path).unwrap();
    PngEncoder::new_with_quality(f, CompressionType::Uncompressed, FilterType::NoFilter)
        .write_image(&bytes, 64, 64, image::ExtendedColorType::Rgba8)
        .unwrap();
}
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON {e}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(root).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.insert(
                    format!("{}/", p.strip_prefix(base).unwrap().display()),
                    vec![],
                );
                walk(&p, base, out)
            } else {
                out.insert(
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    fs::read(p).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn help_and_version_work() {
    assert!(bin().arg("--help").output().unwrap().status.success());
    assert!(bin().arg("--version").output().unwrap().status.success());
}
#[test]
fn all_readonly_modes_leave_entire_tree_unchanged() {
    let d = TempDir::new().unwrap();
    png(&d.path().join("res/drawable-mdpi/a.png"));
    let before = tree(d.path());
    for mode in ["--dry-run", "--check", "analyze"] {
        let mut cmd = bin();
        if mode == "analyze" {
            cmd.arg(mode);
        }
        cmd.arg(d.path()).args([
            "--formats",
            "png",
            "--min-bytes",
            "0",
            "--report",
            "json",
            "--jobs",
            "1",
        ]);
        if mode != "analyze" {
            cmd.arg(mode);
        }
        let out = cmd.output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(if mode == "--check" { 3 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let r = json(&out);
        assert_eq!(r["summary"]["optimized"], 1);
        assert_eq!(tree(d.path()), before);
        assert_eq!(r["cache_enabled"], mode != "analyze");
    }
}
#[test]
fn output_preserves_sources_and_relative_layout() {
    let d = TempDir::new().unwrap();
    let input = d.path().join("input");
    let source = input.join("drawable-hdpi/a.png");
    png(&source);
    let old = fs::read(&source).unwrap();
    let output = d.path().join("out");
    let result = bin()
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = json(&result);
    assert_eq!(report["summary"]["optimized"], 1);
    assert_eq!(fs::read(&source).unwrap(), old);
    let new = fs::read(output.join("drawable-hdpi/a.png")).unwrap();
    assert!(new.len() < old.len());
    assert_eq!(
        image::load_from_memory(&new).unwrap().to_rgba8(),
        image::load_from_memory(&old).unwrap().to_rgba8()
    );
}
#[test]
fn miniature_android_project_protects_special_resources() {
    let d = TempDir::new().unwrap();
    let main = d.path().join("app/src/main");
    let res = main.join("res");
    for name in [
        "drawable/a.png",
        "drawable/nine.9.png",
        "mipmap-hdpi/brand.png",
        "drawable/ic_launcher_foreground.png",
    ] {
        png(&res.join(name));
    }
    fs::write(
        main.join("AndroidManifest.xml"),
        "<application android:icon='@mipmap/brand'/>",
    )
    .unwrap();
    fs::write(res.join("drawable/vector.xml"), "<vector/>").unwrap();
    let nine = fs::read(res.join("drawable/nine.9.png")).unwrap();
    let brand = fs::read(res.join("mipmap-hdpi/brand.png")).unwrap();
    let out = confirmed(
        bin()
            .arg(&res)
            .args(["--formats", "png", "--report", "json"]),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = json(&out);
    assert_eq!(r["summary"]["optimized"], 1);
    assert_eq!(fs::read(res.join("drawable/nine.9.png")).unwrap(), nine);
    assert_eq!(fs::read(res.join("mipmap-hdpi/brand.png")).unwrap(), brand);
    assert_eq!(r["summary"]["skipped"], 4);
}
#[test]
fn backups_are_created_and_never_overwritten() {
    let d = TempDir::new().unwrap();
    let source = d.path().join("input/a.png");
    png(&source);
    let old = fs::read(&source).unwrap();
    let backup = d.path().join("backup");
    let output = bin()
        .arg(&source)
        .arg("--backup")
        .arg(&backup)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(backup.join("a.png")).unwrap(), old);
    png(&source);
    let output = bin()
        .arg(&source)
        .arg("--backup")
        .arg(&backup)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert_eq!(fs::read(backup.join("a.png")).unwrap(), old);
    assert_eq!(fs::read(&source).unwrap(), old);
    assert_eq!(output.status.code(), Some(1));
}
#[test]
fn errors_have_specified_exit_codes() {
    let d = TempDir::new().unwrap();
    for args in [
        vec!["--score", "101"],
        vec!["--formats", "gif"],
        vec!["--jobs", "0"],
        vec!["--dry-run", "--report-file", "report.json"],
    ] {
        let out = bin().current_dir(d.path()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
    }
    let out = bin().arg(d.path().join("missing.png")).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(tree(d.path()).is_empty());
}
#[test]
fn malformed_input_is_reported_without_stopping_batch() {
    let d = TempDir::new().unwrap();
    fs::write(d.path().join("broken.png"), b"not png").unwrap();
    png(&d.path().join("good.png"));
    let out = bin()
        .arg(d.path())
        .args([
            "--min-bytes",
            "0",
            "--formats",
            "png",
            "--dry-run",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let r = json(&out);
    assert_eq!(r["summary"]["optimized"], 1);
    assert_eq!(r["summary"]["skipped"], 1);
    assert!(r["files"].as_array().unwrap().iter().any(|f| {
        f["reason"]
            .as_str()
            .is_some_and(|s| s.contains("unrecognized"))
    }));
}
#[test]
fn init_will_not_overwrite_and_template_parses() {
    let d = TempDir::new().unwrap();
    let p = d.path().join("tinyimg.toml");
    assert!(bin().arg("init").arg(&p).output().unwrap().status.success());
    assert!(tinyimg_core::config::Config::read(&p).is_ok());
    let bytes = fs::read(&p).unwrap();
    assert!(!bin().arg("init").arg(&p).output().unwrap().status.success());
    assert_eq!(fs::read(&p).unwrap(), bytes);
}
#[test]
fn explicit_cli_score_overrides_directory_config() {
    let d = TempDir::new().unwrap();
    png(&d.path().join("a.png"));
    fs::write(
        d.path().join("tinyimg.toml"),
        "score=90\nformats=['png']\n[[overrides]]\nglob='*.png'\nscore=99\n",
    )
    .unwrap();
    let out = bin()
        .arg("analyze")
        .arg(d.path())
        .args(["--score", "70", "--report", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = json(&out);
    assert!(!r["files"][0]["candidates"].as_array().unwrap().is_empty());
}
#[test]
fn refs_lists_literals_without_modifying_sources() {
    let d = TempDir::new().unwrap();
    fs::write(
        d.path().join("main.kt"),
        "val a=\"assets/icon.png\"; val b=R.drawable.icon",
    )
    .unwrap();
    let before = tree(d.path());
    let out = bin()
        .arg("refs")
        .arg(d.path())
        .args(["--report", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let r = json(&out);
    assert_eq!(r.as_array().unwrap().len(), 1);
    assert_eq!(r[0]["literal"], "assets/icon.png");
    assert_eq!(tree(d.path()), before);
}

#[test]
fn paths_with_unicode_and_spaces_preserve_structure() {
    let d = TempDir::new().unwrap();
    let input = d.path().join("图片 资源");
    let source = input.join("drawable-mdpi").join("图标 hello.png");
    png(&source);
    let old = fs::read(&source).unwrap();
    let output = d.path().join("输出 目录");
    let result = bin()
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&source).unwrap(), old);
    assert!(
        output
            .join("drawable-mdpi")
            .join("图标 hello.png")
            .is_file()
    );
}

#[test]
fn report_failure_is_preflighted_before_image_changes() {
    let d = TempDir::new().unwrap();
    let source = d.path().join("a.png");
    png(&source);
    let old = fs::read(&source).unwrap();
    let report = d.path().join("result.json");
    fs::write(&report, b"existing").unwrap();
    for path in [&report, &source] {
        let result = bin()
            .arg(&source)
            .arg("--report-file")
            .arg(path)
            .args(["--formats", "png"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(fs::read(&source).unwrap(), old);
    }
    assert_eq!(fs::read(&report).unwrap(), b"existing");
}

#[test]
fn output_or_backup_ancestor_input_is_an_error_not_empty_success() {
    let d = TempDir::new().unwrap();
    let source = d.path().join("input/a.png");
    png(&source);
    let old = fs::read(&source).unwrap();
    for option in ["--output", "--backup"] {
        let result = bin()
            .arg(&source)
            .arg(option)
            .arg(d.path())
            .args(["--formats", "png"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(fs::read(&source).unwrap(), old);
    }
}

#[test]
fn directory_override_filters_are_rejected_not_ignored() {
    let d = TempDir::new().unwrap();
    png(&d.path().join("a.png"));
    fs::write(
        d.path().join("tinyimg.toml"),
        "[[overrides]]\nglob='*.png'\nexclude=['*.png']\n",
    )
    .unwrap();
    let before = tree(d.path());
    let result = bin().arg(d.path()).output().unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(tree(d.path()), before);
}

#[cfg(unix)]
#[test]
fn non_utf8_filename_produces_lossless_json_after_optimization() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let d = TempDir::new().unwrap();
    let source = d.path().join(OsString::from_vec(b"icon-\xff.png".to_vec()));
    png(&source);
    let result = bin()
        .arg(&source)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = json(&result);
    assert_eq!(report["summary"]["optimized"], 1);
    assert_eq!(report["files"][0]["source"]["encoding"], "unix-bytes-hex");
    assert!(
        report["files"][0]["source"]["hex"]
            .as_str()
            .unwrap()
            .ends_with("69636f6e2dff2e706e67")
    );
}

#[cfg(unix)]
#[test]
fn explicit_output_root_alias_resolves_but_descendant_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let d = TempDir::new().unwrap();
    let input = d.path().join("input");
    png(&input.join("nested/a.png"));
    let actual = d.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let alias = d.path().join("alias");
    symlink(&actual, &alias).unwrap();
    let result = bin()
        .arg(&input)
        .arg("-o")
        .arg(alias.join("out"))
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(actual.join("out/nested/a.png").is_file());
    let output = d.path().join("second");
    fs::create_dir(&output).unwrap();
    let foreign = d.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    symlink(&foreign, output.join("nested")).unwrap();
    let result = bin()
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--formats", "png", "--report", "json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(!foreign.join("a.png").exists());
}

#[test]
fn processed_marker_prevents_repeat_work_and_readonly_checks_do_not_modify_it() {
    let d = TempDir::new().unwrap();
    let input = d.path().join("input");
    let source = input.join("nested/a.png");
    png(&source);
    let first = confirmed(bin().arg(&input).args([
        "--formats",
        "png",
        "--min-bytes",
        "0",
        "--report",
        "json",
    ]));
    assert!(first.status.success());
    assert_eq!(json(&first)["summary"]["optimized"], 1);
    assert!(input.join(".tinyimg-cache").is_dir());
    let after = tree(d.path());
    let second = bin()
        .arg(&source)
        .args([
            "--formats",
            "png",
            "--min-bytes",
            "0",
            "--check",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let result = json(&second);
    assert_eq!(result["summary"]["cache_hits"], 1);
    assert_eq!(tree(d.path()), after);
    let uncached = bin()
        .arg(&source)
        .args([
            "--formats",
            "png",
            "--min-bytes",
            "0",
            "--check",
            "--no-cache",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert!(uncached.status.success());
    let result = json(&uncached);
    assert_eq!(result["cache_enabled"], false);
    assert_eq!(result["summary"]["cache_hits"], 0);
    assert_eq!(tree(d.path()), after);
}

#[test]
fn output_markers_do_not_modify_input_tree_and_settings_changes_invalidate_them() {
    let d = TempDir::new().unwrap();
    let input = d.path().join("input");
    let source = input.join("a.png");
    png(&source);
    let before = tree(&input);
    let output = d.path().join("out");
    let first = bin()
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--formats", "png", "--min-bytes", "0", "--report", "json"])
        .output()
        .unwrap();
    assert!(first.status.success());
    assert_eq!(tree(&input), before);
    assert!(output.join(".tinyimg-cache").is_dir());
    let second = bin()
        .arg(&output)
        .args([
            "--formats",
            "png",
            "--min-bytes",
            "0",
            "--check",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(json(&second)["summary"]["cache_hits"], 1);
    let changed = bin()
        .arg(&output)
        .args([
            "--formats",
            "png",
            "--min-bytes",
            "0",
            "--score",
            "99",
            "--check",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert!(changed.status.success());
    assert_eq!(json(&changed)["summary"]["cache_hits"], 0);
}

#[test]
fn explicit_avif_output_is_real_and_external_avif_input_remains_skipped() {
    let d = TempDir::new().unwrap();
    let source = d.path().join("a.png");
    png(&source);
    let original = fs::read(&source).unwrap();
    let output = d.path().join("out");
    let result = bin()
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .args([
            "--formats",
            "avif",
            "--min-saving",
            "0",
            "--score",
            "50",
            "--report",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = json(&result);
    assert_eq!(report["summary"]["optimized"], 1);
    assert_eq!(report["files"][0]["encoder"], "ravif");
    assert!(report["files"][0]["score"].as_f64().unwrap() >= 50.0);
    let avif = output.join("a.avif");
    assert_eq!(
        image::guess_format(&fs::read(&avif).unwrap()).unwrap(),
        image::ImageFormat::Avif
    );
    assert_eq!(fs::read(&source).unwrap(), original);
    let again = bin()
        .arg("analyze")
        .arg(&avif)
        .args(["--min-bytes", "0", "--report", "json"])
        .output()
        .unwrap();
    assert!(again.status.success());
    assert_eq!(json(&again)["summary"]["skipped"], 1);
}

#[test]
fn ordinary_text_omits_preset_notice_but_verbose_and_json_keep_it() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("small.png"), b"tiny").unwrap();
    for flags in [vec![], vec!["-v"], vec!["--report", "json"], vec!["-q"]] {
        let output = bin()
            .arg(dir.path())
            .arg("--dry-run")
            .args(&flags)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.contains('\r') && !stderr.contains('\r'));
        assert!(!stdout.contains('\x1b') && !stderr.contains('\x1b'));
        if flags.contains(&"json") {
            let report = json(&output);
            assert_eq!(report["presets_calibrated"], false);
            assert!(
                report["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w.as_str().unwrap().contains("not visually calibrated"))
            );
        } else if flags.contains(&"-v") {
            assert!(stdout.contains("not visually calibrated"));
        } else {
            assert!(!stdout.contains("not visually calibrated"));
        }
        if flags.contains(&"-q") {
            assert!(stdout.is_empty() && stderr.is_empty());
        }
    }
}

#[test]
fn concurrent_progress_counts_skips_and_errors_once_without_changing_reports() {
    use std::sync::Mutex;
    use tinyimg_core::{
        Mode, ProgressEvent, RunOptions,
        config::{Config, SettingsPatch},
    };
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("small.png"), b"tiny").unwrap();
    fs::write(dir.path().join("invalid.png"), vec![0; 3000]).unwrap();
    let removed = dir.path().join("removed.png");
    fs::write(&removed, vec![0; 3000]).unwrap();
    let prepared = tinyimg_core::prepare(&RunOptions {
        inputs: vec![dir.path().to_path_buf()],
        config: Config::default(),
        cli_settings: SettingsPatch::default(),
        output: None,
        backup: None,
        mode: Mode::DryRun,
        jobs: 3,
        use_cache: false,
    })
    .unwrap();
    fs::remove_file(removed).unwrap();
    let events = Mutex::new(Vec::new());
    let mut report = prepared
        .run_with_progress(|event| {
            if let ProgressEvent::Complete { file_index, .. } = event {
                events.lock().unwrap().push(file_index);
            }
        })
        .unwrap();
    let mut completed = events.into_inner().unwrap();
    completed.sort_unstable();
    assert_eq!(completed, vec![0, 1, 2]);
    assert_eq!(report.summary.scanned, 3);
    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.skipped, 2);
    report
        .warnings
        .push("Reference scan incomplete: test failure".into());
    let ordinary = report.text(false);
    assert!(ordinary.contains("ERROR"));
    assert!(ordinary.contains("Reference scan incomplete: test failure"));
    assert!(!ordinary.contains("not visually calibrated"));
}

#[test]
fn human_text_sizes_and_savings_match_unchanged_json_bytes() {
    let dir = TempDir::new().unwrap();
    png(&dir.path().join("image.png"));
    let args = ["--dry-run", "--formats", "png", "--jobs", "1"];
    let structured = bin()
        .arg(dir.path())
        .args(args)
        .args(["--report", "json"])
        .output()
        .unwrap();
    assert!(structured.status.success());
    let report = json(&structured);
    let summary = &report["summary"];
    let expected = tinyimg_core::format_savings(
        summary["original_bytes"].as_u64().unwrap(),
        summary["optimized_bytes"].as_u64().unwrap(),
        summary["saving_bytes"].as_u64().unwrap(),
    );
    let text = bin().arg(dir.path()).args(args).output().unwrap();
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert_eq!(
        text.matches(&expected).count(),
        2,
        "file and total use the same sizes: {text}"
    );
    assert!(text.contains("KB"));
    assert!(text.contains("% ("));
    assert!(!text.contains(" bytes"));
}
