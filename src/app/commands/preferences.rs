//! Mapping persisted preferences to stable UI choices.

use super::*;

// Font choices offered in Settings. The "auto" entry resolves at startup to
// Noto Sans CJK JP when it is installed, otherwise to Yu Gothic.
pub(super) const FONT_OPTIONS: &[(&str, &str)] = &[
    ("Auto (Noto Sans CJK JP → Yu Gothic)", "auto"),
    ("Noto Sans CJK JP", "Noto Sans CJK JP"),
    ("Noto Sans CJK KR", "Noto Sans CJK KR"),
    ("Noto Sans KR", "Noto Sans KR"),
    ("Noto Sans JP", "Noto Sans JP"),
    ("Yu Gothic", "Yu Gothic"),
    ("Yu Gothic UI", "Yu Gothic UI"),
    ("Meiryo", "Meiryo"),
    ("Malgun Gothic", "Malgun Gothic"),
    ("Segoe UI", "Segoe UI"),
];

// Theme choices offered in Settings. The selection index maps to a stored
// registry value: 0 = follow the system, 1 = light, 2 = dark.
pub(super) fn theme_selection_index(preference: &str) -> i32 {
    match preference {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    }
}

pub(super) fn theme_registry_key(index: i32) -> &'static str {
    match index {
        1 => "light",
        2 => "dark",
        _ => "auto",
    }
}

pub(super) const LANGUAGE_OPTIONS: &[(&str, &str)] = &[
    ("Default", "default"),
    ("English", "en"),
    ("한국어", "ko"),
    ("日本語", "ja"),
];
pub(super) const PROJECT_GITHUB_URL: &str = "https://github.com/kirinonakar/ArchiveRclick";

pub(super) const CODEPAGE_OPTIONS: &[(&str, u32)] = &[
    ("Auto", 0),
    ("UTF-8", 65001),
    ("CP949 — Korean", 949),
    ("CP932 — Japanese", 932),
    ("CP936 — Simplified Chinese", 936),
    ("CP950 — Traditional Chinese", 950),
    ("CP1361 — Johab", 1361),
    ("CP50220 — ISO-2022-JP", 50220),
    ("CP54936 — GB18030", 54936),
    ("UTF-16 LE", 1200),
    ("UTF-16 BE", 1201),
];

pub(super) fn language_selection_index(preference: &str) -> i32 {
    match platform::resolve_language_preference(preference) {
        "ko" => 1,
        "ja" => 2,
        _ => 0,
    }
}

pub(super) fn language_preference_selection_index(preference: &str) -> i32 {
    LANGUAGE_OPTIONS
        .iter()
        .position(|(_, key)| *key == preference)
        .unwrap_or(0) as i32
}

pub(super) fn language_registry_key(index: i32) -> &'static str {
    LANGUAGE_OPTIONS
        .get(index.max(0) as usize)
        .map(|(_, key)| *key)
        .unwrap_or("default")
}

pub(super) fn pathname_codepage(index: i32) -> u32 {
    CODEPAGE_OPTIONS
        .get(index.max(0) as usize)
        .map(|(_, codepage)| *codepage)
        .unwrap_or(0)
}
