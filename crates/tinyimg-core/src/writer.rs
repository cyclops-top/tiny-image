//! Conservative publication of already-verified image bytes.
//!
//! The caller must decode/verify the candidate before calling [`write_candidate`].
//! Temporary files live beside their destinations, are flushed and synced before
//! publication, and are removed on errors. Existing destinations and backups are
//! never overwritten, except for an explicitly requested same-path replacement.
//!
//! This is a portable filesystem writer, not a security boundary against another
//! process maliciously replacing directories between syscalls. It rejects existing
//! symlink ancestors, checks source identity/content immediately before mutation,
//! and uses atomic no-clobber publication for new paths. Portable filesystem APIs
//! cannot provide atomic compare-and-replace/unlink or eliminate every TOCTOU race.
//! Do not run destructive operations in directories writable by untrusted users.

use anyhow::{Context, Result, anyhow, bail, ensure};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;
use tempfile::{Builder, NamedTempFile};

/// Hard cap on the encoded bytes read for a source image.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// A checked read of a regular, non-symlink source file.
#[derive(Debug)]
pub struct SourceSnapshot {
    bytes: Vec<u8>,
    hash: blake3::Hash,
    identity: Identity,
    permissions: Permissions,
}

impl SourceSnapshot {
    pub fn capture(path: &Path) -> Result<Self> {
        let path = checked_path(path)?;
        let before = regular_metadata(&path)?;
        ensure!(
            before.len() <= MAX_SOURCE_BYTES,
            "source exceeds the 256 MiB encoded-file limit: {}",
            path.display()
        );
        let mut file =
            File::open(&path).with_context(|| format!("cannot read source {}", path.display()))?;
        let opened = file.metadata()?;
        ensure!(
            same_identity(&before, &opened),
            "source changed while opening {}",
            path.display()
        );
        // Recheck the path before reading from the open handle. This also catches
        // an ordinary symlink swap during open without trusting its target.
        let current = regular_metadata(&path)?;
        ensure!(
            same_identity(&opened, &current),
            "source changed while opening {}",
            path.display()
        );
        ensure!(
            opened.len() <= MAX_SOURCE_BYTES,
            "source exceeds the 256 MiB encoded-file limit: {}",
            path.display()
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_SOURCE_BYTES,
            "source exceeds the 256 MiB encoded-file limit: {}",
            path.display()
        );
        let after = file.metadata()?;
        let current = regular_metadata(&path)?;
        ensure!(
            stable_metadata(&opened, &after)
                && stable_metadata(&after, &current)
                && bytes.len() as u64 == after.len(),
            "source changed while reading {}",
            path.display()
        );
        let identity = Identity::from_file(file)?;
        let path_identity = Identity::from_file(File::open(&path)?)?;
        regular_metadata(&path)?;
        ensure!(
            identity == path_identity,
            "source replaced while reading {}",
            path.display()
        );
        Ok(Self {
            hash: blake3::hash(&bytes),
            bytes,
            identity,
            permissions: after.permissions(),
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn hash(&self) -> blake3::Hash {
        self.hash
    }
}

#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub bytes_written: u64,
    pub source_removed: bool,
    pub backup: Option<PathBuf>,
}

/// Validate output/backup paths without creating directories or writing bytes.
/// Suitable for dry-run and check-mode diagnostics.
pub fn preflight_target(
    source: &Path,
    target: &Path,
    backup: Option<&Path>,
    preserve_source: bool,
) -> Result<()> {
    let source = checked_path(source)?;
    let target = checked_path(target)?;
    let metadata = regular_metadata(&source)?;
    let inplace = source == target;
    ensure!(
        !(inplace && preserve_source),
        "preserve-source output must be different from the source"
    );
    if !preserve_source {
        ensure!(
            !metadata.permissions().readonly(),
            "refusing to replace or remove read-only source {}",
            source.display()
        );
        // Opening for write checks access but never truncates or modifies bytes.
        OpenOptions::new()
            .write(true)
            .open(&source)
            .with_context(|| format!("source is not writable: {}", source.display()))?;
        regular_metadata(&source)?;
    }
    check_no_collision(&target, inplace.then_some(source.as_path()))?;
    if let Some(backup) = backup {
        let backup = checked_path(backup)?;
        ensure!(
            !same_spelling_folded(&backup, &source) && !same_spelling_folded(&backup, &target),
            "backup must be different from source and target"
        );
        check_no_collision(&backup, None)?;
    }
    Ok(())
}

/// Publish a candidate that the caller has already decoded and verified.
///
/// `expected_source_hash` must be the hash of the exact original bytes used to
/// generate the candidate. A different target uses no-clobber publication. Unless
/// `preserve_source` is true, the original is removed only after publication and a
/// second content/identity check. Failed second-stage operations attempt a safe
/// rollback and explicitly identify any output/backup left for recovery.
///
/// Missing parent directories are created without traversing symlinks.
/// Backups contain the original bytes;
/// they are never overwritten and may remain after an otherwise failed operation.
/// File permission bits are preserved, not ownership, ACLs, xattrs or timestamps.
pub fn write_candidate(
    source: &Path,
    target: &Path,
    expected_source_hash: blake3::Hash,
    verified_bytes: &[u8],
    backup: Option<&Path>,
    preserve_source: bool,
) -> Result<WriteOutcome> {
    write_with_hook(
        source,
        target,
        expected_source_hash,
        verified_bytes,
        backup,
        preserve_source,
        &mut |_| Ok(()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    BeforePublish,
    AfterPublish,
}

#[allow(clippy::too_many_arguments)]
fn write_with_hook(
    source: &Path,
    target: &Path,
    expected_source_hash: blake3::Hash,
    verified_bytes: &[u8],
    backup: Option<&Path>,
    preserve_source: bool,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<WriteOutcome> {
    ensure!(
        !verified_bytes.is_empty(),
        "refusing to publish an empty candidate"
    );
    preflight_target(source, target, backup, preserve_source)?;
    let source = checked_path(source)?;
    let target = checked_path(target)?;
    let backup = backup.map(checked_path).transpose()?;
    let inplace = source == target;
    ensure!(
        !(inplace && preserve_source),
        "preserve-source output must be different from the source"
    );
    let original = SourceSnapshot::capture(&source)?;
    ensure!(
        original.hash == expected_source_hash,
        "source changed since optimization: {}",
        source.display()
    );
    if !preserve_source {
        check_writable_source(&source, &original)?;
    }
    check_no_collision(&target, inplace.then_some(source.as_path()))?;
    if let Some(backup) = &backup {
        ensure!(
            !same_spelling_folded(backup, &source) && !same_spelling_folded(backup, &target),
            "backup must be different from source and target"
        );
        check_no_collision(backup, None)?;
    }

    create_safe_parents(&target)?;
    if let Some(path) = &backup {
        create_safe_parents(path)?;
    }
    let candidate = prepare_temp(&target, verified_bytes, &original.permissions)?;
    let mut backup_created = false;
    if let Some(backup) = &backup {
        let copy = prepare_temp(backup, &original.bytes, &original.permissions)?;
        check_source(&source, &original, !preserve_source)?;
        check_no_collision(backup, None)?;
        let saved_backup = persist_new(copy, backup)?;
        backup_created = true;
        #[cfg(windows)]
        saved_backup.set_permissions(original.permissions.clone()).with_context(|| {
            format!("backup exists at {}, but restoring its permissions failed; source is unchanged", backup.display())
        })?;
        drop(saved_backup);
        sync_parent(backup).with_context(|| {
            format!(
                "backup exists at {}, but syncing its directory failed; source is unchanged",
                backup.display()
            )
        })?;
    }
    let recovery = || {
        if backup_created {
            format!(
                "; original backup remains at {}",
                backup.as_ref().unwrap().display()
            )
        } else {
            String::new()
        }
    };

    (|| -> Result<()> {
        hook(Stage::BeforePublish)?;
        check_source(&source, &original, !preserve_source)?;
        check_no_collision(&target, inplace.then_some(source.as_path()))?;
        Ok(())
    })()
    .with_context(|| {
        format!(
            "publication cancelled; source was not replaced{}",
            recovery()
        )
    })?;

    if inplace {
        // rename/replace is atomic on supported local filesystems. The destination
        // is only the original source whose identity/content we just checked.
        #[cfg(windows)]
        let permissions = original.permissions.clone();
        drop(original);
        let installed = candidate.persist(&target, false).map_err(|error| {
            anyhow!(
                "could not atomically replace {}: {}{}",
                target.display(),
                error,
                recovery()
            )
        })?;
        #[cfg(windows)]
        installed.set_permissions(permissions).with_context(|| {
            format!(
                "verified output was installed at {}, but restoring permissions failed{}",
                target.display(),
                recovery()
            )
        })?;
        drop(installed);
        sync_parent(&target).with_context(|| {
            format!(
                "verified output was installed at {}, but directory sync failed{}",
                target.display(),
                recovery()
            )
        })?;
        return Ok(WriteOutcome {
            bytes_written: verified_bytes.len() as u64,
            source_removed: false,
            backup,
        });
    }

    let published_identity = Identity::from_file(candidate.as_file().try_clone()?)?;
    let published = persist_new(candidate, &target)
        .with_context(|| format!("source is unchanged{}", recovery()))?;
    let published_hash = blake3::hash(verified_bytes);
    let second_stage = (|| -> Result<()> {
        // tempfile clears Windows attributes during MoveFileEx; restore the
        // original readonly bit on our returned handle, then close it before
        // any rollback/delete. A failure here follows the same rollback path.
        #[cfg(windows)]
        published.set_permissions(original.permissions.clone())?;
        drop(published);
        sync_parent(&target)?;
        hook(Stage::AfterPublish)?;
        // An explicit output directory keeps the original, but still does not
        // report success for a candidate derived from source bytes that changed.
        check_published_output(&target, &published_identity, published_hash)?;
        check_source(&source, &original, !preserve_source)?;
        if !preserve_source {
            // Recheck ancestry too: never intentionally traverse a symlink to
            // remove a file, and never remove a differently identified source.
            drop(original);
            assert_safe_ancestors(&source)?;
            fs::remove_file(&source)
                .with_context(|| format!("cannot remove original {}", source.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = second_stage {
        let rollback = remove_our_output(&target, published_identity, published_hash);
        return match rollback {
            Ok(()) => Err(anyhow!(
                "output publication rolled back; source was not removed: {error:#}{}",
                recovery()
            )),
            Err(rollback_error) => Err(anyhow!(
                "source was not removed: {error:#}; could not safely roll back output at {}: {rollback_error:#}; inspect this output before retrying{}",
                target.display(),
                recovery()
            )),
        };
    }
    if !preserve_source {
        sync_parent(&source).with_context(|| {
            format!("conversion completed at {} and original was removed, but source directory sync failed{}", target.display(), recovery())
        })?;
    }
    Ok(WriteOutcome {
        bytes_written: verified_bytes.len() as u64,
        source_removed: !preserve_source,
        backup,
    })
}

fn create_safe_parents(path: &Path) -> Result<()> {
    check_ancestors(path, true)?;
    let parent = path.parent().context("path has no parent")?;
    let mut cursor = PathBuf::new();
    for component in parent.components() {
        cursor.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&cursor) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Check again before every create, not just before create_dir_all.
                assert_safe_ancestors(&cursor)?;
                match fs::create_dir(&cursor) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("cannot create output parent {}", cursor.display())
                        });
                    }
                }
            }
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&cursor)?;
        ensure!(
            !is_link_like(&metadata) && metadata.is_dir(),
            "refusing non-directory or symlink output parent {}",
            cursor.display()
        );
    }
    Ok(())
}

// A read-only source may be copied with -o. On Windows a read-only temp
// cannot be unlinked, so restore write permission on our private temp handle
// before error cleanup. This never changes source-file permissions.
struct PreparedTemp(Option<NamedTempFile>);

impl PreparedTemp {
    fn as_file(&self) -> &File {
        self.0.as_ref().unwrap().as_file()
    }

    fn persist(mut self, destination: &Path, no_clobber: bool) -> std::io::Result<File> {
        let temp = self.0.take().unwrap();
        let result = if no_clobber {
            temp.persist_noclobber(destination)
        } else {
            temp.persist(destination)
        };
        match result {
            Ok(file) => Ok(file),
            Err(error) => {
                self.0 = Some(error.file);
                Err(error.error)
            }
        }
    }
}

impl Drop for PreparedTemp {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(temp) = &self.0 {
            let _ = make_own_file_removable(temp.as_file());
        }
    }
}

#[cfg(windows)]
fn make_own_file_removable(file: &File) -> Result<()> {
    let mut permissions = file.metadata()?.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

fn prepare_temp(
    destination: &Path,
    bytes: &[u8],
    permissions: &Permissions,
) -> Result<PreparedTemp> {
    assert_safe_ancestors(destination)?;
    let temp = Builder::new()
        .prefix(".tinyimg-")
        .suffix(".tmp")
        .tempfile_in(destination.parent().context("destination has no parent")?)?;
    let mut prepared = PreparedTemp(Some(temp));
    let temp = prepared.0.as_mut().unwrap();
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().set_permissions(permissions.clone())?;
    temp.as_file().sync_all()?;
    Ok(prepared)
}

fn persist_new(temp: PreparedTemp, destination: &Path) -> Result<File> {
    assert_safe_ancestors(destination)?;
    temp.persist(destination, true).map_err(|error| {
        anyhow!(
            "cannot publish without overwriting {}: {}",
            destination.display(),
            error
        )
    })
}

fn check_source(path: &Path, original: &SourceSnapshot, destructive: bool) -> Result<()> {
    let now = SourceSnapshot::capture(path)?;
    ensure!(
        now.identity == original.identity && now.hash == original.hash,
        "source changed since optimization: {}",
        path.display()
    );
    if destructive {
        check_writable_source(path, &now)?;
    }
    Ok(())
}

fn check_writable_source(path: &Path, original: &SourceSnapshot) -> Result<()> {
    ensure!(
        !original.permissions.readonly(),
        "refusing to replace or remove read-only source {}",
        path.display()
    );
    // A writable parent alone would permit replacing a read-only file on Unix.
    // Require file write access as well, without writing or truncating any bytes.
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("source is not writable: {}", path.display()))?;
    let metadata = regular_metadata(path)?;
    ensure!(
        Identity::from_file(file)? == original.identity && !metadata.permissions().readonly(),
        "source changed while checking write access: {}",
        path.display()
    );
    Ok(())
}

fn check_published_output(
    path: &Path,
    identity: &Identity,
    hash: blake3::Hash,
) -> Result<SourceSnapshot> {
    let current = SourceSnapshot::capture(path)?;
    ensure!(
        &current.identity == identity && current.hash == hash,
        "output changed after publication; refusing to remove it"
    );
    Ok(current)
}

fn remove_our_output(path: &Path, identity: Identity, hash: blake3::Hash) -> Result<()> {
    let current = check_published_output(path, &identity, hash)?;
    #[cfg(windows)]
    make_own_file_removable(current.identity.0.as_file())?;
    drop(current);
    drop(identity);
    assert_safe_ancestors(path)?;
    fs::remove_file(path)?;
    sync_parent(path).context("output removed, but rollback directory sync failed")
}

fn is_link_like(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and other reparse points, not only symlink tags.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn regular_metadata(path: &Path) -> Result<Metadata> {
    assert_safe_ancestors(path)?;
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(
        !is_link_like(&metadata),
        "refusing symlink {}",
        path.display()
    );
    ensure!(metadata.is_file(), "not a regular file: {}", path.display());
    Ok(metadata)
}

fn native_absolute(path: &Path) -> Result<PathBuf> {
    // std handles Windows drive prefixes, drive-relative paths and UNC shares;
    // do not emulate these rules with string splitting or hard-coded separators.
    std::path::absolute(path)
        .with_context(|| format!("cannot resolve native path {}", path.display()))
}

fn checked_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.file_name().is_some(),
        "path must name a file: {}",
        path.display()
    );
    let absolute = native_absolute(path)?;
    // Check the actual supplied route before normalizing '..', so normalization
    // cannot hide traversal through a symlink.
    check_ancestors(&absolute, true)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn assert_safe_ancestors(path: &Path) -> Result<()> {
    check_ancestors(path, false)
}

fn check_ancestors(path: &Path, allow_missing: bool) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    let mut cursor = PathBuf::new();
    for component in parent.components() {
        cursor.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&cursor) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect parent {}", cursor.display()));
            }
        };
        ensure!(
            !is_link_like(&metadata),
            "refusing symlink parent {}",
            cursor.display()
        );
        ensure!(
            metadata.is_dir(),
            "parent is not a directory: {}",
            cursor.display()
        );
    }
    Ok(())
}

fn check_no_collision(path: &Path, allowed_existing: Option<&Path>) -> Result<()> {
    check_ancestors(path, true)?;
    let name = path
        .file_name()
        .context("path has no filename")?
        .to_string_lossy()
        .to_lowercase();
    let entries = match fs::read_dir(path.parent().context("path has no parent")?) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().to_lowercase() == name {
            if allowed_existing == Some(entry.path().as_path()) {
                regular_metadata(&entry.path())?;
                continue;
            }
            bail!(
                "destination collision (case-insensitive): {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn same_spelling_folded(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    assert_safe_ancestors(path)?;
    File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    // std does not expose a portable Windows directory flush. Candidate file
    // contents are synced; directory durability is filesystem/platform dependent.
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Identity(same_file::Handle);

impl Identity {
    fn from_file(file: File) -> Result<Self> {
        Ok(Self(same_file::Handle::from_file(file)?))
    }
}

fn same_identity(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        // Only a preliminary read-stability check; destructive operations use
        // the open-file Identity above, not timestamps as a file identifier.
        a.created().ok() == b.created().ok()
    }
}

fn stable_metadata(a: &Metadata, b: &Metadata) -> bool {
    fn modified(metadata: &Metadata) -> Option<SystemTime> {
        metadata.modified().ok()
    }
    same_identity(a, b) && a.len() == b.len() && modified(a) == modified(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf, blake3::Hash) {
        let dir = tempfile::tempdir().unwrap();
        // macOS commonly exposes /var as a symlink. The production writer rejects
        // symlink ancestors, so fixtures use the real, canonical directory route.
        let source = dir.path().canonicalize().unwrap().join("photo.png");
        fs::write(&source, b"original bytes").unwrap();
        let hash = SourceSnapshot::capture(&source).unwrap().hash();
        (dir, source, hash)
    }

    fn no_temps(dir: &Path) {
        assert!(fs::read_dir(dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tinyimg-")
        }));
    }

    #[test]
    fn rejects_oversized_encoded_source_before_allocating_it() {
        let (dir, source, _) = fixture();
        OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        let error = SourceSnapshot::capture(&source).unwrap_err();
        assert!(error.to_string().contains("256 MiB encoded-file limit"));
        no_temps(dir.path());
    }

    #[test]
    fn same_path_replacement_is_complete_and_keeps_backup() {
        let (dir, source, hash) = fixture();
        let backup = source.with_extension("png.bak");
        let outcome = write_candidate(
            &source,
            &source,
            hash,
            b"verified smaller",
            Some(&backup),
            false,
        )
        .unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"verified smaller");
        assert_eq!(fs::read(&backup).unwrap(), b"original bytes");
        assert!(!outcome.source_removed);
        no_temps(dir.path());
    }

    #[test]
    fn conversion_publishes_then_removes_source() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        let outcome = write_candidate(&source, &target, hash, b"verified", None, false).unwrap();
        assert!(outcome.source_removed);
        assert!(!source.exists());
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        no_temps(dir.path());
    }

    #[test]
    fn output_mode_preserves_source() {
        let (dir, source, hash) = fixture();
        let target = source.with_file_name("copy.png");
        let outcome = write_candidate(&source, &target, hash, b"verified", None, true).unwrap();
        assert!(!outcome.source_removed);
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        no_temps(dir.path());
    }

    #[test]
    fn backup_collision_leaves_everything_untouched() {
        let (dir, source, hash) = fixture();
        let backup = source.with_extension("png.bak");
        fs::write(&backup, b"older backup").unwrap();
        assert!(
            write_candidate(&source, &source, hash, b"verified", Some(&backup), false).is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&backup).unwrap(), b"older backup");
        no_temps(dir.path());
    }

    #[test]
    fn target_collision_is_case_insensitive_and_never_clobbers() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        let existing = source.with_file_name("PHOTO.WEBP");
        fs::write(&existing, b"existing").unwrap();
        assert!(write_candidate(&source, &target, hash, b"verified", None, false).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"existing");
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        no_temps(dir.path());
    }

    #[test]
    fn stale_hash_never_publishes() {
        let (dir, source, hash) = fixture();
        fs::write(&source, b"edited").unwrap();
        let target = source.with_extension("webp");
        assert!(write_candidate(&source, &target, hash, b"verified", None, false).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"edited");
        assert!(!target.exists());
        no_temps(dir.path());
    }

    #[test]
    fn precommit_failure_cleans_temp_and_preserves_source() {
        let (dir, source, hash) = fixture();
        let error = write_with_hook(
            &source,
            &source,
            hash,
            b"verified",
            None,
            false,
            &mut |stage| {
                if stage == Stage::BeforePublish {
                    bail!("injected failure");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected failure"));
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        no_temps(dir.path());
    }

    #[test]
    fn source_change_after_publish_rolls_back_new_target() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        let error = write_with_hook(
            &source,
            &target,
            hash,
            b"verified",
            None,
            false,
            &mut |stage| {
                if stage == Stage::AfterPublish {
                    fs::write(&source, b"new user edit")?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"));
        assert_eq!(fs::read(&source).unwrap(), b"new user edit");
        assert!(!target.exists());
        no_temps(dir.path());
    }

    #[test]
    fn second_stage_failure_rolls_back_without_data_loss() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        assert!(
            write_with_hook(
                &source,
                &target,
                hash,
                b"verified",
                None,
                false,
                &mut |stage| {
                    if stage == Stage::AfterPublish {
                        bail!("injected delete failure");
                    }
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert!(!target.exists());
        no_temps(dir.path());
    }

    #[test]
    fn rollback_will_not_remove_an_unexpected_modified_output() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        let error = write_with_hook(
            &source,
            &target,
            hash,
            b"verified",
            None,
            false,
            &mut |stage| {
                if stage == Stage::AfterPublish {
                    fs::write(&target, b"other process output")?;
                    bail!("injected failure");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("inspect this output"));
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"other process output");
        no_temps(dir.path());
    }

    #[test]
    fn changed_published_output_does_not_allow_source_deletion() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        let error = write_with_hook(
            &source,
            &target,
            hash,
            b"verified",
            None,
            false,
            &mut |stage| {
                if stage == Stage::AfterPublish {
                    fs::write(&target, b"changed output")?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("inspect this output"));
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"changed output");
        no_temps(dir.path());
    }

    #[test]
    fn copying_readonly_source_preserves_readonly_permissions() {
        let (dir, source, hash) = fixture();
        let original = fs::metadata(&source).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&source, readonly).unwrap();
        let target = source.with_extension("webp");
        write_candidate(&source, &target, hash, b"verified", None, true).unwrap();
        assert!(fs::metadata(&target).unwrap().permissions().readonly());
        assert!(fs::metadata(&source).unwrap().permissions().readonly());
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        fs::set_permissions(&source, original.clone()).unwrap();
        fs::set_permissions(&target, original).unwrap();
        no_temps(dir.path());
    }

    #[test]
    fn copying_readonly_source_failure_cleans_temporary_files() {
        let (dir, source, hash) = fixture();
        let original = fs::metadata(&source).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&source, readonly).unwrap();
        let target = source.with_extension("webp");
        let error = write_with_hook(
            &source,
            &target,
            hash,
            b"verified",
            None,
            true,
            &mut |stage| {
                if stage == Stage::BeforePublish {
                    bail!("injected failure");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected failure"));
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert!(!target.exists());
        fs::set_permissions(&source, original).unwrap();
        no_temps(dir.path());
    }

    #[test]
    fn atomic_no_clobber_rejects_late_existing_destination_and_cleans_temp() {
        let (dir, source, _) = fixture();
        let target = source.with_extension("webp");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let prepared = prepare_temp(&target, b"verified", &permissions).unwrap();
        fs::write(&target, b"arrived after planning").unwrap();
        assert!(persist_new(prepared, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"arrived after planning");
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        no_temps(dir.path());
    }

    #[test]
    fn failure_after_backup_creation_identifies_recovery_copy() {
        let (dir, source, hash) = fixture();
        let backup = source.with_extension("png.bak");
        let error = write_with_hook(
            &source,
            &source,
            hash,
            b"verified",
            Some(&backup),
            false,
            &mut |stage| {
                if stage == Stage::BeforePublish {
                    bail!("injected failure");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(&backup.display().to_string()));
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&backup).unwrap(), b"original bytes");
        no_temps(dir.path());
    }

    #[test]
    fn destination_created_at_commit_is_not_overwritten() {
        let (dir, source, hash) = fixture();
        let target = source.with_extension("webp");
        assert!(
            write_with_hook(
                &source,
                &target,
                hash,
                b"verified",
                None,
                false,
                &mut |stage| {
                    if stage == Stage::BeforePublish {
                        fs::write(&target, b"concurrent file")?;
                    }
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"concurrent file");
        no_temps(dir.path());
    }

    #[test]
    fn native_paths_support_spaces_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("照片 résumé image.png");
        let target = root.join("输出 folder").join("converted image.webp");
        let backup = root.join("backup folder").join("original 照片.png");
        fs::write(&source, b"original bytes").unwrap();
        let hash = SourceSnapshot::capture(&source).unwrap().hash();
        write_candidate(&source, &target, hash, b"verified", Some(&backup), true).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        assert_eq!(fs::read(&backup).unwrap(), b"original bytes");
        no_temps(&root);
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_drive_and_unc_prefixes_are_preserved() {
        for path in [
            r"C:\images\folder with spaces\照片.png",
            r"\\server\share\images\照片.png",
            r"\\?\C:\images\照片.png",
            r"\\?\UNC\server\share\images\照片.png",
        ] {
            let absolute = native_absolute(Path::new(path)).unwrap();
            assert!(absolute.is_absolute());
            assert!(matches!(
                absolute.components().next(),
                Some(Component::Prefix(_))
            ));
            assert_eq!(absolute.file_name().unwrap(), "照片.png");
        }
    }

    #[test]
    fn preflight_does_not_create_nested_output_or_backup_directories() {
        let (dir, source, _) = fixture();
        let target = source
            .parent()
            .unwrap()
            .join("out")
            .join("nested")
            .join("photo.webp");
        let backup = source
            .parent()
            .unwrap()
            .join("backup")
            .join("nested")
            .join("photo.png");
        preflight_target(&source, &target, Some(&backup), false).unwrap();
        assert!(!target.parent().unwrap().exists());
        assert!(!backup.parent().unwrap().exists());
        no_temps(dir.path());
    }

    #[test]
    fn write_creates_nested_output_and_backup_parents() {
        let (dir, source, hash) = fixture();
        let target = source
            .parent()
            .unwrap()
            .join("out")
            .join("nested")
            .join("photo.webp");
        let backup = source
            .parent()
            .unwrap()
            .join("backup")
            .join("nested")
            .join("photo.png");
        write_candidate(&source, &target, hash, b"verified", Some(&backup), true).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        assert_eq!(fs::read(&backup).unwrap(), b"original bytes");
        no_temps(dir.path());
        no_temps(target.parent().unwrap());
        no_temps(backup.parent().unwrap());
    }

    #[test]
    fn output_same_as_source_is_rejected_even_with_preserve_enabled() {
        let (dir, source, hash) = fixture();
        assert!(preflight_target(&source, &source, None, true).is_err());
        assert!(write_candidate(&source, &source, hash, b"verified", None, true).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        no_temps(dir.path());
    }

    #[test]
    fn read_only_sources_cannot_be_replaced_or_removed() {
        let (dir, source, hash) = fixture();
        let mut permissions = fs::metadata(&source).unwrap().permissions();
        let original = permissions.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions).unwrap();
        assert!(write_candidate(&source, &source, hash, b"verified", None, false).is_err());
        assert!(
            write_candidate(
                &source,
                &source.with_extension("webp"),
                hash,
                b"verified",
                None,
                false
            )
            .is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        fs::set_permissions(&source, original).unwrap();
        no_temps(dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_and_parent_symlinks() {
        use std::os::unix::fs::symlink;
        let (dir, source, hash) = fixture();
        let alias = source.with_file_name("alias.png");
        symlink(&source, &alias).unwrap();
        assert!(SourceSnapshot::capture(&alias).is_err());
        let linked_dir = source.with_file_name("linked-dir");
        symlink(source.parent().unwrap(), &linked_dir).unwrap();
        assert!(
            write_candidate(
                &source,
                &linked_dir.join("new.webp"),
                hash,
                b"verified",
                None,
                false
            )
            .is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        no_temps(dir.path());
    }
}
