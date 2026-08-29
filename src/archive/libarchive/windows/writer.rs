//! RAII wrapper for configured libarchive writers.

use super::*;

pub(super) struct Writer<'a> {
    pub(super) api: &'a Api,
    pub(super) write: &'a WriteApi,
    pub(super) raw: NonNull<RawArchive>,
    pub(super) finished: bool,
}

impl<'a> Writer<'a> {
    pub(super) fn create(
        api: &'a Api,
        path: &Path,
        options: &CreateOptions,
    ) -> ArchiveResult<Self> {
        let write = api.write.as_ref().ok_or_else(|| {
            ArchiveError::UnsupportedOption(
                "this libarchive DLL does not expose the archive write API".to_owned(),
            )
        })?;
        // SAFETY: constructor has no preconditions and returns an owned handle.
        let raw = NonNull::new(unsafe { (write.write_new)() }).ok_or_else(|| {
            ArchiveError::LibArchive {
                operation: "creating archive writer",
                message: "archive_write_new returned null".to_owned(),
            }
        })?;
        let mut writer = Self {
            api,
            write,
            raw,
            finished: false,
        };
        writer.configure(options)?;
        let wide = wide_nul(path)?;
        writer.require_ok(
            // SAFETY: writer is configured and path is NUL-terminated.
            unsafe { (write.open_filename_w)(raw.as_ptr(), wide.as_ptr()) },
            "opening archive output",
        )?;
        Ok(writer)
    }

    fn configure(&mut self, options: &CreateOptions) -> ArchiveResult<()> {
        validate_compression_level(options)?;
        match options.format {
            CreateFormat::Zip => {
                self.call_unary(self.write.set_format_zip, "selecting ZIP format")?;
                // Write entry names as UTF-8 and set the ZIP language
                // encoding flag (general purpose bit 11), the standard
                // UTF-8 extension for ZIP headers.  Without this option
                // the writer falls back to the CRT locale and fails with
                // "Can't translate pathname to current locale".
                self.format_option("zip", "hdrcharset", "UTF-8")?;
                let method = if options.compression_level == 0 {
                    "store"
                } else {
                    "deflate"
                };
                self.format_option("zip", "compression", method)?;
                self.format_option(
                    "zip",
                    "compression-level",
                    &options.compression_level.to_string(),
                )?;
            }
            CreateFormat::SevenZip => {
                let setter = self.write.set_format_7zip.ok_or_else(|| {
                    ArchiveError::UnsupportedOption(
                        "this libarchive build cannot write 7z archives".to_owned(),
                    )
                })?;
                self.call_unary(setter, "selecting 7z format")?;
                self.format_option(
                    "7zip",
                    "compression",
                    if options.compression_level == 0 {
                        "copy"
                    } else {
                        "lzma2"
                    },
                )?;
                self.format_option(
                    "7zip",
                    "compression-level",
                    &options.compression_level.to_string(),
                )?;
            }
            CreateFormat::Tar
            | CreateFormat::TarGzip
            | CreateFormat::TarXz
            | CreateFormat::TarZstd => {
                self.call_unary(self.write.set_format_pax_restricted, "selecting TAR format")?;
                let filter = match options.format {
                    CreateFormat::Tar => None,
                    CreateFormat::TarGzip => Some((
                        "gzip",
                        self.write.add_filter_gzip,
                        options.compression_level.to_string(),
                    )),
                    CreateFormat::TarXz => Some((
                        "xz",
                        self.write.add_filter_xz,
                        options.compression_level.to_string(),
                    )),
                    CreateFormat::TarZstd => Some((
                        "zstd",
                        self.write.add_filter_zstd,
                        options.compression_level.to_string(),
                    )),
                    _ => unreachable!(),
                };
                if let Some((name, setter, level)) = filter {
                    let setter = setter.ok_or_else(|| {
                        ArchiveError::UnsupportedOption(format!(
                            "this libarchive build has no {name} writer"
                        ))
                    })?;
                    self.call_unary(setter, "enabling compression filter")?;
                    self.filter_option(name, "compression-level", &level)?;
                }
            }
        }

        if let Some(password) = options.password.as_deref() {
            if options.format != CreateFormat::Zip {
                return Err(ArchiveError::UnsupportedOption(
                    "libarchive password creation is supported here only for ZIP".to_owned(),
                ));
            }
            let setter = self.write.set_passphrase.ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "this libarchive build has no encrypted ZIP writer".to_owned(),
                )
            })?;
            let password = secret_c_string(password)?;
            self.format_option("zip", "encryption", "aes256")?;
            self.require_ok(
                // SAFETY: libarchive copies the password during this call.
                unsafe { setter(self.raw.as_ptr(), password.as_ptr()) },
                "setting archive password",
            )?;
        }
        Ok(())
    }

    pub(super) fn write_header(&mut self, item: &SourceItem) -> ArchiveResult<()> {
        // SAFETY: constructor returns a uniquely owned entry.
        let entry = NonNull::new(unsafe { (self.write.entry_new)() }).ok_or_else(|| {
            ArchiveError::LibArchive {
                operation: "allocating archive entry",
                message: "archive_entry_new returned null".to_owned(),
            }
        })?;
        let guard = EntryGuard {
            raw: entry,
            free: self.write.entry_free,
        };
        let mut name = item.archive_name.clone();
        if item.kind == SourceKind::Directory && !name.ends_with('/') {
            name.push('/');
        }
        if name.contains('\0') {
            return Err(ArchiveError::InvalidInput(
                "archive pathname contains NUL".to_owned(),
            ));
        }
        // The wide API keeps the entry's internal copy independent of the
        // CRT locale (the default "C" locale cannot translate UTF-8
        // names); ZIP's `hdrcharset=UTF-8` option then writes the name
        // as UTF-8 with the language-encoding flag set.
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        wide.push(0);
        let raw_entry = guard.raw.as_ptr();
        // SAFETY: setters copy scalar/string metadata into the live entry.
        unsafe {
            (self.write.entry_copy_pathname_w)(raw_entry, wide.as_ptr());
            (self.write.entry_set_filetype)(
                raw_entry,
                u32::from(match item.kind {
                    SourceKind::File => AE_IFREG,
                    SourceKind::Directory => AE_IFDIR,
                }),
            );
            (self.write.entry_set_perm)(
                raw_entry,
                match item.kind {
                    SourceKind::File => 0o644,
                    SourceKind::Directory => 0o755,
                },
            );
            (self.write.entry_set_size)(raw_entry, item.size as i64);
            if let Some(mtime) = item.modified_unix_seconds {
                (self.write.entry_set_mtime)(raw_entry, mtime, 0);
            }
        }
        self.require_ok(
            // SAFETY: writer is ready for a header and entry remains live.
            unsafe { (self.write.write_header)(self.raw.as_ptr(), raw_entry) },
            "writing archive header",
        )
    }

    pub(super) fn write_all(&mut self, mut bytes: &[u8]) -> ArchiveResult<()> {
        while !bytes.is_empty() {
            // SAFETY: input slice is readable and writer is in DATA state.
            let amount = unsafe {
                (self.write.write_data)(
                    self.raw.as_ptr(),
                    bytes.as_ptr().cast::<c_void>(),
                    bytes.len(),
                )
            };
            if amount <= 0 || amount as usize > bytes.len() {
                return Err(self.error("writing archive data"));
            }
            bytes = &bytes[amount as usize..];
        }
        Ok(())
    }

    pub(super) fn finish_entry(&mut self) -> ArchiveResult<()> {
        self.call_unary(self.write.finish_entry, "finishing archive entry")
    }

    pub(super) fn finish(&mut self) -> ArchiveResult<()> {
        let status = unsafe { (self.write.close)(self.raw.as_ptr()) };
        self.finished = true;
        self.require_ok(status, "closing archive output")
    }

    pub(super) fn call_unary(
        &mut self,
        function: ArchiveWriteUnary,
        operation: &'static str,
    ) -> ArchiveResult<()> {
        // SAFETY: caller selects a function valid for the current writer state.
        self.require_ok(unsafe { function(self.raw.as_ptr()) }, operation)
    }

    fn format_option(&self, module: &str, option: &str, value: &str) -> ArchiveResult<()> {
        self.option(self.write.set_format_option, module, option, value)
    }

    fn filter_option(&self, module: &str, option: &str, value: &str) -> ArchiveResult<()> {
        self.option(self.write.set_filter_option, module, option, value)
    }

    pub(super) fn option(
        &self,
        setter: ArchiveWriteOption,
        module: &str,
        option: &str,
        value: &str,
    ) -> ArchiveResult<()> {
        let module = CString::new(module).expect("static module contains no NUL");
        let option = CString::new(option).expect("static option contains no NUL");
        let value = CString::new(value)
            .map_err(|_| ArchiveError::InvalidInput("archive option contains NUL".to_owned()))?;
        // SAFETY: all C strings live for the call; writer is in NEW state.
        let status = unsafe {
            setter(
                self.raw.as_ptr(),
                module.as_ptr(),
                option.as_ptr(),
                value.as_ptr(),
            )
        };
        if status == ARCHIVE_OK {
            Ok(())
        } else {
            Err(ArchiveError::UnsupportedOption(format!(
                "{module:?} option {option:?}: {}",
                self.api
                    .last_error(self.raw.as_ptr(), "setting archive option")
            )))
        }
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

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // `archive_write_fail` aborts the writer state, but some
            // Windows builds leave the output handle open until the
            // normal close path runs.  Always close before freeing so
            // TemporaryPath can remove a cancelled/failed output file.
            let _ = unsafe { (self.write.close)(self.raw.as_ptr()) };
            self.finished = true;
        }
        // SAFETY: writer uniquely owns this handle.
        let _ = unsafe { (self.write.free)(self.raw.as_ptr()) };
    }
}

pub(super) struct EntryGuard {
    pub(super) raw: NonNull<RawEntry>,
    pub(super) free: ArchiveEntryFree,
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        // SAFETY: guard uniquely owns this entry.
        unsafe { (self.free)(self.raw.as_ptr()) };
    }
}
