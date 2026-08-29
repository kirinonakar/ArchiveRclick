//! Conflict handling and race-resistant extraction output installation.

use super::*;

pub(super) enum RuntimeConflictPolicy {
    Ask,
    OverwriteAll,
    SkipAll,
}

impl From<InitialConflictPolicy> for RuntimeConflictPolicy {
    fn from(value: InitialConflictPolicy) -> Self {
        match value {
            InitialConflictPolicy::Ask => Self::Ask,
            InitialConflictPolicy::OverwriteAll => Self::OverwriteAll,
            InitialConflictPolicy::SkipAll => Self::SkipAll,
        }
    }
}

pub(super) enum ConflictAction {
    Overwrite,
    Skip,
}

pub(super) fn resolve_existing(
    target: &Path,
    policy: &mut RuntimeConflictPolicy,
    resolver: &dyn ConflictResolver,
) -> ArchiveResult<ConflictAction> {
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            if is_reparse(&metadata) {
                return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConflictAction::Overwrite);
        }
        Err(error) => return Err(ArchiveError::io(target, error)),
    }

    match policy {
        RuntimeConflictPolicy::OverwriteAll => Ok(ConflictAction::Overwrite),
        RuntimeConflictPolicy::SkipAll => Ok(ConflictAction::Skip),
        RuntimeConflictPolicy::Ask => match resolver.resolve(target) {
            ConflictChoice::Overwrite => Ok(ConflictAction::Overwrite),
            ConflictChoice::Skip => Ok(ConflictAction::Skip),
            ConflictChoice::OverwriteAll => {
                *policy = RuntimeConflictPolicy::OverwriteAll;
                Ok(ConflictAction::Overwrite)
            }
            ConflictChoice::SkipAll => {
                *policy = RuntimeConflictPolicy::SkipAll;
                Ok(ConflictAction::Skip)
            }
            ConflictChoice::Cancel => Err(ArchiveError::Cancelled),
        },
    }
}

pub(super) fn prepare_directory(
    root: &Path,
    target: &Path,
    policy: &mut RuntimeConflictPolicy,
    resolver: &dyn ConflictResolver,
) -> ArchiveResult<ConflictAction> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_reparse(&metadata) => {
            return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
        }
        Ok(metadata) if metadata.is_dir() => return Ok(ConflictAction::Overwrite),
        Ok(_) => match resolve_existing(target, policy, resolver)? {
            ConflictAction::Skip => return Ok(ConflictAction::Skip),
            ConflictAction::Overwrite => {
                fs::remove_file(target).map_err(|error| ArchiveError::io(target, error))?;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ArchiveError::io(target, error)),
    }
    ensure_parent_directories(root, target)?;
    fs::create_dir(target)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| ArchiveError::io(target, error))?;
    ensure_no_reparse_ancestors(root, target)?;
    verify_directory_handle(root, target)?;
    Ok(ConflictAction::Overwrite)
}

pub(super) fn ensure_parent_directories(root: &Path, target: &Path) -> ArchiveResult<()> {
    let parent = target
        .parent()
        .ok_or_else(|| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;
    ensure_no_reparse_ancestors(root, parent)?;
    fs::create_dir_all(parent).map_err(|error| ArchiveError::io(parent, error))?;
    ensure_no_reparse_ancestors(root, parent)?;
    verify_directory_handle(root, parent)
}

pub(super) fn verification_handle(path: &Path) -> ArchiveResult<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| ArchiveError::io(path, error))
}

pub(super) fn final_path_by_handle(file: &File, subject: &Path) -> ArchiveResult<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `file` owns a live Windows handle and `buffer` is writable
        // for the supplied capacity. Zero selects normalized DOS paths.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        if length == 0 {
            return Err(ArchiveError::io(subject, std::io::Error::last_os_error()));
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        let required = length.checked_add(1).ok_or_else(|| {
            ArchiveError::LimitExceeded("resolved Windows path length overflow".to_owned())
        })?;
        if required > MAX_ARCHIVE_PATH_UNITS {
            return Err(ArchiveError::LimitExceeded(
                "resolved Windows path is too long".to_owned(),
            ));
        }
        buffer.resize(required, 0);
    }
}

pub(super) fn path_is_within_case_insensitive(candidate: &Path, root: &Path) -> bool {
    let mut candidate = candidate.components();
    root.components().all(|root_component| {
        candidate.next().is_some_and(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&root_component.as_os_str().to_string_lossy())
        })
    })
}

pub(super) fn verified_root_final_path(root: &Path) -> ArchiveResult<PathBuf> {
    let metadata = fs::symlink_metadata(root).map_err(|error| ArchiveError::io(root, error))?;
    if is_reparse(&metadata) {
        return Err(ArchiveError::ReparsePoint(root.to_path_buf()));
    }
    let handle = verification_handle(root)?;
    final_path_by_handle(&handle, root)
}

pub(super) fn verify_directory_handle(root: &Path, directory: &Path) -> ArchiveResult<()> {
    let root_final = verified_root_final_path(root)?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|error| ArchiveError::io(directory, error))?;
    if is_reparse(&metadata) {
        return Err(ArchiveError::ReparsePoint(directory.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(ArchiveError::UnsafeEntryPath(
            directory.display().to_string(),
        ));
    }
    let handle = verification_handle(directory)?;
    let resolved = final_path_by_handle(&handle, directory)?;
    if path_is_within_case_insensitive(&resolved, &root_final) {
        Ok(())
    } else {
        Err(ArchiveError::UnsafeEntryPath(format!(
            "{} resolves outside {}",
            directory.display(),
            root.display()
        )))
    }
}

pub(super) fn verify_file_handle_within_root(
    root: &Path,
    file: &File,
    subject: &Path,
) -> ArchiveResult<()> {
    let root_final = verified_root_final_path(root)?;
    let resolved = final_path_by_handle(file, subject)?;
    if path_is_within_case_insensitive(&resolved, &root_final) {
        Ok(())
    } else {
        Err(ArchiveError::UnsafeEntryPath(format!(
            "{} resolves outside {}",
            subject.display(),
            root.display()
        )))
    }
}

pub(super) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) struct TemporaryPath {
    pub(super) path: PathBuf,
    pub(super) file: Option<File>,
    pub(super) armed: bool,
}

impl TemporaryPath {
    pub(super) fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("temporary file handle is still open")
    }

    pub(super) fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("temporary file handle is still open")
    }

    pub(super) fn close_file(&mut self) {
        drop(self.file.take());
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        self.close_file();
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn temporary_file(directory: &Path) -> ArchiveResult<TemporaryPath> {
    for _ in 0..128 {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".archiverclick-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                return Ok(TemporaryPath {
                    path,
                    file: Some(file),
                    armed: true,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ArchiveError::io(path, error)),
        }
    }
    Err(ArchiveError::InvalidInput(
        "could not allocate a unique temporary file".to_owned(),
    ))
}

pub(super) fn install_temporary(root: &Path, temporary: &Path, target: &Path) -> ArchiveResult<()> {
    ensure_no_reparse_ancestors(root, target)?;
    let target_parent = target
        .parent()
        .ok_or_else(|| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;
    verify_directory_handle(root, target_parent)?;
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_reparse(&metadata) => {
            return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
        }
        Ok(metadata) if metadata.is_dir() => {
            return Err(ArchiveError::InvalidInput(format!(
                "cannot replace a directory with a file: {}",
                target.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ArchiveError::io(target, error)),
    }
    ensure_no_reparse_ancestors(root, target)?;
    verify_directory_handle(root, target_parent)?;
    let existing = wide_nul(temporary)?;
    let replacement = wide_nul(target)?;
    // SAFETY: both strings are NUL-terminated and the paths share a
    // directory/volume. REPLACE_EXISTING preserves the old file if the
    // atomic rename cannot be completed. The handle-resolved parent check
    // above detects path swaps that precede it. A same-user swap in the
    // final path-based Win32 call's narrow race window cannot be eliminated
    // without NT handle-relative rename APIs; see the security handoff.
    let moved = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved != 0 {
        Ok(())
    } else {
        Err(ArchiveError::io(target, std::io::Error::last_os_error()))
    }
}
