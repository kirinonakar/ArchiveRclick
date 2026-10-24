//! Destination-volume preflight. Header sizes are estimates, never write authority.
use super::{ArchiveError, ArchiveResult, ExtractOptions};
use std::path::Path;

pub(crate) fn available_bytes(directory: &Path) -> ArchiveResult<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::PCWSTR};
    let mut wide: Vec<u16> = directory.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = 0;
    // SAFETY: directory is NUL-terminated and the output remains live.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
        .map_err(|error| {
            ArchiveError::io(
                directory,
                std::io::Error::from_raw_os_error(error.code().0 & 0xffff),
            )
        })?;
    Ok(available)
}

fn check_estimate(directory: &Path, declared: u64, available: u64) -> ArchiveResult<()> {
    if declared > available {
        return Err(ArchiveError::InsufficientSpace {
            path: directory.to_owned(),
            required: declared,
            available,
        });
    }
    Ok(())
}

pub(crate) fn check_declared(directory: &Path, declared: u64) -> ArchiveResult<()> {
    check_estimate(directory, declared, available_bytes(directory)?)
}

pub(crate) fn extraction_options(
    directory: &Path,
    declared: Option<u64>,
    options: &ExtractOptions,
) -> ArchiveResult<ExtractOptions> {
    let available = available_bytes(directory)?;
    check_estimate(directory, declared.unwrap_or(0), available)?;
    let mut options = options.clone();
    // Bound actual writes too, including archives with missing/false sizes.
    // Other processes, allocation overhead and quotas can still exhaust space;
    // callers must preserve and report real write errors.
    options.max_total_bytes = options.max_total_bytes.min(available);
    options.max_file_bytes = options.max_file_bytes.min(available);
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insufficient_estimates_fail_before_output_and_exact_fit_is_allowed() {
        let path = Path::new("destination");
        assert!(check_estimate(path, 100, 100).is_ok());
        assert!(check_estimate(path, 0, 0).is_ok());
        assert!(matches!(
            check_estimate(path, 101, 100),
            Err(ArchiveError::InsufficientSpace {
                required: 101,
                available: 100,
                ..
            })
        ));
    }
    #[test]
    fn uses_destination_volume_and_caps_actual_output() {
        let root = tempfile::tempdir().unwrap();
        let options = ExtractOptions {
            max_total_bytes: u64::MAX,
            max_file_bytes: u64::MAX,
            ..Default::default()
        };
        let effective = extraction_options(root.path(), Some(1), &options).unwrap();
        assert!(effective.max_total_bytes > 0 && effective.max_total_bytes < u64::MAX);
        assert_eq!(effective.max_file_bytes, effective.max_total_bytes);
        assert!(matches!(
            extraction_options(root.path(), Some(u64::MAX), &options),
            Err(ArchiveError::InsufficientSpace { .. })
        ));
    }
}
