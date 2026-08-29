//! Shared validation, format detection, progress, and FFI string helpers.

use super::*;

pub(super) fn opening_snapshot(path: &Path, total_bytes: u64) -> ProgressSnapshot {
    let mut snapshot = ProgressSnapshot::new(ProgressPhase::Opening);
    snapshot.current_file = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();
    snapshot.total_bytes = Some(total_bytes);
    snapshot
}

pub(super) fn file_length(path: &Path) -> ArchiveResult<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| ArchiveError::io(path, error))
}

pub(super) fn check_cancel(cancel: &CancellationToken) -> ArchiveResult<()> {
    if cancel.is_cancelled() {
        Err(ArchiveError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn enforce_limit(value: u64, limit: u64, subject: &str) -> ArchiveResult<()> {
    if value > limit {
        Err(ArchiveError::LimitExceeded(format!(
            "{subject} exceeds {limit}"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn checked_add_with_limit(
    current: u64,
    amount: u64,
    limit: u64,
    subject: &str,
) -> ArchiveResult<u64> {
    let value = current
        .checked_add(amount)
        .ok_or_else(|| ArchiveError::LimitExceeded(format!("{subject} overflow")))?;
    enforce_limit(value, limit, subject)?;
    Ok(value)
}

pub(super) fn validate_compression_level(options: &CreateOptions) -> ArchiveResult<()> {
    let maximum = match options.format {
        CreateFormat::TarZstd => 22,
        _ => 9,
    };
    if options.compression_level > maximum {
        return Err(ArchiveError::UnsupportedOption(format!(
            "compression level {} is outside 0..={maximum}",
            options.compression_level
        )));
    }
    Ok(())
}

pub(super) fn is_standalone_filter(path: &Path) -> ArchiveResult<bool> {
    let mut file = File::open(path).map_err(|error| ArchiveError::io(path, error))?;
    let mut signature = [0u8; 8];
    let amount = file
        .read(&mut signature)
        .map_err(|error| ArchiveError::io(path, error))?;
    let bytes = &signature[..amount];
    Ok(bytes.starts_with(&[0x1f, 0x8b])
        || bytes.starts_with(b"BZh")
        || bytes.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00])
        || bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || bytes.starts_with(&[0x04, 0x22, 0x4d, 0x18])
        || bytes.starts_with(b"LZIP")
        || bytes.starts_with(&[0x1f, 0x9d]))
}

pub(super) fn is_wrapped_tar_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    [".tar.gz", ".tgz"]
        .iter()
        .any(|extension| name.ends_with(extension))
}

pub(super) fn standalone_payload_name(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_string_lossy();
    let lower = file_name.to_ascii_lowercase();
    for (extension, payload_extension) in [(".tgz", ".tar"), (".tbz2", ".tar"), (".txz", ".tar")] {
        if lower.ends_with(extension) {
            let stem = &file_name[..file_name.len() - extension.len()];
            return (!stem.is_empty()).then(|| PathBuf::from(format!("{stem}{payload_extension}")));
        }
    }

    const COMPRESSION_EXTENSIONS: [&str; 7] = ["gz", "bz2", "xz", "zst", "lz4", "lz", "z"];

    let extension = path.extension()?.to_str()?;
    if !COMPRESSION_EXTENSIONS
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        return None;
    }
    let stem = path.file_stem()?;
    (!stem.is_empty()).then(|| PathBuf::from(stem))
}

pub(super) fn wide_nul(path: &Path) -> ArchiveResult<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(ArchiveError::InvalidInput(format!(
            "path contains NUL: {}",
            path.display()
        )));
    }
    wide.push(0);
    Ok(wide)
}

pub(super) unsafe fn copy_wide_string(pointer: *const u16) -> ArchiveResult<Option<OsString>> {
    if pointer.is_null() {
        return Ok(None);
    }
    let mut length = 0usize;
    // SAFETY: libarchive promises a NUL-terminated wchar_t string.  The
    // explicit cap prevents pathological metadata from consuming memory.
    while length <= MAX_ARCHIVE_PATH_UNITS && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    if length > MAX_ARCHIVE_PATH_UNITS {
        return Err(ArchiveError::LimitExceeded(
            "archive pathname is longer than 1,048,576 UTF-16 units".to_owned(),
        ));
    }
    // SAFETY: the scan above established `length` initialized units.
    let units = unsafe { std::slice::from_raw_parts(pointer, length) };
    Ok(Some(OsString::from_wide(units)))
}

pub(super) unsafe fn copy_c_string_bounded(
    pointer: *const c_char,
    max_bytes: usize,
    subject: &str,
) -> ArchiveResult<Option<String>> {
    if pointer.is_null() {
        return Ok(None);
    }
    let mut length = 0usize;
    // SAFETY: libarchive promises a NUL-terminated string. The explicit
    // maximum bounds the attacker-influenced scan and resulting allocation.
    while length <= max_bytes && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    if length > max_bytes {
        return Err(ArchiveError::LimitExceeded(format!(
            "{subject} exceeds {max_bytes} bytes"
        )));
    }
    // SAFETY: the bounded scan established `length` initialized bytes.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

pub(super) unsafe fn copy_c_string_bytes_bounded(
    pointer: *const c_char,
    max_bytes: usize,
    subject: &str,
) -> ArchiveResult<Option<Vec<u8>>> {
    if pointer.is_null() {
        return Ok(None);
    }
    let mut length = 0usize;
    // SAFETY: libarchive promises a NUL-terminated string. The explicit
    // maximum bounds the attacker-influenced scan and resulting allocation.
    while length <= max_bytes && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    if length > max_bytes {
        return Err(ArchiveError::LimitExceeded(format!(
            "{subject} exceeds {max_bytes} bytes"
        )));
    }
    // SAFETY: the bounded scan established `length` initialized bytes.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
    Ok(Some(bytes.to_vec()))
}

pub(super) fn copy_c_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        None
    } else {
        // SAFETY: all callers pass libarchive-owned NUL-terminated strings.
        Some(
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

pub(super) fn secret_c_string(secret: &str) -> ArchiveResult<CString> {
    if secret.is_empty() {
        return Err(ArchiveError::InvalidInput(
            "archive password cannot be empty".to_owned(),
        ));
    }
    CString::new(secret)
        .map_err(|_| ArchiveError::InvalidInput("archive password contains NUL".to_owned()))
}

pub(super) fn looks_like_password_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "passphrase required",
        "passphrase is required",
        "password required",
        "password is required",
        "no passphrase",
        "no password",
        "incorrect passphrase",
        "incorrect password",
        "wrong passphrase",
        "wrong password",
        "invalid passphrase",
        "invalid password",
        "bad passphrase",
        "bad password",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}
