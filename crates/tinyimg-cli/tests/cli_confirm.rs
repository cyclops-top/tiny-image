use image::{
    ImageEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tinyimg"));
    command.args(["--formats", "png", "--report", "json", "--jobs", "1"]);
    command
}

fn png(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pixels = vec![120; 64 * 64 * 4];
    PngEncoder::new_with_quality(
        fs::File::create(path).unwrap(),
        CompressionType::Uncompressed,
        FilterType::NoFilter,
    )
    .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
    .unwrap();
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, base: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            out.insert(
                path.strip_prefix(base).unwrap().to_path_buf(),
                if path.is_dir() {
                    vec![]
                } else {
                    fs::read(&path).unwrap()
                },
            );
            if path.is_dir() {
                walk(&path, base, out);
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

fn answer(command: &mut Command, answer: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(answer).unwrap();
    child.wait_with_output().unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn prompted(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("Continue in place? [y/N]")
}

#[test]
fn negative_empty_eof_and_other_answers_cancel_without_any_mutations() {
    for response in [b"n\n".as_slice(), b"\n", b"", b"sure\n", b"yesterday\n"] {
        let dir = TempDir::new().unwrap();
        let input = dir.path().join("input");
        png(&input.join("a.png"));
        let before = tree(dir.path());
        let output = answer(
            bin()
                .arg(&input)
                .arg("--backup")
                .arg(dir.path().join("backup"))
                .arg("--report-file")
                .arg(dir.path().join("report.json")),
            response,
        );
        assert!(output.status.success());
        assert!(prompted(&output));
        assert!(!output.stderr.contains(&b'\r'));
        assert!(!output.stderr.contains(&0x1b));
        assert_eq!(json(&output)["cancelled"], true);
        assert_eq!(json(&output)["eligible_images"], 1);
        assert_eq!(tree(dir.path()), before);
    }
}

#[test]
fn affirmative_y_and_yes_proceed_and_keep_json_stdout_clean() {
    for response in [b"y\n".as_slice(), b" YES \n"] {
        let dir = TempDir::new().unwrap();
        let image = dir.path().join("a.png");
        png(&image);
        let before = fs::read(&image).unwrap();
        let output = answer(bin().arg(dir.path()), response);
        assert!(output.status.success());
        assert!(prompted(&output));
        assert_eq!(json(&output)["summary"]["optimized"], 1);
        assert!(fs::read(&image).unwrap().len() < before.len());
        assert!(dir.path().join(".tinyimg-cache").exists());
    }
}

#[test]
fn count_honors_filters_protected_formats_and_effective_minimum_size() {
    let dir = TempDir::new().unwrap();
    for name in ["eligible.png", "excluded.png", "nine.9.png", "override.png"] {
        png(&dir.path().join(name));
    }
    fs::write(dir.path().join("tiny.png"), b"tiny").unwrap();
    fs::write(dir.path().join("unsupported.gif"), b"GIF89a").unwrap();
    fs::write(
        dir.path().join("tinyimg.toml"),
        "exclude=['excluded.png']\n[[overrides]]\nglob='override.png'\nmin_bytes=1000000\n",
    )
    .unwrap();
    let before = tree(dir.path());
    let output = answer(bin().arg(dir.path()), b"n\n");
    assert!(output.status.success());
    assert_eq!(json(&output)["eligible_images"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Found 1 eligible image(s)"));
    assert_eq!(json(&output)["known_skipped"], 4);
    assert!(String::from_utf8_lossy(&output.stderr).contains("4 known skipped item(s)"));
    assert_eq!(tree(dir.path()), before);
}

#[test]
fn output_directory_bypasses_confirmation_and_preserves_sources() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("input");
    png(&input.join("a.png"));
    let before = tree(&input);
    let output = bin()
        .arg(&input)
        .arg("--output")
        .arg(dir.path().join("out"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!prompted(&output));
    assert_eq!(json(&output)["summary"]["optimized"], 1);
    assert_eq!(tree(&input), before);
    assert!(dir.path().join("out/a.png").exists());
}

#[test]
fn readonly_modes_bypass_confirmation_and_change_nothing() {
    let dir = TempDir::new().unwrap();
    png(&dir.path().join("a.png"));
    let before = tree(dir.path());
    for mode in ["--dry-run", "--check", "analyze"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tinyimg"));
        command.arg(mode).arg(dir.path()).args([
            "--formats",
            "png",
            "--report",
            "json",
            "--jobs",
            "1",
        ]);
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if mode == "--check" { 3 } else { 0 })
        );
        assert!(!prompted(&output));
        assert_eq!(json(&output)["summary"]["optimized"], 1);
        assert_eq!(tree(dir.path()), before);
    }
}

#[test]
fn only_explicit_files_bypass_confirmation() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.png");
    let b = dir.path().join("b.png");
    png(&a);
    png(&b);
    let output = bin().arg(a).arg(b).output().unwrap();
    assert!(output.status.success());
    assert!(!prompted(&output));
    assert_eq!(json(&output)["summary"]["optimized"], 2);
}

#[test]
fn mixed_directory_and_file_inputs_require_confirmation() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("input");
    let single = dir.path().join("single.png");
    png(&input.join("a.png"));
    png(&single);
    let before = tree(dir.path());
    let output = answer(bin().arg(input).arg(single), b"n\n");
    assert!(output.status.success());
    assert!(prompted(&output));
    assert_eq!(json(&output)["eligible_images"], 2);
    assert_eq!(tree(dir.path()), before);
}

#[test]
fn no_eligible_images_needs_no_confirmation_or_cache() {
    let dir = TempDir::new().unwrap();
    png(&dir.path().join("nine.9.png"));
    fs::write(dir.path().join("small.png"), b"tiny").unwrap();
    let before = tree(dir.path());
    let output = bin().arg(dir.path()).output().unwrap();
    assert!(output.status.success());
    assert!(!prompted(&output));
    assert_eq!(json(&output)["summary"]["optimized"], 0);
    assert_eq!(tree(dir.path()), before);
}

#[test]
fn default_current_directory_and_quiet_mode_still_require_confirmation() {
    let dir = TempDir::new().unwrap();
    png(&dir.path().join("a.png"));
    let before = tree(dir.path());
    let output = bin()
        .current_dir(dir.path())
        .arg("--quiet")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(prompted(&output));
    assert_eq!(json(&output)["cancelled"], true);
    assert_eq!(tree(dir.path()), before);
}

#[test]
fn execution_uses_the_confirmed_scan_without_adding_later_images() {
    let dir = TempDir::new().unwrap();
    let first = dir.path().join("a.png");
    png(&first);
    let original = fs::read(&first).unwrap();
    let mut child = bin()
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut prompt = Vec::new();
    let mut byte = [0];
    while !prompt.ends_with(b"[y/N] ") {
        assert_eq!(child.stderr.as_mut().unwrap().read(&mut byte).unwrap(), 1);
        prompt.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&prompt).contains("Found 1 eligible image(s)"));
    assert!(!prompt.contains(&b'\r'));
    assert!(!prompt.contains(&0x1b));
    // The prompt must precede even the first image/cache write.
    assert_eq!(fs::read(&first).unwrap(), original);
    assert!(!dir.path().join(".tinyimg-cache").exists());
    let later = dir.path().join("later.png");
    png(&later);
    let untouched = fs::read(&later).unwrap();
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(json(&output)["summary"]["scanned"], 1);
    assert_eq!(json(&output)["summary"]["optimized"], 1);
    assert_eq!(fs::read(later).unwrap(), untouched);
}
