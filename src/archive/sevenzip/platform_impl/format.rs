//! Archive signature detection and ZIP pathname metadata decoding.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadFormat {
    SevenZip,
    Zip,
    Lzh,
    Rar4,
    Rar5,
    Iso,
    SevenZipVolume,
    ZipVolume,
}

impl ReadFormat {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::SevenZip => "7z",
            Self::Zip => "ZIP",
            Self::Lzh => "LZH",
            Self::Rar4 | Self::Rar5 => "RAR",
            Self::Iso => "ISO 9660",
            Self::SevenZipVolume => "7z split volume",
            Self::ZipVolume => "ZIP split volume",
        }
    }

    pub(super) fn base(self) -> Self {
        match self {
            Self::SevenZipVolume => Self::SevenZip,
            Self::ZipVolume => Self::Zip,
            other => other,
        }
    }

    pub(super) fn is_zip(self) -> bool {
        matches!(self, Self::Zip | Self::ZipVolume)
    }

    pub(super) fn is_volume(self) -> bool {
        matches!(self, Self::SevenZipVolume | Self::ZipVolume)
    }
}

// ------------------------------------------------------------------
// Shared helpers
// ------------------------------------------------------------------

fn split_volume_base(path: &Path) -> Option<(PathBuf, u32)> {
    let name = path.file_name()?.to_str()?;
    let dot = name.rfind('.')?;
    let suffix = &name[dot + 1..];
    if suffix.len() < 3 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index = suffix.parse::<u32>().ok()?;
    (index > 0).then(|| (path.with_file_name(&name[..dot]), index))
}

pub(super) fn volume_part_path(base: &Path, index: u32) -> PathBuf {
    let mut name = base
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_else(|| OsString::from("archive"));
    name.push(format!(".{index:03}"));
    base.with_file_name(name)
}

fn split_archive_format(path: &Path) -> Option<ReadFormat> {
    let (base, _) = split_volume_base(path)?;
    match base.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("7z") => Some(ReadFormat::SevenZipVolume),
        Some(extension) if extension.eq_ignore_ascii_case("zip") => Some(ReadFormat::ZipVolume),
        _ => None,
    }
}

pub(super) fn split_volume_paths(path: &Path) -> Option<Vec<PathBuf>> {
    let (base, _) = split_volume_base(path)?;
    let first = volume_part_path(&base, 1);
    if !first.is_file() {
        return None;
    }
    let mut paths = Vec::new();
    for index in 1..=u32::from(u16::MAX) {
        let candidate = volume_part_path(&base, index);
        if !candidate.is_file() {
            break;
        }
        paths.push(candidate);
    }
    (!paths.is_empty()).then_some(paths)
}

pub(super) fn archive_format(path: &Path) -> Option<ReadFormat> {
    if let Some(format) = split_archive_format(path) {
        return Some(format);
    }
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "iso" | "img"))
    {
        return Some(ReadFormat::Iso);
    }
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "lha" | "lzh"))
    {
        return Some(ReadFormat::Lzh);
    }
    let Ok(mut file) = File::open(path) else {
        return None;
    };
    let mut signature = [0u8; 8];
    let Ok(amount) = file.read(&mut signature) else {
        return None;
    };
    if amount >= SEVENZIP_SIGNATURE.len()
        && signature[..SEVENZIP_SIGNATURE.len()] == SEVENZIP_SIGNATURE
    {
        return Some(ReadFormat::SevenZip);
    }
    if amount >= 4
        && signature[0] == b'P'
        && signature[1] == b'K'
        && matches!(signature[2], 0x03 | 0x05 | 0x07)
        && matches!(signature[3], 0x04 | 0x06 | 0x08)
    {
        return Some(ReadFormat::Zip);
    }
    if amount >= 7 && signature[..7] == *b"Rar!\x1a\x07\x00" {
        return Some(ReadFormat::Rar4);
    }
    if amount >= 8 && signature == *b"Rar!\x1a\x07\x01\x00" {
        return Some(ReadFormat::Rar5);
    }
    if amount >= 7
        && signature[2] == b'-'
        && signature[3] == b'l'
        && signature[4] == b'h'
        && matches!(signature[5], b'0'..=b'7' | b'd')
        && signature[6] == b'-'
    {
        return Some(ReadFormat::Lzh);
    }
    None
}

const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4B50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4B50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4B50;
const ZIP_CENTRAL_SIGNATURE: u32 = 0x0201_4B50;
const ZIP_EOCD_SIZE: usize = 22;
const ZIP_MAX_COMMENT_SIZE: u64 = 65_535;

pub(super) struct ZipNameRecord {
    raw_name: Vec<u8>,
    flags: u16,
    unicode_name: Option<String>,
}

struct ZipDirectoryLayout {
    entries: u64,
    offset: u64,
    size: u64,
}

/// Determines the code page used for legacy ZIP names.  The ZIP format
/// does not declare a code page for names without the UTF-8 flag, so
/// automatic mode samples the raw central-directory names with the
/// detector shared by the libarchive backend.
pub(super) fn effective_zip_codepage(
    format: ReadFormat,
    requested: u32,
    records: Option<&[ZipNameRecord]>,
) -> u32 {
    if !format.is_zip() || requested != 0 {
        return requested;
    }
    records.map(detect_zip_codepage).unwrap_or(0)
}

fn detect_zip_codepage(records: &[ZipNameRecord]) -> u32 {
    let mut weights = BTreeMap::<u32, u64>::new();
    for record in records {
        if record.flags & 0x0800 != 0 || record.unicode_name.is_some() {
            continue;
        }
        let detected = encoding::detect(&record.raw_name);
        if matches!(
            detected,
            encoding::DetectedEncoding::Utf8
                | encoding::DetectedEncoding::Utf16Le
                | encoding::DetectedEncoding::Utf16Be
        ) {
            continue;
        }
        let weight = record
            .raw_name
            .iter()
            .filter(|byte| **byte >= 0x80)
            .count()
            .max(1) as u64;
        *weights.entry(detected.codepage()).or_default() += weight;
    }

    weights
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(codepage, _)| codepage)
        .unwrap_or(0)
}

pub(super) fn read_zip_name_records(path: &Path) -> ArchiveResult<Option<Vec<ZipNameRecord>>> {
    let mut file = File::open(path).map_err(|error| ArchiveError::io(path, error))?;
    let file_length = file
        .metadata()
        .map_err(|error| ArchiveError::io(path, error))?
        .len();
    if file_length < ZIP_EOCD_SIZE as u64 {
        return Ok(None);
    }

    let tail_length = file_length.min(ZIP_EOCD_SIZE as u64 + ZIP_MAX_COMMENT_SIZE);
    file.seek(SeekFrom::Start(file_length - tail_length))
        .map_err(|error| ArchiveError::io(path, error))?;
    let mut tail = Vec::with_capacity(tail_length as usize);
    file.read_to_end(&mut tail)
        .map_err(|error| ArchiveError::io(path, error))?;
    let Some(eocd_index) = find_zip_eocd(&tail) else {
        return Ok(None);
    };
    let eocd_offset = file_length - tail_length + eocd_index as u64;
    let Some(layout) =
        zip_directory_layout(&mut file, file_length, eocd_offset, &tail[eocd_index..])?
    else {
        return Ok(None);
    };
    if layout.entries > MAX_LIST_ENTRIES
        || layout
            .offset
            .checked_add(layout.size)
            .is_none_or(|end| end > file_length)
    {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(layout.offset))
        .map_err(|error| ArchiveError::io(path, error))?;
    let mut records = Vec::with_capacity(layout.entries as usize);
    let mut consumed = 0u64;
    for _ in 0..layout.entries {
        if layout.size.saturating_sub(consumed) < 46 {
            return Ok(None);
        }
        let mut header = [0u8; 46];
        file.read_exact(&mut header)
            .map_err(|error| ArchiveError::io(path, error))?;
        consumed += 46;
        if read_u32(&header, 0) != Some(ZIP_CENTRAL_SIGNATURE) {
            return Ok(None);
        }

        let flags = read_u16(&header, 8).unwrap_or(0);
        let name_length = u64::from(read_u16(&header, 28).unwrap_or(0));
        let extra_length = u64::from(read_u16(&header, 30).unwrap_or(0));
        let comment_length = u64::from(read_u16(&header, 32).unwrap_or(0));
        let variable_length = name_length
            .checked_add(extra_length)
            .and_then(|length| length.checked_add(comment_length))
            .ok_or_else(|| {
                ArchiveError::LimitExceeded("ZIP central-directory length overflow".to_owned())
            })?;
        if variable_length > layout.size.saturating_sub(consumed) {
            return Ok(None);
        }

        let mut raw_name = vec![0u8; name_length as usize];
        file.read_exact(&mut raw_name)
            .map_err(|error| ArchiveError::io(path, error))?;
        let mut extra = vec![0u8; extra_length as usize];
        file.read_exact(&mut extra)
            .map_err(|error| ArchiveError::io(path, error))?;
        if comment_length > 0 {
            file.seek(SeekFrom::Current(comment_length as i64))
                .map_err(|error| ArchiveError::io(path, error))?;
        }
        consumed += variable_length;
        records.push(ZipNameRecord {
            unicode_name: unicode_path_extra(&raw_name, &extra),
            raw_name,
            flags,
        });
    }

    Some(records)
        .filter(|records| records.len() as u64 == layout.entries)
        .map_or(Ok(None), |records| Ok(Some(records)))
}

fn decode_zip_name(record: &ZipNameRecord, codepage: u32) -> Option<String> {
    if let Some(unicode_name) = &record.unicode_name {
        return Some(unicode_name.clone());
    }
    if record.flags & 0x0800 != 0 {
        return Some(String::from_utf8_lossy(&record.raw_name).into_owned());
    }
    encoding::decode_name_with_codepage(&record.raw_name, codepage)
}

pub(super) fn apply_zip_name_records(
    entries: &mut [ArchiveEntry],
    records: Option<&[ZipNameRecord]>,
    codepage: u32,
) -> ArchiveResult<()> {
    let Some(records) = records.filter(|records| records.len() == entries.len()) else {
        return Ok(());
    };

    let mut total_path_bytes = 0u64;
    for (entry, record) in entries.iter_mut().zip(records) {
        let Some(display_path) = decode_zip_name(record, codepage) else {
            continue;
        };
        let Ok(path) = build_path(&display_path) else {
            continue;
        };
        total_path_bytes = checked_add_with_limit(
            total_path_bytes,
            (display_path.encode_utf16().count() as u64).saturating_mul(2),
            MAX_LIST_PATH_BYTES,
            "7z ZIP listing pathname metadata",
        )?;
        entry.path = path;
        entry.display_path = display_path;
    }
    Ok(())
}

fn find_zip_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < ZIP_EOCD_SIZE {
        return None;
    }
    (0..=tail.len() - ZIP_EOCD_SIZE).rev().find(|&index| {
        read_u32(tail, index) == Some(ZIP_EOCD_SIGNATURE)
            && read_u16(tail, index + 20).is_some_and(|comment_length| {
                index + ZIP_EOCD_SIZE + usize::from(comment_length) <= tail.len()
            })
    })
}

fn zip_directory_layout(
    file: &mut File,
    file_length: u64,
    eocd_offset: u64,
    eocd: &[u8],
) -> ArchiveResult<Option<ZipDirectoryLayout>> {
    if eocd.len() < ZIP_EOCD_SIZE {
        return Ok(None);
    }
    let disk = read_u16(eocd, 4).unwrap_or(u16::MAX);
    let central_disk = read_u16(eocd, 6).unwrap_or(u16::MAX);
    let entries_on_disk = read_u16(eocd, 8).unwrap_or(u16::MAX);
    let entries = read_u16(eocd, 10).unwrap_or(u16::MAX);
    let size = u64::from(read_u32(eocd, 12).unwrap_or(u32::MAX));
    let offset = u64::from(read_u32(eocd, 16).unwrap_or(u32::MAX));
    let needs_zip64 = disk == u16::MAX
        || central_disk == u16::MAX
        || entries_on_disk == u16::MAX
        || entries == u16::MAX
        || size == u64::from(u32::MAX)
        || offset == u64::from(u32::MAX);
    if !needs_zip64 {
        return Ok(Some(ZipDirectoryLayout {
            entries: u64::from(entries),
            offset,
            size,
        }));
    }

    if eocd_offset < 20 {
        return Ok(None);
    }
    let mut locator = [0u8; 20];
    file.seek(SeekFrom::Start(eocd_offset - 20))
        .map_err(|error| ArchiveError::io("ZIP64 locator", error))?;
    file.read_exact(&mut locator)
        .map_err(|error| ArchiveError::io("ZIP64 locator", error))?;
    if read_u32(&locator, 0) != Some(ZIP64_LOCATOR_SIGNATURE) || read_u32(&locator, 4) != Some(0) {
        return Ok(None);
    }
    let Some(zip64_offset) = read_u64(&locator, 8) else {
        return Ok(None);
    };
    if zip64_offset
        .checked_add(56)
        .is_none_or(|end| end > file_length)
    {
        return Ok(None);
    }
    let mut record = [0u8; 56];
    file.seek(SeekFrom::Start(zip64_offset))
        .map_err(|error| ArchiveError::io("ZIP64 end record", error))?;
    file.read_exact(&mut record)
        .map_err(|error| ArchiveError::io("ZIP64 end record", error))?;
    if read_u32(&record, 0) != Some(ZIP64_EOCD_SIGNATURE)
        || read_u64(&record, 4).is_none_or(|size| size < 44)
    {
        return Ok(None);
    }
    Ok(Some(ZipDirectoryLayout {
        entries: read_u64(&record, 32).unwrap_or(0),
        size: read_u64(&record, 40).unwrap_or(0),
        offset: read_u64(&record, 48).unwrap_or(0),
    }))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset.checked_add(2)?)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes.get(offset..offset.checked_add(8)?).map(|bytes| {
        u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    })
}

fn unicode_path_extra(raw_name: &[u8], extra: &[u8]) -> Option<String> {
    let mut offset = 0usize;
    while offset.checked_add(4).is_some_and(|end| end <= extra.len()) {
        let id = read_u16(extra, offset).unwrap_or(0);
        let length = usize::from(read_u16(extra, offset + 2).unwrap_or(0));
        let data_start = offset + 4;
        let Some(data_end) = data_start.checked_add(length) else {
            return None;
        };
        if data_end > extra.len() {
            return None;
        }
        if id == 0x7075
            && length >= 5
            && extra[data_start] == 1
            && crc32(raw_name) == read_u32(extra, data_start + 1).unwrap_or(0)
        {
            return std::str::from_utf8(&extra[data_start + 5..data_end])
                .ok()
                .map(str::to_owned);
        }
        offset = data_end;
    }
    None
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
