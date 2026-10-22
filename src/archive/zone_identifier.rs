//! Download-origin propagation to every extracted file, including unknown types.
use super::{ArchiveError, ArchiveResult, OperationSummary};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Default)]
pub(crate) struct ZoneIdentifier {
    data: Option<Arc<[u8]>>,
    failures: Arc<Failures>,
}

#[derive(Default)]
struct Failures {
    count: AtomicU64,
    first: Mutex<Option<String>>,
}

fn stream_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(":Zone.Identifier");
    name.into()
}

fn unsupported(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(1 | 50 | 123))
}

impl ZoneIdentifier {
    pub(crate) fn read(archive: &Path, enabled: bool) -> ArchiveResult<Self> {
        if !enabled {
            return Ok(Self::default());
        }
        let path = stream_path(archive);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound || unsupported(&error) => {
                return Ok(Self::default());
            }
            Err(error) => return Err(ArchiveError::io(&path, error)),
        };
        // ADS is untrusted input; origin metadata should be small.
        let mut data = Vec::new();
        file.take(65537)
            .read_to_end(&mut data)
            .map_err(|error| ArchiveError::io(&path, error))?;
        if data.len() > 65536 {
            return Err(ArchiveError::LimitExceeded(
                "Zone.Identifier exceeds 64 KiB".into(),
            ));
        }
        Ok(Self {
            data: Some(data.into()),
            ..Default::default()
        })
    }

    /// Mark the staged file before installation. Metadata failures become warnings.
    pub(crate) fn apply(&self, output: &Path, target: &Path) {
        let Some(data) = &self.data else {
            return;
        };
        if let Err(error) = fs::write(stream_path(output), data) {
            self.failures.count.fetch_add(1, Ordering::Relaxed);
            let mut first = self
                .failures
                .first
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if first.is_none() {
                *first = Some(format!("{}: {error}", target.display()));
            }
        }
    }

    pub(crate) fn report_warnings(&self, summary: &mut OperationSummary) {
        let count = self.failures.count.load(Ordering::Relaxed);
        if count != 0 {
            let first = self
                .failures
                .first
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            summary.add_warning(&format!(
                "Could not copy Zone.Identifier to {count} file(s); first failure: {}",
                first.as_deref().unwrap_or("unknown error")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ads_failure_becomes_a_warning() {
        let work = tempfile::tempdir().unwrap();
        let zone = ZoneIdentifier {
            data: Some(Arc::from(&b"[ZoneTransfer]\r\nZoneId=3\r\n"[..])),
            ..Default::default()
        };
        zone.apply(&work.path().join("missing/file"), Path::new("file.docm"));
        let mut summary = OperationSummary::default();
        zone.report_warnings(&mut summary);
        assert!(summary.warning.unwrap().contains("1 file(s)"));
    }
}
