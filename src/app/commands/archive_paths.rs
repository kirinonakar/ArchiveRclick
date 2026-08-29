//! Archive-name classification shared by startup and drag/drop inputs.

use super::*;

const ARCHIVE_DROP_EXTENSIONS: &[&str] = &[
    "zip", "zipx", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "cab", "lha", "lzh", "tgz",
    "tbz2", "txz", "iso", "img",
];

pub(super) fn is_archive_drop_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let extension_matches = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ARCHIVE_DROP_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        });
    extension_matches || split_archive_volume_base_name(&name).is_some()
}

pub(super) fn split_archive_volume_base_name(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('.')?;
    let base_lower = base.to_ascii_lowercase();
    (base_lower.ends_with(".zip") || base_lower.ends_with(".7z"))
        .then_some(())
        .filter(|_| suffix.len() >= 3 && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|_| base)
}
