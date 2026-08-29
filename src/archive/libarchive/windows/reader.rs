//! RAII wrapper and scan budgets for libarchive readers.

use super::*;

pub(super) struct Reader<'a> {
    pub(super) api: &'a Api,
    pub(super) raw: NonNull<RawArchive>,
    pub(super) standalone_payload_name: Option<PathBuf>,
    pub(super) prefer_source_payload_name: bool,
    pub(super) closed: bool,
}

pub(super) struct RawEntryInfo {
    pub(super) path: PathBuf,
    pub(super) display_path: String,
    pub(super) size: Option<u64>,
    pub(super) modified_unix_seconds: Option<i64>,
    pub(super) kind: ArchiveEntryKind,
    pub(super) encrypted: bool,
}

pub(super) struct ScanBudget {
    pub(super) entries_visited: u64,
    pub(super) decoded_bytes: u64,
    pub(super) max_entries: u64,
    pub(super) max_decoded_bytes: u64,
    pub(super) operation: &'static str,
}

impl ScanBudget {
    pub(super) fn new(max_entries: u64, max_decoded_bytes: u64, operation: &'static str) -> Self {
        Self {
            entries_visited: 0,
            decoded_bytes: 0,
            max_entries,
            max_decoded_bytes,
            operation,
        }
    }

    pub(super) fn visit_entry(&mut self) -> ArchiveResult<()> {
        self.entries_visited = self.entries_visited.checked_add(1).ok_or_else(|| {
            ArchiveError::LimitExceeded(format!("{} entry scan overflow", self.operation))
        })?;
        if self.entries_visited > self.max_entries {
            return Err(ArchiveError::LimitExceeded(format!(
                "{} entry scan exceeds {}",
                self.operation, self.max_entries
            )));
        }
        Ok(())
    }

    pub(super) fn account_decoded(&mut self, amount: usize) -> ArchiveResult<()> {
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(amount as u64)
            .ok_or_else(|| {
                ArchiveError::LimitExceeded(format!(
                    "{} decoded-data scan overflow",
                    self.operation
                ))
            })?;
        if self.decoded_bytes > self.max_decoded_bytes {
            return Err(ArchiveError::LimitExceeded(format!(
                "{} decoded-data scan exceeds {} bytes",
                self.operation, self.max_decoded_bytes
            )));
        }
        Ok(())
    }
}

impl<'a> Reader<'a> {
    pub(super) fn open(api: &'a Api, path: &Path, password: Option<&str>) -> ArchiveResult<Self> {
        let standalone_filter = is_standalone_filter(path)?;
        let raw_only = standalone_filter && is_wrapped_tar_path(path);
        // SAFETY: constructor has no preconditions and returns an owned handle.
        let raw = NonNull::new(unsafe { (api.read_new)() }).ok_or_else(|| {
            ArchiveError::LibraryUnavailable("archive_read_new returned null".to_owned())
        })?;
        let reader = Self {
            api,
            raw,
            standalone_payload_name: standalone_filter
                .then(|| standalone_payload_name(path))
                .flatten(),
            prefer_source_payload_name: raw_only,
            closed: false,
        };

        for support in &api.read_filters {
            reader.require_ok(
                // SAFETY: reader owns a live read handle in NEW state.
                unsafe { support(raw.as_ptr()) },
                "enabling an archive filter",
            )?;
        }
        if is_iso_path(path) {
            // ISO 9660 images can begin with bytes that make libarchive's
            // tar bidder win when every format is enabled.  The ISO
            // extension is an explicit format hint, so enable only the
            // ISO reader for these paths and avoid the false tar match.
            reader.require_ok(
                // SAFETY: reader owns a live read handle in NEW state.
                unsafe { (api.read_support_format_iso9660)(raw.as_ptr()) },
                "enabling the ISO 9660 archive format",
            )?;
        } else if is_lha_path(path) {
            // LHA/LZH has no fixed magic signature.  A compressed LHA
            // member can contain a ZIP signature, so enabling every
            // bidder lets libarchive select the nested ZIP by mistake.
            reader.require_ok(
                // SAFETY: reader owns a live read handle in NEW state.
                unsafe { (api.read_support_format_lha)(raw.as_ptr()) },
                "enabling the LHA archive format",
            )?;
        } else if !raw_only {
            for support in &api.read_formats {
                reader.require_ok(
                    // SAFETY: reader owns a live read handle in NEW state.
                    unsafe { support(raw.as_ptr()) },
                    "enabling an archive format",
                )?;
            }
        }
        if standalone_filter {
            let support_raw = api.read_support_format_raw.ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "this libarchive build cannot read standalone compressed streams".to_owned(),
                )
            })?;
            reader.require_ok(
                // SAFETY: reader owns a live read handle in NEW state.
                unsafe { support_raw(raw.as_ptr()) },
                "enabling raw compressed streams",
            )?;
        }

        if let Some(password) = password {
            let password = secret_c_string(password)?;
            let add = api.read_add_passphrase.ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "this libarchive build has no password reader API".to_owned(),
                )
            })?;
            reader.require_ok(
                // SAFETY: libarchive copies the passphrase during this call.
                unsafe { add(raw.as_ptr(), password.as_ptr()) },
                "adding archive password",
            )?;
        }

        let wide = wide_nul(path)?;
        reader.require_ok(
            // SAFETY: path is NUL-terminated and the handle is in NEW state.
            unsafe { (api.read_open_filename_w)(raw.as_ptr(), wide.as_ptr(), OPEN_BLOCK_SIZE) },
            "opening archive",
        )?;
        Ok(reader)
    }

    pub(super) fn next_entry(
        &mut self,
        pathname_codepage: u32,
    ) -> ArchiveResult<Option<RawEntryInfo>> {
        let mut retries = 0;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: the read handle is open; libarchive owns the entry ptr.
            let status = unsafe { (self.api.read_next_header)(self.raw.as_ptr(), &mut entry) };
            match status {
                ARCHIVE_EOF => return Ok(None),
                ARCHIVE_OK | ARCHIVE_WARN if !entry.is_null() => {
                    return self.copy_entry(entry, pathname_codepage).map(Some);
                }
                ARCHIVE_RETRY if retries < 3 => retries += 1,
                _ => return Err(self.error("reading archive header")),
            }
        }
    }

    pub(super) fn copy_entry(
        &self,
        entry: *mut RawEntry,
        pathname_codepage: u32,
    ) -> ArchiveResult<RawEntryInfo> {
        // SAFETY: entry is borrowed from the live reader until next_header.
        // Prefer the raw pathname bytes.  libarchive keeps legacy
        // (non-UTF-8) names as raw bytes, while the locale-based wide and
        // UTF-8 getters would either fail or return mojibake for them
        // under the default CRT locale.  Valid UTF-8 passes through
        // unchanged; other encodings are recovered by the heuristic
        // encoding detector (CP949 / Shift_JIS / GBK / Big5 / ...).
        let raw = unsafe {
            copy_c_string_bytes_bounded(
                (self.api.entry_pathname)(entry),
                MAX_ARCHIVE_PATH_UTF8_BYTES,
                "archive raw pathname",
            )?
        };
        let path = raw
            .and_then(|raw| encoding::decode_name_with_codepage(&raw, pathname_codepage))
            .filter(|name| !name.is_empty())
            .map(PathBuf::from);
        let mut path = match path {
            Some(path) => path,
            None => {
                // Formats that store Unicode directly (7z, RAR, Joliet
                // ISO 9660) do not expose raw mbs bytes; fall back to the
                // wide getter, which libarchive fills from its internal
                // Unicode copy.
                let wide_path = unsafe { copy_wide_string((self.api.entry_pathname_w)(entry))? };
                match wide_path {
                    Some(path) => PathBuf::from(path),
                    None => match self.api.entry_pathname_utf8 {
                        Some(getter) => {
                            // SAFETY: getter borrows a NUL-terminated string
                            // from this live entry; the bounded copy prevents
                            // an attacker-controlled pathname from causing an
                            // unbounded scan/allocation in the UTF-8 fallback.
                            unsafe {
                                copy_c_string_bounded(
                                    getter(entry),
                                    MAX_ARCHIVE_PATH_UTF8_BYTES,
                                    "archive UTF-8 pathname",
                                )?
                            }
                            .map(PathBuf::from)
                            .ok_or_else(|| {
                                ArchiveError::LibArchive {
                                    operation: "decoding archive pathname",
                                    message: "entry pathname is missing or cannot be decoded"
                                        .to_owned(),
                                }
                            })?
                        }
                        None => {
                            return Err(ArchiveError::LibArchive {
                                operation: "decoding archive pathname",
                                message: "entry pathname is missing or cannot be decoded"
                                    .to_owned(),
                            });
                        }
                    },
                }
            }
        };

        // A raw compressed stream may expose either libarchive's synthetic
        // `data` pathname or a stale original name from its gzip header.
        // Use the current source name for wrapped TAR files and as the
        // fallback for unnamed streams so listing, selection, and
        // extraction agree on the useful payload name.
        if self.format_name().eq_ignore_ascii_case("raw")
            && (path == Path::new("data") || self.prefer_source_payload_name)
            && let Some(payload_name) = &self.standalone_payload_name
        {
            path.clone_from(payload_name);
        }

        // SAFETY: all accessors borrow immutable fields from `entry`.
        let (size, filetype, modified, hardlink, encrypted) = unsafe {
            let size = if (self.api.entry_size_is_set)(entry) != 0 {
                u64::try_from((self.api.entry_size)(entry)).ok()
            } else {
                None
            };
            let modified = if (self.api.entry_mtime_is_set)(entry) != 0 {
                Some((self.api.entry_mtime)(entry))
            } else {
                None
            };
            (
                size,
                (self.api.entry_filetype)(entry) & AE_IFMT,
                modified,
                (self.api.entry_hardlink_is_set)(entry) != 0,
                self.api
                    .entry_is_encrypted
                    .is_some_and(|getter| getter(entry) > 0),
            )
        };

        let kind = if hardlink {
            ArchiveEntryKind::Hardlink
        } else {
            match filetype {
                AE_IFREG => ArchiveEntryKind::File,
                AE_IFDIR => ArchiveEntryKind::Directory,
                AE_IFLNK => ArchiveEntryKind::Symlink,
                _ => ArchiveEntryKind::Other,
            }
        };
        let display_path = path.to_string_lossy().into_owned();
        Ok(RawEntryInfo {
            path,
            display_path,
            size,
            modified_unix_seconds: modified,
            kind,
            encrypted,
        })
    }

    pub(super) fn read(&mut self, buffer: &mut [u8]) -> ArchiveResult<usize> {
        // SAFETY: buffer is writable for its length and handle is on entry data.
        let amount = unsafe {
            (self.api.read_data)(
                self.raw.as_ptr(),
                buffer.as_mut_ptr().cast::<c_void>(),
                buffer.len(),
            )
        };
        if amount < 0 {
            Err(self.error("reading archive entry data"))
        } else {
            Ok(amount as usize)
        }
    }

    pub(super) fn drain_current_entry(
        &mut self,
        buffer: &mut [u8],
        cancel: &CancellationToken,
        scan: &mut ScanBudget,
        mut on_chunk: impl FnMut(u64, u64),
    ) -> ArchiveResult<u64> {
        let mut entry_bytes = 0u64;
        loop {
            check_cancel(cancel)?;
            let amount = self.read(buffer)?;
            if amount == 0 {
                return Ok(entry_bytes);
            }
            scan.account_decoded(amount)?;
            entry_bytes = entry_bytes.checked_add(amount as u64).ok_or_else(|| {
                ArchiveError::LimitExceeded("entry scan byte count overflow".to_owned())
            })?;
            on_chunk(entry_bytes, self.consumed_bytes());
        }
    }

    pub(super) fn consumed_bytes(&self) -> u64 {
        // -1 is the client-facing/raw stream filter.
        let value = unsafe { (self.api.filter_bytes)(self.raw.as_ptr(), -1) };
        u64::try_from(value).unwrap_or(0)
    }

    pub(super) fn format_name(&self) -> String {
        // SAFETY: returned strings live until the reader is freed.
        unsafe { copy_c_string((self.api.format_name)(self.raw.as_ptr())) }
            .unwrap_or_else(|| "unknown".to_owned())
    }

    pub(super) fn filter_name(&self) -> Option<String> {
        // SAFETY: returned strings live until the reader is freed.
        let name = unsafe { copy_c_string((self.api.filter_name)(self.raw.as_ptr(), 0)) }?;
        (!name.eq_ignore_ascii_case("none")).then_some(name)
    }

    pub(super) fn has_encrypted_entries(&self) -> bool {
        self.api.read_has_encrypted_entries.is_some_and(|getter| {
            // SAFETY: the getter only examines reader state.
            unsafe { getter(self.raw.as_ptr()) > 0 }
        })
    }

    pub(super) fn finish(&mut self) -> ArchiveResult<()> {
        if self.closed {
            return Ok(());
        }
        // SAFETY: handle is live and close is called once explicitly.
        let status = unsafe { (self.api.read_close)(self.raw.as_ptr()) };
        self.closed = true;
        self.require_ok(status, "closing archive")
    }

    fn require_ok(&self, status: c_int, operation: &'static str) -> ArchiveResult<()> {
        if status >= ARCHIVE_OK {
            Ok(())
        } else {
            Err(self.error(operation))
        }
    }

    fn error(&self, operation: &'static str) -> ArchiveError {
        self.api.last_error(self.raw.as_ptr(), operation)
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        // SAFETY: reader uniquely owns the handle; free also closes if needed.
        let _ = unsafe { (self.api.read_free)(self.raw.as_ptr()) };
    }
}
