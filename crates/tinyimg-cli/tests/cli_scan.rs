use image::{
    ImageEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn png(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pixels = vec![120u8; 64 * 64 * 4];
    PngEncoder::new_with_quality(
        fs::File::create(path).unwrap(),
        CompressionType::Uncompressed,
        FilterType::NoFilter,
    )
    .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
    .unwrap();
}
fn run(cwd: &Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tinyimg"))
        .current_dir(cwd)
        .args(args)
        .args(["--report", "json", "--formats", "png", "--no-cache"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn relative_explicit_filenames_inside_target_release_are_not_silently_ignored() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().join("target/release");
    for name in ["编组 (2).png", "test.png"] {
        let source = cwd.join(name);
        png(&source);
        let before = fs::read(&source).unwrap();
        let report = run(&cwd, &[name]);
        assert_eq!(report["summary"]["scanned"], 1);
        assert_eq!(report["summary"]["optimized"], 1);
        assert!(fs::read(source).unwrap().len() < before.len());
    }
}
#[test]
fn chosen_directory_under_ignored_ancestor_scans_but_nested_build_trees_do_not() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().join("target/release");
    png(&cwd.join("test.png"));
    for child in ["target", ".git", "node_modules", ".tinyimg-cache"] {
        png(&cwd.join(child).join("nested.png"));
    }
    let report = run(&cwd, &[".", "--dry-run"]);
    assert_eq!(report["summary"]["scanned"], 1);
    assert_eq!(report["summary"]["optimized"], 1);
    let report = run(&cwd, &["target", "--dry-run"]);
    assert_eq!(report["summary"]["scanned"], 1);
}
#[test]
fn explicitly_filtered_file_reports_the_skip_reason() {
    let dir = TempDir::new().unwrap();
    png(&dir.path().join("test.png"));
    let before = fs::read(dir.path().join("test.png")).unwrap();
    for filters in [["--exclude", "test.png"], ["--include", "*.jpg"]] {
        let report = run(dir.path(), &["test.png", filters[0], filters[1]]);
        assert_eq!(report["summary"]["scanned"], 1);
        assert_eq!(report["summary"]["skipped"], 1);
        assert!(
            report["files"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("patterns")
        );
        assert_eq!(fs::read(dir.path().join("test.png")).unwrap(), before);
    }
}
