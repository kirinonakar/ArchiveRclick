//! Legacy text-encoding detection and decoding for archive pathnames.
//!
//! The heuristic in this module is a 1:1 port of the detector used by the
//! EncodingConverter utility (`main.cpp`): BOM sniffing, ISO-2022-JP escape
//! detection, strict UTF-8 validation, an HTML `<meta charset>` hint, and
//! byte-range scoring for EUC-KR, Shift_JIS, Johab, GBK, GB18030 and Big5.
//!
//! libarchive stores ZIP pathnames that carry neither the UTF-8 language
//! encoding flag (general purpose bit 11) nor the Info-ZIP Unicode Path
//! extra field (0x7075) as raw bytes.  Under a UTF-8 CRT locale those bytes
//! cannot be translated by libarchive, so this module recovers them and
//! decodes them with the Windows codepage tables (CP949, CP932, CP1361,
//! CP936, CP54936, CP950, CP50220) through the Win32 conversion API.

use windows::{
    Win32::Globalization::{
        CP_UTF8, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar, WideCharToMultiByte,
    },
    core::PCSTR,
};

/// Text encoding identified by [`detect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Sjis,
    Iso2022Jp,
    EucKr,
    Johab,
    Gbk,
    Gb18030,
    Big5,
}

impl DetectedEncoding {
    /// Windows code page used to decode this encoding (65001/1200/1201 for
    /// the Unicode variants, which [`decode_to_utf8`] handles separately).
    pub fn codepage(self) -> u32 {
        match self {
            Self::Utf8 => 65001,
            Self::Utf16Le => 1200,
            Self::Utf16Be => 1201,
            Self::Sjis => 932,
            Self::Iso2022Jp => 50220,
            Self::EucKr => 949,
            Self::Johab => 1361,
            Self::Gbk => 936,
            Self::Gb18030 => 54936,
            Self::Big5 => 950,
        }
    }
}

/// Decodes `bytes` to UTF-8 using `encoding`.
pub fn decode_to_utf8(bytes: &[u8], encoding: DetectedEncoding) -> Option<String> {
    match encoding {
        DetectedEncoding::Utf8 => Some(String::from_utf8_lossy(bytes).into_owned()),
        DetectedEncoding::Utf16Le => decode_utf16(bytes, true),
        DetectedEncoding::Utf16Be => decode_utf16(bytes, false),
        _ => decode_codepage(bytes, encoding.codepage()),
    }
}

/// Decodes a raw pathname byte sequence: detects the encoding first and then
/// converts it to UTF-8.  Never fails for non-empty input: undecodable bytes
/// are replaced with U+FFFD so a single odd entry cannot fail an archive
/// listing.
pub fn decode_name(bytes: &[u8]) -> Option<String> {
    // ISO-2022-JP is 7-bit ASCII and would pass the UTF-8 checks below, so
    // it must be tested first (same order as the reference detector).
    if has_jis_escape_sequence(bytes) {
        return decode_to_utf8(bytes, DetectedEncoding::Iso2022Jp);
    }
    // Strictly valid UTF-8 passes through unchanged.  The ported detector's
    // validator is deliberately lenient (it mirrors the C++ reference), so
    // run detection only for bytes that are not strictly valid UTF-8;
    // otherwise overlong/surrogate byte pairs from legacy CJK encodings
    // would be misdetected as UTF-8 and replaced with U+FFFD.
    if let Ok(name) = std::str::from_utf8(bytes) {
        return Some(name.to_owned());
    }
    decode_to_utf8(bytes, detect(bytes))
}

/// Detects the encoding of `bytes` with the EncodingConverter algorithm.
pub fn detect(bytes: &[u8]) -> DetectedEncoding {
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        return DetectedEncoding::Utf8;
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return DetectedEncoding::Utf16Le;
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        return DetectedEncoding::Utf16Be;
    }

    // ISO-2022-JP is 7-bit and would pass the UTF-8 check below, so it must
    // be tested first.
    if has_jis_escape_sequence(bytes) {
        return DetectedEncoding::Iso2022Jp;
    }

    if is_valid_utf8(bytes) {
        return DetectedEncoding::Utf8;
    }

    if let Some(encoding) = get_html_charset(bytes) {
        return encoding;
    }

    let euc_kr_score = get_euc_kr_score(bytes);
    let sjis_score = get_sjis_score(bytes);
    let johab_score = get_johab_score(bytes);
    let johab_marker_pair_count = count_johab_marker_pairs(bytes);
    let gbk_score = get_gbk_score(bytes);
    let gb18030_score = get_gb18030_score(bytes);
    let big5_score = get_big5_score(bytes);

    let max_score = [
        sjis_score,
        euc_kr_score,
        johab_score,
        gbk_score,
        gb18030_score,
        big5_score,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);

    if max_score > 0 {
        if max_score == euc_kr_score {
            return DetectedEncoding::EucKr;
        }
        if max_score == sjis_score {
            return DetectedEncoding::Sjis;
        }

        let chinese_score_is_winning = max_score == gbk_score
            || max_score == gb18030_score
            || max_score == big5_score;
        if chinese_score_is_winning {
            if should_prefer_johab_over_chinese_scores(
                bytes,
                johab_score,
                johab_marker_pair_count,
                gbk_score,
                gb18030_score,
                big5_score,
            ) {
                return DetectedEncoding::Johab;
            }
            if should_prefer_euc_kr_over_chinese_scores(bytes, euc_kr_score) {
                return DetectedEncoding::EucKr;
            }
        }

        if max_score == gb18030_score && gb18030_score > gbk_score {
            return DetectedEncoding::Gb18030;
        }
        if max_score == gbk_score {
            return DetectedEncoding::Gbk;
        }
        if max_score == big5_score {
            return DetectedEncoding::Big5;
        }
        if max_score == johab_score {
            return DetectedEncoding::Johab;
        }
    }

    if johab_score > 0 || johab_marker_pair_count >= 2 {
        return DetectedEncoding::Johab;
    }

    // Reference detector default (Korean environment).
    DetectedEncoding::EucKr
}

fn is_valid_utf8(bytes: &[u8]) -> bool {
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b <= 0x7F {
            i += 1;
        } else if (0xC2..=0xDF).contains(&b) {
            if i + 1 >= len || !(0x80..=0xBF).contains(&bytes[i + 1]) {
                return false;
            }
            i += 2;
        } else if (0xE0..=0xEF).contains(&b) {
            if i + 2 >= len
                || !(0x80..=0xBF).contains(&bytes[i + 1])
                || !(0x80..=0xBF).contains(&bytes[i + 2])
            {
                return false;
            }
            i += 3;
        } else if (0xF0..=0xF4).contains(&b) {
            if i + 3 >= len
                || !(0x80..=0xBF).contains(&bytes[i + 1])
                || !(0x80..=0xBF).contains(&bytes[i + 2])
                || !(0x80..=0xBF).contains(&bytes[i + 3])
            {
                return false;
            }
            i += 4;
        } else {
            return false;
        }
    }
    true
}

fn has_jis_escape_sequence(bytes: &[u8]) -> bool {
    let len = bytes.len();
    let mut i = 0;
    while i + 2 < len {
        if bytes[i] == 0x1B {
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            if (b1 == 0x24 && (b2 == 0x40 || b2 == 0x42))
                || (b1 == 0x28 && (b2 == 0x42 || b2 == 0x4A || b2 == 0x49))
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn get_html_charset(bytes: &[u8]) -> Option<DetectedEncoding> {
    // The reference implementation reads at most 2048 bytes; min() also
    // fixes an out-of-bounds read for short buffers in the original.
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(relative) = head[search_from..].find("charset") {
        let index = search_from + relative + "charset".len();
        let mut rest = &head[index..];
        if let Some(after) = rest.strip_prefix('=') {
            rest = after;
        } else if let Some(after) = rest.strip_prefix('"') {
            rest = after;
        } else if let Some(after) = rest.strip_prefix('\'') {
            rest = after;
        }
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
            .collect();
        if name.is_empty() {
            search_from = index;
            continue;
        }
        return match name.as_str() {
            "shift_jis" | "sjis" | "x-sjis" => Some(DetectedEncoding::Sjis),
            "iso-2022-jp" | "jis" | "cp50220" | "cp50221" => Some(DetectedEncoding::Iso2022Jp),
            "euc-kr" | "cp949" => Some(DetectedEncoding::EucKr),
            "gbk" | "gb2312" | "cp936" => Some(DetectedEncoding::Gbk),
            "gb18030" | "cp54936" => Some(DetectedEncoding::Gb18030),
            "big5" | "cp950" | "big5-hkscs" => Some(DetectedEncoding::Big5),
            "utf-8" | "utf8" => Some(DetectedEncoding::Utf8),
            _ => None,
        };
    }
    None
}

fn get_sjis_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            continue;
        }
        if (0xA1..=0xDF).contains(&b) {
            if i + 1 < len && bytes[i + 1] < 0x80 {
                score += 1;
            }
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        if (0x81..=0x9F).contains(&b) || (0xE0..=0xFC).contains(&b) {
            if (0x40..=0x7E).contains(&b2) || (0x80..=0xFC).contains(&b2) {
                if b == 0x82 || b == 0x83 {
                    score += 5;
                } else {
                    score += 1;
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    score
}

fn get_euc_kr_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b1 = bytes[i];
        if b1 < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        // EUC-KR Hangul range: b1 in 0xB0-0xC8, b2 in 0xA1-0xFE.
        if (0xB0..=0xC8).contains(&b1) && (0xA1..=0xFE).contains(&b2) {
            score += 5;
            i += 2;
            continue;
        }
        // Penalty for typical Chinese character range in EUC-KR (0xC9-0xFD).
        if (0xC9..=0xFD).contains(&b1) && (0xA1..=0xFE).contains(&b2) {
            score -= 10;
            i += 2;
            continue;
        }
        i += 1;
    }
    score
}

fn get_johab_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        if (0x84..=0xD3).contains(&b) {
            if (0x5B..=0x60).contains(&b2) || (0x7B..=0x7E).contains(&b2) {
                score += 3;
                i += 2;
                continue;
            }
            if (0x41..=0x7E).contains(&b2) || (0x81..=0xFE).contains(&b2) {
                score += 1;
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    score
}

fn count_johab_marker_pairs(bytes: &[u8]) -> i32 {
    let mut count = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        if (0x84..=0xD3).contains(&b) {
            let johab_only_second = (0x5B..=0x60).contains(&b2) || (0x7B..=0x7E).contains(&b2);
            if johab_only_second {
                count += 1;
                i += 2;
                continue;
            }
            if (0x41..=0x7E).contains(&b2) || (0x81..=0xFE).contains(&b2) {
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    count
}

fn get_gbk_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b1 = bytes[i];
        if b1 < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        // GBK (CP936) double-byte range: first byte 0x81-0xFE, second byte
        // 0x40-0xFE excluding 0x7F.
        if (0x81..=0xFE).contains(&b1) && (0x40..=0xFE).contains(&b2) && b2 != 0x7F {
            // Highly common GB2312 Level 1 & 2 Hanzi and symbols.
            if (0xA1..=0xF7).contains(&b1) && (0xA1..=0xFE).contains(&b2) {
                // 0xC9-0xF7: Simplified-Chinese specific, not the common
                // Korean EUC-KR Hangul range.
                if b1 >= 0xC9 {
                    score += 5;
                } else {
                    score += 2;
                }
            } else {
                score += 1;
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    score
}

fn get_big5_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b1 = bytes[i];
        if b1 < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= len {
            break;
        }
        let b2 = bytes[i + 1];
        // Big5 (CP950) double-byte range: first byte 0xA1-0xF9, second byte
        // 0x40-0x7E or 0xA1-0xFE.
        if (0xA1..=0xF9).contains(&b1) && ((0x40..=0x7E).contains(&b2) || (0xA1..=0xFE).contains(&b2)) {
            // Big5 Level 1 (common Traditional Chinese characters).
            if (0xA4..=0xC6).contains(&b1) {
                if (0x40..=0x7E).contains(&b2) {
                    score += 5;
                } else {
                    score += 2;
                }
            } else {
                score += 1;
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    score
}

fn get_gb18030_score(bytes: &[u8]) -> i32 {
    let mut score = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b1 = bytes[i];
        if b1 < 0x80 {
            i += 1;
            continue;
        }

        if i + 3 < len {
            let b2 = bytes[i + 1];
            let b3 = bytes[i + 2];
            let b4 = bytes[i + 3];
            if (0x81..=0xFE).contains(&b1)
                && (0x30..=0x39).contains(&b2)
                && (0x81..=0xFE).contains(&b3)
                && (0x30..=0x39).contains(&b4)
            {
                score += 8;
                i += 4;
                continue;
            }
        }

        if i + 1 >= len {
            break;
        }
        let tb2 = bytes[i + 1];
        if (0x81..=0xFE).contains(&b1)
            && ((0x40..=0x7E).contains(&tb2) || (0x80..=0xFE).contains(&tb2))
        {
            if (0xB0..=0xF7).contains(&b1) && (0xA1..=0xFE).contains(&tb2) {
                score += 2;
            } else {
                score += 1;
            }
            i += 2;
            continue;
        }

        i += 1;
    }
    score
}

const PROFILE_SAMPLE_LIMIT: usize = 128 * 1024;

struct TextScriptProfile {
    hangul_count: i32,
    cjk_count: i32,
    bad_character_count: i32,
}

fn is_hangul(ch: u16) -> bool {
    (0xAC00..=0xD7A3).contains(&ch)
        || (0x1100..=0x11FF).contains(&ch)
        || (0x3130..=0x318F).contains(&ch)
}

fn is_cjk(ch: u16) -> bool {
    (0x4E00..=0x9FFF).contains(&ch) || (0x3400..=0x4DBF).contains(&ch)
}

fn get_text_script_profile(bytes: &[u8], codepage: u32) -> TextScriptProfile {
    let sample_length = bytes.len().min(PROFILE_SAMPLE_LIMIT);
    let mut profile = TextScriptProfile {
        hangul_count: 0,
        cjk_count: 0,
        bad_character_count: 0,
    };
    if sample_length > 0 {
        // SAFETY: read-only conversion; the returned length bounds the buffer.
        let wide_len = unsafe {
            MultiByteToWideChar(
                codepage,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                &bytes[..sample_length],
                None,
            )
        };
        if wide_len > 0 {
            let mut wide = vec![0u16; wide_len as usize];
            // SAFETY: `wide` has exactly the capacity the API requested.
            let written = unsafe {
                MultiByteToWideChar(
                    codepage,
                    MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                    &bytes[..sample_length],
                    Some(&mut wide),
                )
            };
            if written > 0 {
                for &ch in &wide[..written as usize] {
                    if is_hangul(ch) {
                        profile.hangul_count += 1;
                    } else if is_cjk(ch) {
                        profile.cjk_count += 1;
                    } else if ch == 0xFFFD || ch == u16::from(b'?') {
                        profile.bad_character_count += 1;
                    }
                }
            }
        }
    }
    profile
}

fn should_prefer_euc_kr_over_chinese_scores(bytes: &[u8], euc_kr_score: i32) -> bool {
    if euc_kr_score <= 0 {
        return false;
    }
    let profile = get_text_script_profile(bytes, 949);
    let required_hangul_count = if bytes.len() < 1024 { 8 } else { 32 };
    if profile.hangul_count < required_hangul_count {
        return false;
    }
    if profile.cjk_count * 3 > profile.hangul_count {
        return false;
    }
    profile.bad_character_count <= 2.max(profile.hangul_count / 6)
}

fn should_prefer_johab_over_chinese_scores(
    bytes: &[u8],
    johab_score: i32,
    johab_marker_pair_count: i32,
    gbk_score: i32,
    gb18030_score: i32,
    big5_score: i32,
) -> bool {
    if johab_score <= 0 {
        return false;
    }

    // Check the script profile for Johab (CP1361).
    let profile = get_text_script_profile(bytes, 1361);

    // If the system successfully decoded and analyzed the profile (meaning
    // we got some characters), trust it.
    if profile.hangul_count > 0 || profile.cjk_count > 0 || profile.bad_character_count > 0 {
        let required_hangul_count = if bytes.len() < 1024 { 4 } else { 16 };
        return profile.hangul_count >= required_hangul_count
            && profile.cjk_count * 3 <= profile.hangul_count
            && profile.bad_character_count <= 2.max(profile.hangul_count / 6);
    }

    // Fallback to marker pair logic only if the script profile could not be
    // computed.
    let required_marker_pairs = if bytes.len() < 1024 {
        1
    } else if bytes.len() >= 16 * 1024 {
        8
    } else {
        2
    };
    if johab_marker_pair_count >= required_marker_pairs {
        let chinese_score = gbk_score.max(gb18030_score).max(big5_score);
        return chinese_score <= 0 || i64::from(johab_score) * 4 >= i64::from(chinese_score) * 3;
    }

    false
}

fn decode_codepage(bytes: &[u8], codepage: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: read-only conversion; the caller bounds the input length.
    let wide_len = unsafe {
        MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None)
    };
    if wide_len <= 0 {
        // The bytes are not representable in this codepage at all; fall back
        // to a lossy copy so a single odd entry cannot fail the whole archive.
        return Some(String::from_utf8_lossy(bytes).into_owned());
    }
    let mut wide = vec![0u16; wide_len as usize];
    // SAFETY: `wide` has exactly the size the API requested.
    let written =
        unsafe { MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut wide)) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);

    // SAFETY: read-only conversion; a null default char means '?' fallback.
    // `Option<PCSTR>` does not implement `Param<PCSTR>` in windows-core 0.61,
    // so pass the null pointer directly.
    let utf8_len = unsafe { WideCharToMultiByte(CP_UTF8, 0, &wide, None, PCSTR::null(), None) };
    if utf8_len <= 0 {
        return None;
    }
    let mut utf8 = vec![0u8; utf8_len as usize];
    // SAFETY: `utf8` has exactly the size the API requested.
    let written =
        unsafe { WideCharToMultiByte(CP_UTF8, 0, &wide, Some(&mut utf8), PCSTR::null(), None) };
    if written <= 0 {
        return None;
    }
    utf8.truncate(written as usize);
    String::from_utf8(utf8).ok()
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        let unit = if little_endian {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        };
        units.push(unit);
        i += 2;
    }
    // Drop a leading byte-order mark if the caller passed the raw BOM bytes.
    if units.first() == Some(&0xFEFF) {
        units.remove(0);
    }
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_boms() {
        assert_eq!(detect(&[0xEF, 0xBB, 0xBF, b'a']), DetectedEncoding::Utf8);
        assert_eq!(detect(&[0xFF, 0xFE, 0x48, 0x00]), DetectedEncoding::Utf16Le);
        assert_eq!(detect(&[0xFE, 0xFF, 0x00, 0x48]), DetectedEncoding::Utf16Be);
    }

    #[test]
    fn decodes_utf16_name() {
        assert_eq!(
            decode_name(&[0xFF, 0xFE, 0x2E, 0x00, 0x74, 0x00, 0x78, 0x00, 0x74, 0x00]).as_deref(),
            Some(".txt")
        );
    }

    #[test]
    fn detects_jis_escape_before_utf8() {
        // ESC $ B "あい" ESC ( B  (ISO-2022-JP is 7-bit ASCII and would
        // otherwise pass the UTF-8 validation; 0x2422=あ, 0x2424=い).
        let jis = [0x1B, 0x24, 0x42, 0x24, 0x22, 0x24, 0x24, 0x1B, 0x28, 0x42];
        assert_eq!(detect(&jis), DetectedEncoding::Iso2022Jp);
        assert_eq!(decode_name(&jis).as_deref(), Some("あい"));
    }

    #[test]
    fn detects_and_decodes_euc_kr() {
        // "안녕하세요" in CP949 (요 = 0xBF 0xE4).
        let bytes = [0xBE, 0xC8, 0xB3, 0xE7, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4];
        assert_eq!(detect(&bytes), DetectedEncoding::EucKr);
        assert_eq!(decode_name(&bytes).as_deref(), Some("안녕하세요"));
    }

    #[test]
    fn detects_and_decodes_shift_jis() {
        // "こんにちは" in Shift_JIS.
        let bytes = [0x82, 0xB1, 0x82, 0xF1, 0x82, 0xC9, 0x82, 0xBF, 0x82, 0xCD];
        assert_eq!(detect(&bytes), DetectedEncoding::Sjis);
        assert_eq!(decode_name(&bytes).as_deref(), Some("こんにちは"));
    }

    #[test]
    fn detects_and_decodes_gbk() {
        // "中文" in GBK.
        let bytes = [0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(detect(&bytes), DetectedEncoding::Gbk);
        assert_eq!(decode_name(&bytes).as_deref(), Some("中文"));
    }

    #[test]
    fn detects_and_decodes_big5() {
        // "這" in Big5 (lead 0xA4..0xC6 with a 0x40..0x7E trail so the Big5
        // score out-ranks GBK for the same pair).
        let bytes = [0xB3, 0x6F];
        assert_eq!(detect(&bytes), DetectedEncoding::Big5);
        assert_eq!(decode_name(&bytes).as_deref(), Some("這"));
    }

    #[test]
    fn detects_johab_marker_pairs() {
        // Johab-only trail bytes 0x5B/0x5C after a 0x84 lead.
        let bytes = [0x84, 0x5B, 0x84, 0x5C];
        assert_eq!(detect(&bytes), DetectedEncoding::Johab);
        assert!(decode_name(&bytes).is_some());
    }

    #[test]
    fn detects_gb18030_four_byte_sequence() {
        let bytes = [0x81, 0x30, 0x81, 0x30];
        assert_eq!(detect(&bytes), DetectedEncoding::Gb18030);
        assert!(decode_name(&bytes).is_some());
    }

    #[test]
    fn ascii_passes_through() {
        assert_eq!(detect(b"hello.txt"), DetectedEncoding::Utf8);
        assert_eq!(decode_name(b"hello.txt").as_deref(), Some("hello.txt"));
    }

    #[test]
    fn honors_html_charset_hint() {
        let bytes = b"<meta charset=big5>\x81\x40\x81\x40";
        assert_eq!(detect(bytes), DetectedEncoding::Big5);
    }
}
