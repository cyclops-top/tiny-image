//! Recognition of exact previously published outputs, not a cache of candidates.
//!
//! A marker proves only that this local cache recorded the exact output hash with
//! the same effective settings and engine revision. It is not a fresh quality
//! measurement. No image bytes, paths, or quality scores are loaded from markers.
//! The caller records a marker only after its verified writer reports success.
//!
//! Invalid/unreadable markers are advisory errors: the caller must warn and
//! recompute, never fail the image operation or remove/replace cache entries.
//! Cache writes use random temporary files and atomic no-clobber publication.
//! Like the portable image writer, this is not a security boundary against a
//! hostile process replacing directories between filesystem calls. Do not use
//! destructive image operations in directories writable by untrusted users.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, Metadata},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub const DIRECTORY: &str = ".tinyimg-cache";
const SCHEMA: u32 = 1;
const MAX_MARKER_BYTES: u64 = 4096;
// Bump the revision whenever encoding/selection/verification behavior changes,
// including changes to encoder dependencies or their build features. Package
// version invalidates released versions; this explicit revision covers dev builds.
const ENGINE: &str = concat!(
    "tinyimg/",
    env!("CARGO_PKG_VERSION"),
    ";processed-output-v1;oxipng=10.2.1;imagequant=4.4.1;",
    "webp=0.3.1;libwebp-sys=0.9.6;mozjpeg=0.10.13;mozjpeg-sys=2.2.3;",
    "ssimulacra2=0.5.1;image=0.25.10;ravif=0.13.0-no-default-features;",
    "avif-decode=3.0.0-vendored;rav1d=1.1.0-no-asm-bitdepth8-bitdepth16-threads1"
);

/// Candidate roots for read-only lookup, nearest first. Up to eight source
/// ancestors are checked so a file selected on a later run can find a marker
/// written when its containing directory was selected. The explicitly selected
/// input root is also included if deeper than this bound. No directory is created
/// and no inferred ancestor may be used as a write root. Paths come from the
/// scanner and must already be absolute/resolved; filesystem roots are excluded.
pub fn lookup_roots(source: &Path, selected_input_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = source
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take(8)
        .filter(|root| root.is_absolute() && root.parent().is_some())
        .map(Path::to_path_buf)
        .collect();
    if let Some(root) = selected_input_root
        && root.is_absolute()
        && root.parent().is_some()
        && source.starts_with(root)
        && !roots.iter().any(|candidate| candidate == root)
    {
        roots.push(root.to_path_buf());
    }
    roots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// A deterministic key built from trusted in-memory data, never cache input.
#[derive(Debug, Clone)]
pub struct OutputKey {
    marker: Marker,
    filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    schema: u32,
    engine: String,
    settings_hash: String,
    output_hash: String,
    output_bytes: u64,
}

impl OutputKey {
    /// Pass resolved OptimizeOptions and effective min_bytes, not SettingsPatch.
    /// `output_hash` must come from the exact bytes read by SourceSnapshot (lookup)
    /// or successfully published by the verified writer (record).
    pub fn new(
        output_hash: blake3::Hash,
        output_bytes: u64,
        effective_options: &impl Serialize,
        min_bytes: u64,
    ) -> Result<Self> {
        Self::with_engine(
            output_hash,
            output_bytes,
            effective_options,
            min_bytes,
            ENGINE,
        )
    }

    fn with_engine(
        output_hash: blake3::Hash,
        output_bytes: u64,
        effective_options: &impl Serialize,
        min_bytes: u64,
        engine: &str,
    ) -> Result<Self> {
        let settings = serde_json::to_vec(&(effective_options, min_bytes))
            .context("serialize effective cache settings")?;
        let marker = Marker {
            schema: SCHEMA,
            engine: engine.to_owned(),
            settings_hash: blake3::hash(&settings).to_hex().to_string(),
            output_hash: output_hash.to_hex().to_string(),
            output_bytes,
        };
        let serialized = serde_json::to_vec(&marker)?;
        ensure!(
            serialized.len() as u64 <= MAX_MARKER_BYTES,
            "cache marker too large"
        );
        let filename = format!("{}.json", blake3::hash(&serialized).to_hex());
        Ok(Self { marker, filename })
    }
}

#[derive(Debug, Clone)]
pub struct ProcessedCache {
    root: PathBuf,
    directory: PathBuf,
    access: Access,
}

impl ProcessedCache {
    /// `root` is an already resolved, absolute selected input/output directory.
    /// Construction performs no I/O. It must not canonicalize .tinyimg-cache:
    /// doing so could turn a malicious cache symlink into an accepted directory.
    /// An automatic cache is never created at a filesystem root.
    pub fn new(root: &Path, access: Access) -> Result<Self> {
        ensure!(
            root.is_absolute(),
            "cache root must be an absolute selected directory"
        );
        ensure!(
            root.parent().is_some(),
            "refusing automatic cache at filesystem root"
        );
        ensure!(
            root.components()
                .all(|c| !matches!(c, Component::CurDir | Component::ParentDir)),
            "cache root must already be resolved"
        );
        Ok(Self {
            root: root.to_owned(),
            directory: root.join(DIRECTORY),
            access,
        })
    }

    /// Read only. Missing cache/marker is an ordinary miss. Corrupt, unsafe, or
    /// unreadable entries return an advisory error so the caller can warn/recompute.
    pub fn contains(&self, key: &OutputKey) -> Result<bool> {
        if !safe_directory_chain(&self.directory, true)? {
            return Ok(false);
        }
        let path = self.directory.join(&key.filename);
        let before = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("inspect cache marker"),
        };
        check_marker_metadata(&before)?;
        let mut file = File::open(&path).context("open cache marker")?;
        let opened = file.metadata()?;
        check_marker_metadata(&opened)?;
        let opened_identity = same_file::Handle::from_file(file.try_clone()?)?;
        let path_identity = same_file::Handle::from_path(&path)?;
        ensure!(
            opened_identity == path_identity,
            "cache marker changed while opening"
        );
        check_marker_metadata(&fs::symlink_metadata(&path)?)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_MARKER_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_MARKER_BYTES,
            "cache marker exceeds size limit"
        );
        let after = file.metadata()?;
        let current = fs::symlink_metadata(&path)?;
        check_marker_metadata(&current)?;
        ensure!(
            opened.len() == after.len()
                && after.len() == current.len()
                && bytes.len() as u64 == after.len()
                && opened.modified().ok() == after.modified().ok()
                && after.modified().ok() == current.modified().ok()
                && opened_identity == same_file::Handle::from_path(&path)?,
            "cache marker changed while reading"
        );
        safe_directory_chain(&self.directory, false)?;
        let marker: Marker = serde_json::from_slice(&bytes).context("invalid cache marker")?;
        ensure!(
            marker == key.marker,
            "cache marker content does not match its key"
        );
        Ok(true)
    }

    /// Call only after successful image publication. Never overwrites a marker,
    /// follows a cache link, creates the selected root, or writes image bytes.
    /// An advisory failure does not undo or invalidate the completed image write.
    pub fn record(&self, key: &OutputKey) -> Result<()> {
        ensure!(self.access == Access::ReadWrite, "cache is read-only");
        // The image writer must have created the selected output root already.
        safe_directory_chain(&self.root, false)?;
        match fs::create_dir(&self.directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create processed-output cache"),
        }
        safe_directory_chain(&self.directory, false)?;
        if self.contains(key)? {
            return Ok(());
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".pending-")
            .suffix(".tmp")
            .tempfile_in(&self.directory)
            .context("create cache temporary file")?;
        let bytes = serde_json::to_vec(&key.marker)?;
        temporary.write_all(&bytes)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        safe_directory_chain(&self.directory, false)?;
        match temporary.persist_noclobber(self.directory.join(&key.filename)) {
            Ok(file) => drop(file),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another worker may have written the identical marker. Retain
                // any different/corrupt entry and let the caller report a warning.
                ensure!(
                    self.contains(key)?,
                    "cache marker disappeared after collision"
                );
                return Ok(());
            }
            Err(error) => {
                return Err(error.error).context("publish cache marker without overwriting");
            }
        }
        #[cfg(unix)]
        {
            safe_directory_chain(&self.directory, false)?;
            File::open(&self.directory)?.sync_all()?;
        }
        Ok(())
    }
}

fn check_marker_metadata(metadata: &Metadata) -> Result<()> {
    ensure!(
        !is_link_like(metadata),
        "refusing symlink/reparse cache marker"
    );
    ensure!(metadata.is_file(), "cache marker is not a regular file");
    ensure!(
        metadata.len() <= MAX_MARKER_BYTES,
        "cache marker exceeds size limit"
    );
    Ok(())
}

/// Inspect the whole lexical path, including the cache directory, without
/// canonicalizing or following any link. `false` means an ordinary missing path.
fn safe_directory_chain(path: &Path, allow_missing: bool) -> Result<bool> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir),
            "unresolved cache ancestor"
        );
        cursor.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&cursor) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect cache directory {}", cursor.display()));
            }
        };
        ensure!(
            !is_link_like(&metadata),
            "refusing symlink/reparse cache directory {}",
            cursor.display()
        );
        ensure!(
            metadata.is_dir(),
            "cache ancestor is not a directory: {}",
            cursor.display()
        );
    }
    Ok(true)
}

fn is_link_like(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ProcessedCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache =
            ProcessedCache::new(&dir.path().canonicalize().unwrap(), Access::ReadWrite).unwrap();
        (dir, cache)
    }

    fn key(bytes: &[u8], options: u32) -> OutputKey {
        OutputKey::new(blake3::hash(bytes), bytes.len() as u64, &options, 2048).unwrap()
    }

    #[test]
    fn lookup_checks_nearby_ancestors_and_an_explicit_deeper_root_without_writes() {
        let (_dir, cache) = fixture();
        let source = cache.root.join("a/b/c/d/e/f/g/h/i/j/image.webp");
        let roots = lookup_roots(&source, Some(&cache.root));
        assert_eq!(roots.len(), 9);
        assert_eq!(roots[0], source.parent().unwrap());
        assert_eq!(roots.last().unwrap(), &cache.root);
        assert!(!cache.root.join("a").exists());
        assert!(lookup_roots(Path::new("relative.png"), None).is_empty());
    }

    #[test]
    fn lookup_finds_directory_marker_when_later_selecting_individual_output() {
        let (_dir, cache) = fixture();
        let source = cache.root.join("res/drawable/icon.webp");
        let k = key(b"output", 1);
        cache.record(&k).unwrap();
        assert!(lookup_roots(&source, None).into_iter().any(|root| {
            ProcessedCache::new(&root, Access::ReadOnly)
                .unwrap()
                .contains(&k)
                .unwrap()
        }));
        assert!(!cache.root.join("res").exists());
    }

    #[test]
    fn lookup_is_read_only_even_when_root_is_missing() {
        let (dir, _) = fixture();
        let root = dir.path().canonicalize().unwrap().join("not-created");
        let cache = ProcessedCache::new(&root, Access::ReadOnly).unwrap();
        assert!(!cache.contains(&key(b"output", 1)).unwrap());
        assert!(!root.exists());
        assert!(cache.record(&key(b"output", 1)).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn readwrite_lookup_does_not_create_cache() {
        let (_dir, cache) = fixture();
        assert!(!cache.contains(&key(b"output", 1)).unwrap());
        assert!(!cache.directory.exists());
    }

    #[test]
    fn exact_output_and_effective_options_hit_without_stored_image_bytes() {
        let (_dir, cache) = fixture();
        let k = key(b"verified candidate bytes", 1);
        cache.record(&k).unwrap();
        assert!(cache.contains(&k).unwrap());
        assert!(!cache.contains(&key(b"source original bytes", 1)).unwrap());
        assert!(
            !cache
                .contains(&key(b"verified candidate bytes", 2))
                .unwrap()
        );
        let bytes = fs::read(cache.directory.join(k.filename)).unwrap();
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("verified candidate bytes")
        );
    }

    #[test]
    fn version_and_minimum_size_invalidate() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        cache.record(&k).unwrap();
        let other_version =
            OutputKey::with_engine(blake3::hash(b"output"), 6, &1u32, 2048, "future").unwrap();
        let other_min = OutputKey::new(blake3::hash(b"output"), 6, &1u32, 2049).unwrap();
        let other_len = OutputKey::new(blake3::hash(b"output"), 7, &1u32, 2048).unwrap();
        assert!(!cache.contains(&other_version).unwrap());
        assert!(!cache.contains(&other_min).unwrap());
        assert!(!cache.contains(&other_len).unwrap());
    }

    #[test]
    fn read_only_access_cannot_write_even_existing_cache() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        cache.record(&k).unwrap();
        let ro = ProcessedCache::new(&cache.root, Access::ReadOnly).unwrap();
        assert!(ro.contains(&k).unwrap());
        assert!(ro.record(&key(b"new", 2)).is_err());
        assert_eq!(fs::read_dir(&cache.directory).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_marker_fails_open_without_overwrite_or_delete() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        fs::create_dir(&cache.directory).unwrap();
        let path = cache.directory.join(&k.filename);
        fs::write(&path, b"not JSON").unwrap();
        assert!(cache.contains(&k).is_err());
        assert!(cache.record(&k).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not JSON");
        assert_eq!(fs::read_dir(&cache.directory).unwrap().count(), 1);
    }

    #[test]
    fn unknown_fields_wrong_hash_and_schema_are_not_hits() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        cache.record(&k).unwrap();
        let path = cache.directory.join(&k.filename);
        for field in ["schema", "output_hash", "unknown_field"] {
            let mut marker = serde_json::to_value(&k.marker).unwrap();
            marker[field] = serde_json::json!(999);
            fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
            assert!(cache.contains(&k).is_err());
        }
    }

    #[test]
    fn huge_marker_is_rejected_before_read() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        fs::create_dir(&cache.directory).unwrap();
        File::create(cache.directory.join(k.filename))
            .unwrap()
            .set_len(MAX_MARKER_BYTES + 1)
            .unwrap();
        assert!(
            cache
                .contains(&key(b"output", 1))
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
    }

    #[test]
    fn repeated_and_parallel_records_are_atomic_and_leave_no_temps() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        std::thread::scope(|scope| {
            for _ in 0..12 {
                scope.spawn(|| cache.record(&k).unwrap());
            }
        });
        assert!(cache.contains(&k).unwrap());
        assert_eq!(fs::read_dir(&cache.directory).unwrap().count(), 1);
    }

    #[test]
    fn record_never_creates_missing_selected_root() {
        let (_dir, cache) = fixture();
        let missing = cache.root.join("missing");
        let absent = ProcessedCache::new(&missing, Access::ReadWrite).unwrap();
        assert!(absent.record(&key(b"output", 1)).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn refuses_relative_traversal_and_filesystem_roots() {
        assert!(ProcessedCache::new(Path::new("relative"), Access::ReadWrite).is_err());
        #[cfg(unix)]
        {
            assert!(ProcessedCache::new(Path::new("/"), Access::ReadWrite).is_err());
            assert!(ProcessedCache::new(Path::new("/tmp/a/../b"), Access::ReadWrite).is_err());
        }
    }

    #[test]
    fn directory_as_marker_is_rejected() {
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        fs::create_dir_all(cache.directory.join(&k.filename)).unwrap();
        assert!(cache.contains(&k).is_err());
        assert!(cache.record(&k).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_cache_directory_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;
        let (_dir, cache) = fixture();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), &cache.directory).unwrap();
        let k = key(b"output", 1);
        assert!(cache.contains(&k).is_err());
        assert!(cache.record(&k).is_err());
        assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_marker_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;
        let (_dir, cache) = fixture();
        let k = key(b"output", 1);
        fs::create_dir(&cache.directory).unwrap();
        let victim = cache.root.join("image.png");
        let bytes = serde_json::to_vec(&k.marker).unwrap();
        fs::write(&victim, &bytes).unwrap();
        symlink(&victim, cache.directory.join(&k.filename)).unwrap();
        assert!(cache.contains(&k).is_err());
        assert!(cache.record(&k).is_err());
        assert_eq!(fs::read(victim).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_ancestor() {
        use std::os::unix::fs::symlink;
        let (_dir, cache) = fixture();
        let real = cache.root.join("real");
        fs::create_dir(&real).unwrap();
        let alias = cache.root.join("alias");
        symlink(&real, &alias).unwrap();
        let through_link = ProcessedCache::new(&alias, Access::ReadWrite).unwrap();
        assert!(through_link.contains(&key(b"output", 1)).is_err());
        assert!(through_link.record(&key(b"output", 1)).is_err());
        assert_eq!(fs::read_dir(real).unwrap().count(), 0);
    }
}
