//! Conservative input discovery and bounded, read-only filename-reference checks.
//!
//! Walks never follow symlinks. Android resource groups retain every qualifier
//! except density, and their keys include the resource root to separate modules.

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Text files larger than this are deliberately outside the reference checker.
pub const MAX_REFERENCE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const IGNORED_DIRS: &[&str] = &[".git", "target", "node_modules", ".tinyimg-cache"];

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Output and backup directories, whether or not they exist yet.
    pub excluded_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanItem {
    #[serde(
        serialize_with = "crate::paths::serialize_path",
        deserialize_with = "crate::paths::deserialize_path"
    )]
    pub source: PathBuf,
    #[serde(
        serialize_with = "crate::paths::serialize_path",
        deserialize_with = "crate::paths::deserialize_path"
    )]
    pub relative_path: PathBuf,
    pub skip_reason: Option<String>,
    pub android: Option<AndroidResource>,
    pub is_asset: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AndroidResource {
    pub resource_name: String,
    pub resource_type: String,
    /// Directory qualifiers with density removed, e.g. `drawable-land-night-v21`.
    pub qualifier_group: String,
    pub density: Option<String>,
    /// Absolute resource root, non-density qualifiers, and resource name.
    pub group_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub items: Vec<ScanItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReferenceHit {
    #[serde(
        serialize_with = "crate::paths::serialize_path",
        deserialize_with = "crate::paths::deserialize_path"
    )]
    pub file: PathBuf,
    /// One-based line number.
    pub line: usize,
    pub literal: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rename {
    #[serde(
        serialize_with = "crate::paths::serialize_path",
        deserialize_with = "crate::paths::deserialize_path"
    )]
    pub old: PathBuf,
    #[serde(
        serialize_with = "crate::paths::serialize_path",
        deserialize_with = "crate::paths::deserialize_path"
    )]
    pub new: PathBuf,
}

#[derive(Debug)]
struct DiscoveredFile {
    path: PathBuf,
    relative: PathBuf,
    explicit: bool,
}

/// Discover images without modifying anything. PNG, JPEG and WebP are supported;
/// other common image formats and Android XML are reported with a skip reason.
/// A single directory retains its relative hierarchy. Multiple directory inputs
/// receive a root-name prefix; ambiguous destinations are rejected, never merged.
pub fn scan(inputs: &[PathBuf], options: &ScanOptions) -> Result<ScanResult> {
    let include = compile_globs(&options.include)?;
    let exclude = compile_globs(&options.exclude)?;
    // Manifest analysis must not be filtered by an image-only include pattern.
    let files = discover(inputs, &options.excluded_dirs, Some(&exclude))?;
    let launcher_resources = launcher_resources(&files, &options.excluded_dirs)?;
    let mut items = Vec::new();
    let mut destinations = BTreeMap::<PathBuf, PathBuf>::new();
    for file in files {
        if (!options.include.is_empty() && !matches(&include, &file.relative))
            || matches(&exclude, &file.relative)
        {
            continue;
        }
        let extension = extension(&file.path);
        let android = android_resource(&file.path);
        let is_android_xml = extension == "xml" && android.is_some();
        if !is_raster_extension(&extension) && !is_android_xml && !file.explicit {
            continue;
        }
        let resource_name = android.as_ref().map(|r| r.resource_name.as_str());
        let is_launcher = android.as_ref().is_some_and(|r| {
            r.resource_name.starts_with("ic_launcher")
                || launcher_resources.contains(&(r.resource_type.clone(), r.resource_name.clone()))
        });
        let skip_reason = if file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".9.png"))
        {
            Some("Android nine-patch images must remain byte-for-byte unchanged".into())
        } else if is_launcher {
            Some(format!(
                "launcher icon resource {} is protected",
                resource_name.unwrap_or_default()
            ))
        } else if is_android_xml {
            Some("Android XML resource (vector or non-raster drawable)".into())
        } else if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
            Some(format!(
                "unsupported image format: {}",
                if extension.is_empty() {
                    "(none)"
                } else {
                    &extension
                }
            ))
        } else {
            None
        };
        if let Some(previous) = destinations.insert(file.relative.clone(), file.path.clone()) {
            bail!(
                "input output-path collision at {} between {} and {}; use separate output directories or distinct root names",
                file.relative.display(),
                previous.display(),
                file.path.display()
            );
        }
        items.push(ScanItem {
            is_asset: file.path.components().any(|c| c.as_os_str() == "assets"),
            source: file.path,
            relative_path: file.relative,
            skip_reason,
            android,
        });
    }
    items.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(ScanResult { items })
}

/// List literal image filenames in supported UTF-8 source/text files no larger
/// than `MAX_REFERENCE_FILE_BYTES`. This is advisory, not a language parser:
/// dynamic/generated references, binary files, and larger files are not covered.
pub fn refs(inputs: &[PathBuf], excluded_dirs: &[PathBuf]) -> Result<Vec<ReferenceHit>> {
    let files = discover(inputs, excluded_dirs, None)?;
    // Include query/fragment suffixes, URLs, Windows separators and Unicode names.
    let image_literal = Regex::new(
        r#"(?i)[\p{L}\p{N}_./@%+~:\\\-]+\.(?:png|jpe?g|webp|avif|gif|bmp|tiff?)(?:[?#][\p{L}\p{N}_=&%+.,:/~\-]*)?"#,
    )?;
    let quoted_image_literal = Regex::new(
        r#"(?i)["']([^"'\r\n<>]*\.(?:png|jpe?g|webp|avif|gif|bmp|tiff?)(?:[?#][^"'\r\n<>]*)?)["']"#,
    )?;
    let mut hits = Vec::new();
    for file in files {
        if !is_text_extension(&extension(&file.path)) {
            continue;
        }
        let Some(text) = read_bounded_text(&file.path)? else {
            continue;
        };
        for (line_index, line) in text.lines().enumerate() {
            let quoted: Vec<_> = quoted_image_literal
                .captures_iter(line)
                .filter_map(|capture| capture.get(1))
                .collect();
            for literal in &quoted {
                hits.push(ReferenceHit {
                    file: file.path.clone(),
                    line: line_index + 1,
                    literal: literal.as_str().to_owned(),
                });
            }
            for literal in image_literal.find_iter(line) {
                if quoted.iter().any(|quoted| {
                    literal.start() >= quoted.start() && literal.end() <= quoted.end()
                }) {
                    continue;
                }
                if line[literal.end()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                hits.push(ReferenceHit {
                    file: file.path.clone(),
                    line: line_index + 1,
                    literal: literal.as_str().to_owned(),
                });
            }
        }
    }
    hits.sort_by(|a, b| (&a.file, a.line, &a.literal).cmp(&(&b.file, b.line, &b.literal)));
    hits.dedup();
    Ok(hits)
}

/// Conservatively identify filename literals affected by proposed renames.
/// Equal basenames also match because source-relative URL resolution varies by
/// framework. Only actual extension/name changes are considered.
pub fn find_rename_references(
    inputs: &[PathBuf],
    renames: &[Rename],
    excluded_dirs: &[PathBuf],
) -> Result<Vec<ReferenceHit>> {
    let changed_names: HashSet<String> = renames
        .iter()
        .filter(|r| r.old.file_name() != r.new.file_name())
        .filter_map(|r| {
            r.old
                .file_name()
                .and_then(|v| v.to_str())
                .map(str::to_owned)
        })
        .collect();
    if changed_names.is_empty() {
        return Ok(Vec::new());
    }
    Ok(refs(inputs, excluded_dirs)?
        .into_iter()
        .filter(|hit| {
            let literal = hit.literal.split(['?', '#']).next().unwrap_or(&hit.literal);
            let name = literal.rsplit(['/', '\\']).next().unwrap_or(literal);
            changed_names.contains(name)
        })
        .collect())
}

/// Find a bounded source tree for advisory reference scanning. This allows a
/// `src/main/res` input to find sibling Java/Kotlin/HTML/assets references. Only
/// eight ancestors are inspected, never the filesystem root or unrelated trees.
/// The nearest manifest directory or Git project boundary ends the search.
pub fn reference_roots(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for input in inputs {
        let canonical = fs::canonicalize(input).unwrap_or_else(|_| input.clone());
        let directory = if canonical.is_file() {
            canonical.parent().unwrap_or(&canonical).to_path_buf()
        } else {
            canonical
        };
        let mut git_root = None;
        let mut manifest_root = None;
        for ancestor in directory.ancestors().take(8) {
            if ancestor.parent().is_none() {
                break;
            }
            if regular_file(&ancestor.join("AndroidManifest.xml")) {
                manifest_root = Some(ancestor.to_path_buf());
                break;
            }
            if is_git_root(ancestor) {
                git_root = Some(ancestor.to_path_buf());
                break;
            }
        }
        let chosen = manifest_root.or(git_root).unwrap_or(directory);
        if !roots
            .iter()
            .any(|existing: &PathBuf| chosen.starts_with(existing))
        {
            roots.retain(|existing| !existing.starts_with(&chosen));
            roots.push(chosen);
        }
    }
    roots
}

fn is_git_root(path: &Path) -> bool {
    // An actual repository has HEAD; an empty .git directory is not sufficient.
    regular_file(&path.join(".git")) || regular_file(&path.join(".git/HEAD"))
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid path glob {pattern:?}"))?);
    }
    Ok(builder.build()?)
}

fn matches(globs: &GlobSet, path: &Path) -> bool {
    // Windows separators are normalized for portable config globs. A backslash
    // is a legitimate Unix filename byte and must remain literal there.
    #[cfg(windows)]
    {
        globs.is_match(path.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        globs.is_match(path)
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            value => normalized.push(value.as_os_str()),
        }
    }
    Ok(normalized)
}

fn excluded_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .map(Ok)
                .unwrap_or_else(|_| absolute_lexical(path))
        })
        .collect()
}

fn excluded(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    excluded_dirs.iter().any(|dir| path.starts_with(dir))
        || path.components().any(|component| {
            IGNORED_DIRS
                .iter()
                .any(|name| component.as_os_str() == *name)
        })
}

fn discover(
    inputs: &[PathBuf],
    excluded_dirs: &[PathBuf],
    exclude_globs: Option<&GlobSet>,
) -> Result<Vec<DiscoveredFile>> {
    if inputs.is_empty() {
        bail!("at least one input path is required");
    }
    let excluded_dirs = excluded_paths(excluded_dirs)?;
    let mut roots = Vec::new();
    let mut root_paths = HashSet::new();
    for input in inputs {
        let metadata = fs::symlink_metadata(input)
            .with_context(|| format!("cannot inspect input {}", input.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = fs::canonicalize(input)
            .with_context(|| format!("cannot resolve input {}", input.display()))?;
        if root_paths.insert(canonical.clone()) {
            roots.push((canonical, metadata.is_file()));
        }
    }
    let multi_root = roots.len() > 1;
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for (root, explicit) in roots {
        if excluded(&root, &excluded_dirs) {
            continue;
        }
        let base = if explicit {
            root.parent().unwrap_or(&root)
        } else {
            &root
        };
        let prefix = if multi_root && !explicit {
            PathBuf::from(
                root.file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("root")),
            )
        } else {
            PathBuf::new()
        };
        let relative =
            |path: &Path| -> PathBuf { prefix.join(path.strip_prefix(base).unwrap_or(path)) };
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                if entry.file_type().is_symlink() || excluded(entry.path(), &excluded_dirs) {
                    return false;
                }
                // Excluded directory globs prune entire trees. Include globs never do.
                !entry.file_type().is_dir()
                    || !exclude_globs.is_some_and(|globs| matches(globs, &relative(entry.path())))
            })
        {
            let entry =
                entry.with_context(|| format!("cannot traverse input {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            // Roots are canonical, and descendants cannot traverse symlinks.
            if seen.insert(path.clone()) {
                files.push(DiscoveredFile {
                    relative: relative(&path),
                    path,
                    explicit,
                });
            }
        }
    }
    Ok(files)
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn is_raster_extension(extension: &str) -> bool {
    matches!(
        extension,
        "png" | "jpg" | "jpeg" | "webp" | "avif" | "gif" | "bmp" | "tif" | "tiff"
    )
}

fn is_text_extension(extension: &str) -> bool {
    matches!(
        extension,
        "xml"
            | "html"
            | "htm"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "json"
            | "jsonc"
            | "md"
            | "mdx"
            | "txt"
            | "csv"
            | "yaml"
            | "yml"
            | "gradle"
            | "kts"
            | "kt"
            | "java"
            | "dart"
            | "swift"
            | "m"
            | "mm"
            | "c"
            | "cpp"
            | "cc"
            | "h"
            | "hpp"
            | "rs"
            | "toml"
            | "properties"
            | "svg"
            | "vue"
            | "svelte"
            | "php"
            | "rb"
            | "py"
            | "sh"
    )
}

fn read_bounded_text(path: &Path) -> Result<Option<String>> {
    let file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    if file.metadata()?.len() > MAX_REFERENCE_FILE_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_REFERENCE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REFERENCE_FILE_BYTES || bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

fn android_resource(path: &Path) -> Option<AndroidResource> {
    let directory = path.parent()?;
    let resource_root = directory.parent()?;
    if resource_root.file_name()? != "res" {
        return None;
    }
    let folder = directory.file_name()?.to_str()?;
    let mut qualifiers = folder.split('-');
    let resource_type = qualifiers.next()?;
    if !matches!(resource_type, "drawable" | "mipmap") {
        return None;
    }
    let mut density = None;
    let mut remaining = vec![resource_type];
    for qualifier in qualifiers {
        if matches!(
            qualifier,
            "ldpi"
                | "mdpi"
                | "tvdpi"
                | "hdpi"
                | "xhdpi"
                | "xxhdpi"
                | "xxxhdpi"
                | "nodpi"
                | "anydpi"
        ) || qualifier
            .strip_suffix("dpi")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            density = Some(qualifier.to_owned());
        } else {
            remaining.push(qualifier);
        }
    }
    let stem = path.file_stem()?.to_str()?;
    let resource_name = stem.strip_suffix(".9").unwrap_or(stem).to_owned();
    let qualifier_group = remaining.join("-");
    let group_key =
        crate::paths::path_key(&resource_root.join(&qualifier_group).join(&resource_name));
    Some(AndroidResource {
        resource_name,
        resource_type: resource_type.to_owned(),
        qualifier_group,
        density,
        group_key,
    })
}

fn launcher_resources(
    files: &[DiscoveredFile],
    excluded_dirs: &[PathBuf],
) -> Result<BTreeSet<(String, String)>> {
    let manifest_icon = Regex::new(
        r#"android:(?:icon|roundIcon)\s*=\s*["']@(?:[\w.]+:)?(drawable|mipmap)/([A-Za-z0-9_]+)["']"#,
    )?;
    let resource_reference = Regex::new(r"@(?:[\w.]+:)?(drawable|mipmap)/([A-Za-z0-9_]+)")?;
    let mut protected = BTreeSet::new();
    let mut graph = BTreeMap::<(String, String), BTreeSet<(String, String)>>::new();
    let mut inspect = BTreeSet::<PathBuf>::new();
    let mut resource_roots = BTreeSet::new();
    let excluded_dirs = excluded_paths(excluded_dirs)?;
    for file in files {
        if file
            .path
            .file_name()
            .is_some_and(|name| name == "AndroidManifest.xml")
        {
            inspect.insert(file.path.clone());
        }
        if android_resource(&file.path).is_some() {
            if extension(&file.path) == "xml" {
                inspect.insert(file.path.clone());
            }
            if let Some(root) = file.path.parent().and_then(Path::parent) {
                resource_roots.insert(root.to_path_buf());
            }
            // Inputs commonly point directly at res, below the manifest.
            for ancestor in file.path.ancestors().skip(1).take(8) {
                if ancestor.parent().is_none() {
                    break;
                }
                let manifest = ancestor.join("AndroidManifest.xml");
                if regular_file(&manifest) && !excluded(&manifest, &excluded_dirs) {
                    inspect.insert(manifest);
                    break;
                }
                // Do not look above a project boundary for an unrelated manifest.
                if is_git_root(ancestor) {
                    break;
                }
            }
        }
    }
    // A selected raster can be referenced by an adaptive icon XML in a sibling
    // density folder. Read only adjacent resource XML, without following links.
    for root in resource_roots {
        for directory in fs::read_dir(&root)
            .with_context(|| format!("cannot inspect Android resources {}", root.display()))?
        {
            let directory = directory?;
            if !directory.file_type()?.is_dir() || excluded(&directory.path(), &excluded_dirs) {
                continue;
            }
            let folder = directory.file_name();
            let folder = folder.to_string_lossy();
            if !matches!(folder.split('-').next(), Some("drawable" | "mipmap")) {
                continue;
            }
            for child in fs::read_dir(directory.path())? {
                let child = child?;
                if child.file_type()?.is_file()
                    && extension(&child.path()) == "xml"
                    && !excluded(&child.path(), &excluded_dirs)
                {
                    inspect.insert(child.path());
                }
            }
        }
    }
    for path in inspect {
        let is_manifest = path
            .file_name()
            .is_some_and(|name| name == "AndroidManifest.xml");
        let Some(text) = read_bounded_text(&path)? else {
            // Never silently miss launcher indirection in unreadable XML.
            bail!(
                "cannot safely inspect Android launcher references in {}: XML must be UTF-8 text no larger than {} bytes",
                path.display(),
                MAX_REFERENCE_FILE_BYTES
            );
        };
        if is_manifest {
            for capture in manifest_icon.captures_iter(&text) {
                protected.insert((capture[1].to_owned(), capture[2].to_owned()));
            }
        } else if let Some(resource) = android_resource(&path) {
            let refs = graph
                .entry((resource.resource_type, resource.resource_name))
                .or_default();
            for capture in resource_reference.captures_iter(&text) {
                refs.insert((capture[1].to_owned(), capture[2].to_owned()));
            }
        }
    }
    // Adaptive icons can reference raster foreground/background resources.
    loop {
        let mut expanded = protected.clone();
        for key in &protected {
            if let Some(children) = graph.get(key) {
                expanded.extend(children.iter().cloned());
            }
        }
        if expanded.len() == protected.len() {
            break;
        }
        protected = expanded;
    }
    Ok(protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn put(root: &Path, name: &str, contents: &str) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn android_groups_keep_non_density_qualifiers_and_modules() {
        let dir = TempDir::new().unwrap();
        for path in [
            "app/src/main/res/drawable-land-hdpi-night-v21/tile.png",
            "app/src/main/res/drawable-land-xhdpi-night-v21/tile.png",
            "app/src/main/res/drawable-hdpi-night-v21/tile.png",
            "lib/src/main/res/drawable-land-hdpi-night-v21/tile.png",
        ] {
            put(dir.path(), path, "image");
        }
        let result = scan(&[dir.path().to_owned()], &ScanOptions::default()).unwrap();
        let a = result
            .items
            .iter()
            .find(|i| {
                i.relative_path.to_string_lossy().contains("land-hdpi")
                    && i.relative_path.starts_with("app")
            })
            .unwrap()
            .android
            .as_ref()
            .unwrap();
        let b = result
            .items
            .iter()
            .find(|i| i.relative_path.to_string_lossy().contains("land-xhdpi"))
            .unwrap()
            .android
            .as_ref()
            .unwrap();
        assert_eq!(a.group_key, b.group_key);
        assert_eq!(a.qualifier_group, "drawable-land-night-v21");
        assert_eq!(a.density.as_deref(), Some("hdpi"));
        assert_eq!(
            result
                .items
                .iter()
                .map(|i| &i.android.as_ref().unwrap().group_key)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
    }

    #[test]
    fn protects_manifest_icons_adaptive_children_ninepatch_and_vectors() {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "app/src/main/AndroidManifest.xml",
            r#"<application android:icon="@mipmap/custom" android:roundIcon='@drawable/round_custom'/>"#,
        );
        put(
            dir.path(),
            "app/src/main/res/mipmap-anydpi-v26/custom.xml",
            r#"<adaptive-icon><foreground android:drawable="@drawable/logo_foreground" /></adaptive-icon>"#,
        );
        for name in [
            "drawable-hdpi/logo_foreground.png",
            "drawable/round_custom.png",
            "mipmap-xhdpi/custom.png",
            "mipmap-hdpi/ic_launcher.png",
            "drawable/panel.9.png",
            "drawable/normal.png",
        ] {
            put(dir.path(), &format!("app/src/main/res/{name}"), "image");
        }
        put(
            dir.path(),
            "app/src/main/res/drawable/vector.xml",
            "<vector />",
        );
        let result = scan(&[dir.path().to_owned()], &ScanOptions::default()).unwrap();
        assert_eq!(result.items.len(), 8);
        assert_eq!(
            result
                .items
                .iter()
                .filter(|i| i.skip_reason.is_none())
                .count(),
            1
        );
        assert_eq!(
            result
                .items
                .iter()
                .find(|i| i.skip_reason.is_none())
                .unwrap()
                .source
                .file_name()
                .unwrap(),
            "normal.png"
        );
    }

    #[test]
    fn filters_and_excluded_outputs_do_not_recurse() {
        let dir = TempDir::new().unwrap();
        for path in [
            "assets/keep.png",
            "assets/drop.jpg",
            "assets/ignore.png",
            "target/generated.png",
            "node_modules/a.png",
            ".git/a.png",
            ".tinyimg-cache/a.png",
            "out/nested/again.png",
            "backup/original.png",
        ] {
            put(dir.path(), path, "image");
        }
        let options = ScanOptions {
            include: vec!["**/*.png".into()],
            exclude: vec!["**/ignore.png".into()],
            excluded_dirs: vec![dir.path().join("out"), dir.path().join("backup")],
        };
        let result = scan(&[dir.path().to_owned()], &options).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].relative_path,
            PathBuf::from("assets/keep.png")
        );
        assert!(result.items[0].is_asset);
    }

    #[test]
    fn overlapping_roots_are_deduplicated_and_root_collisions_rejected() {
        let dir = TempDir::new().unwrap();
        let a = put(dir.path(), "left/images/icon.png", "image");
        let b = put(dir.path(), "right/images/icon.png", "image");
        assert_eq!(
            scan(&[a.clone(), a.clone()], &ScanOptions::default())
                .unwrap()
                .items
                .len(),
            1
        );
        assert!(
            scan(
                &[
                    a.parent().unwrap().to_owned(),
                    b.parent().unwrap().to_owned()
                ],
                &ScanOptions::default()
            )
            .is_err()
        );
        assert!(scan(&[a, b], &ScanOptions::default()).is_err());
    }

    #[test]
    fn reference_checks_cover_assets_not_android_resource_identifiers() {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "src/page.html",
            "<img src=\"assets/hero.png?v=1\">\n<div style=\"background:url('other.webp')\">",
        );
        put(
            dir.path(),
            "src/Main.kt",
            "val icon = R.drawable.hero\nval literal = \"file:///android_asset/hero.png\"",
        );
        put(dir.path(), "out/generated.html", "hero.png");
        put(dir.path(), "node_modules/vendor.js", "hero.png");
        let excluded = vec![dir.path().join("out")];
        let hits = refs(&[dir.path().to_owned()], &excluded).unwrap();
        assert_eq!(hits.len(), 3);
        let hits = find_rename_references(
            &[dir.path().to_owned()],
            &[Rename {
                old: PathBuf::from("assets/hero.png"),
                new: PathBuf::from("assets/hero.webp"),
            }],
            &excluded,
        )
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.literal.contains("hero.png")));
    }

    #[test]
    fn res_only_and_single_file_inputs_protect_sibling_manifest_icons() {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "app/src/main/AndroidManifest.xml",
            r#"<application android:icon="@mipmap/app_icon"/>"#,
        );
        put(
            dir.path(),
            "app/src/main/res/mipmap-anydpi-v26/app_icon.xml",
            r#"<foreground android:drawable="@drawable/foreground"/>"#,
        );
        let icon = put(
            dir.path(),
            "app/src/main/res/drawable-hdpi/foreground.png",
            "image",
        );
        put(
            dir.path(),
            "app/src/main/java/Main.kt",
            "val asset = \"foreground.png\"",
        );
        let res = dir.path().join("app/src/main/res");
        for input in [res.clone(), icon] {
            let result = scan(&[input], &ScanOptions::default()).unwrap();
            assert!(result.items.iter().all(|item| item.skip_reason.is_some()));
        }
        let roots = reference_roots(&[res]);
        assert_eq!(roots, vec![dir.path().join("app/src/main")]);
        assert_eq!(refs(&roots, &[]).unwrap().len(), 1);
    }

    #[test]
    fn reference_roots_prefer_manifest_otherwise_git_and_do_not_overlap() {
        let dir = TempDir::new().unwrap();
        put(dir.path(), ".git/HEAD", "ref: refs/heads/main");
        let input = put(dir.path(), "web/assets/icon.png", "image");
        assert_eq!(reference_roots(&[input]), vec![dir.path().to_owned()]);
        let without_git = TempDir::new().unwrap();
        let standalone = put(without_git.path(), "images/a.png", "image");
        assert_eq!(
            reference_roots(&[standalone]),
            vec![without_git.path().join("images")]
        );
    }

    #[test]
    fn references_keep_quoted_spaces_and_ignore_false_extension_suffixes() {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "page.html",
            r#"<img src="assets/hero image.png"><img src=assets/plain.png><p>config.pngx</p>"#,
        );
        let hits = refs(&[dir.path().to_owned()], &[]).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|hit| hit.literal == "assets/hero image.png")
        );
        let renamed = find_rename_references(
            &[dir.path().to_owned()],
            &[Rename {
                old: PathBuf::from("hero image.png"),
                new: PathBuf::from("hero image.webp"),
            }],
            &[],
        )
        .unwrap();
        assert_eq!(renamed.len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn unix_backslash_filenames_are_not_treated_as_directory_separators() {
        let dir = TempDir::new().unwrap();
        put(dir.path(), r"assets/a\b.png", "image");
        put(dir.path(), "assets/a/b.png", "image");
        let result = scan(
            &[dir.path().to_owned()],
            &ScanOptions {
                include: vec!["**/*.png".into()],
                exclude: vec!["assets/a/b.png".into()],
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].relative_path,
            PathBuf::from(r"assets/a\b.png")
        );
        let literal_only = scan(
            &[dir.path().to_owned()],
            &ScanOptions {
                include: vec![r"assets/a\\b.png".into()],
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(literal_only.items.len(), 1);
        assert_eq!(
            literal_only.items[0].relative_path,
            PathBuf::from(r"assets/a\b.png")
        );
    }

    #[test]
    #[cfg(unix)]
    fn android_group_keys_preserve_non_unicode_resource_roots() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = TempDir::new().unwrap();
        for suffix in [0xfe, 0xff] {
            let root = dir.path().join(OsString::from_vec(vec![b'm', suffix]));
            put(&root, "res/drawable-hdpi/tile.png", "image");
        }
        let result = scan(&[dir.path().to_owned()], &ScanOptions::default()).unwrap();
        assert_eq!(result.items.len(), 2);
        assert_ne!(
            result.items[0].android.as_ref().unwrap().group_key,
            result.items[1].android.as_ref().unwrap().group_key
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_files_directories_and_explicit_symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let image = put(outside.path(), "secret.png", "image");
        symlink(&image, dir.path().join("alias.png")).unwrap();
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        put(dir.path(), "real.png", "image");
        let result = scan(&[dir.path().to_owned()], &ScanOptions::default()).unwrap();
        assert_eq!(result.items.len(), 1);
        assert!(
            scan(&[dir.path().join("alias.png")], &ScanOptions::default())
                .unwrap()
                .items
                .is_empty()
        );
    }
}
