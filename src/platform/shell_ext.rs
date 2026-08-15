//! Explorer context-menu shell extension (IContextMenu + IExplorerCommand).
//!
//! Static registry verbs cannot show per-file labels, so this DLL computes
//! the menu items from the selected paths and displays the real names, e.g.
//! "보고서.zip으로 압축하기", "보고서.7z로 압축하기" and "보고서\ 에 풀기".
//! IContextMenu feeds the classic menu, Windows 11's "추가 옵션 표시", and
//! the menu shown after a right-drag onto a folder; separate IExplorerCommand
//! classes show the same verbs as top-level items in the Windows 11 default
//! menu.
//! Invoking an item launches archive-rclick.exe directly with Unicode
//! arguments (no PowerShell, no cmd), and the app shows its progress window.
#![cfg(windows)]

use std::{
    ffi::{OsString, c_void},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
    sync::atomic::{AtomicUsize, Ordering},
};

use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HMODULE},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HGDIOBJ, SelectObject,
        },
        System::{
            Com::{
                CoTaskMemAlloc, CoTaskMemFree, FORMATETC, IBindCtx, IClassFactory,
                IClassFactory_Impl, IDataObject, STGMEDIUM,
            },
            LibraryLoader::{
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
                GetModuleHandleExW,
            },
            Ole::ReleaseStgMedium,
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RegCloseKey,
                RegCreateKeyW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW,
            },
        },
        UI::{
            Shell::{
                CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, DragQueryFileW, ECS_ENABLED, ECS_HIDDEN,
                ExtractIconExW, HDROP, IContextMenu, IContextMenu_Impl, IEnumExplorerCommand,
                IExplorerCommand, IExplorerCommand_Impl, IShellExtInit, IShellExtInit_Impl,
                IShellItem, IShellItemArray, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify,
                SHCreateItemFromIDList, SIGDN_FILESYSPATH, ShellExecuteW,
            },
            WindowsAndMessaging::{
                DI_NORMAL, DestroyIcon, DrawIconEx, GetSystemMetrics, HICON, HMENU, InsertMenuW,
                MENUITEMINFOW, MF_BYPOSITION, MF_STRING, MIIM_BITMAP, SM_CXMENUCHECK,
                SM_CYMENUCHECK, SW_SHOWNORMAL, SetMenuItemInfoW,
            },
        },
    },
    core::{BOOL, GUID, HSTRING, IUnknown, Interface, PCWSTR, PSTR, PWSTR, Ref, implement, w},
};

use windows::core::Result as WinResult;

use crate::{
    app::commands::{cli_archive_destination, unique_path},
    archive::CreateFormat,
    platform::windows::quote_windows_arg,
};

const CLSID_SHELL_EXT: GUID = GUID {
    data1: 0x6B2B1C4A,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};
const CLSID_EXTRACT_COMMAND: GUID = GUID {
    data1: 0x6B2B1C4B,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};
const CLSID_ZIP_COMMAND: GUID = GUID {
    data1: 0x6B2B1C4C,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};
const CLSID_SEVENZIP_COMMAND: GUID = GUID {
    data1: 0x6B2B1C4D,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};
const CLSID_ZIP_EACH_COMMAND: GUID = GUID {
    data1: 0x6B2B1C4E,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};
const CLSID_SEVENZIP_EACH_COMMAND: GUID = GUID {
    data1: 0x6B2B1C4F,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};

const ARCHIVE_EXTENSIONS: &[&str] = &[
    ".zip", ".zipx", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz", ".zst", ".cab", ".lha", ".lzh",
    ".tgz", ".tbz2", ".txz", ".iso",
];
const EXE_NAME: &str = "archive-rclick.exe";
const SETTINGS_KEY: &str = r"Software\ArchiveRclick";
const EXE_PATH_VALUE: &str = "ExePath";
const LEGACY_MODERN_MENU_KEY: &str = r"Software\Classes\*\shell\ArchiveRclick";
const LEGACY_MODERN_DIRECTORY_MENU_KEY: &str = r"Software\Classes\Directory\shell\ArchiveRclick";
const MODERN_EXTRACT_MENU_KEY: &str = r"Software\Classes\*\shell\ArchiveRclickExtract";
const MODERN_EXTRACT_DIRECTORY_MENU_KEY: &str =
    r"Software\Classes\Directory\shell\ArchiveRclickExtract";
const MODERN_ZIP_MENU_KEY: &str = r"Software\Classes\*\shell\ArchiveRclickZip";
const MODERN_ZIP_DIRECTORY_MENU_KEY: &str = r"Software\Classes\Directory\shell\ArchiveRclickZip";
const MODERN_SEVENZIP_MENU_KEY: &str = r"Software\Classes\*\shell\ArchiveRclickSevenZip";
const MODERN_SEVENZIP_DIRECTORY_MENU_KEY: &str =
    r"Software\Classes\Directory\shell\ArchiveRclickSevenZip";
const MODERN_ZIP_EACH_MENU_KEY: &str = r"Software\Classes\*\shell\ArchiveRclickZipEach";
const MODERN_ZIP_EACH_DIRECTORY_MENU_KEY: &str =
    r"Software\Classes\Directory\shell\ArchiveRclickZipEach";
const MODERN_SEVENZIP_EACH_MENU_KEY: &str =
    r"Software\Classes\*\shell\ArchiveRclickSevenZipEach";
const MODERN_SEVENZIP_EACH_DIRECTORY_MENU_KEY: &str =
    r"Software\Classes\Directory\shell\ArchiveRclickSevenZipEach";
const LEGACY_CONTEXT_MENU_KEY: &str =
    r"Software\Classes\*\shellex\ContextMenuHandlers\ArchiveRclick";
const LEGACY_DIRECTORY_CONTEXT_MENU_KEY: &str =
    r"Software\Classes\Directory\shellex\ContextMenuHandlers\ArchiveRclick";
// Older builds briefly used the Background branch. Remove it during the next
// registration so Explorer is left with only the actual drag-and-drop key.
const LEGACY_BACKGROUND_DRAG_DROP_HANDLER_KEY: &str =
    r"Software\Classes\Directory\Background\shellex\DragDropHandlers\ArchiveRclick";
const DRAG_DROP_HANDLER_KEY: &str =
    r"Software\Classes\Directory\shellex\DragDropHandlers\ArchiveRclick";
const EXPLORER_COMMAND_HANDLER_VALUE: &str = "ExplorerCommandHandler";

// IContextMenu::InvokeCommand receives the zero-based command offset in lpVerb.
// The visible verbs are dynamic, so keep the actual menu order per instance
// instead of assuming fixed numeric offsets.

// HRESULT constants kept local to avoid feature-flag surprises.
const S_OK: windows::core::HRESULT = windows::core::HRESULT(0);
const S_FALSE: windows::core::HRESULT = windows::core::HRESULT(1);
const E_POINTER: windows::core::HRESULT = windows::core::HRESULT(0x8000_4003u32 as i32);
const E_NOINTERFACE: windows::core::HRESULT = windows::core::HRESULT(0x8000_4002u32 as i32);
const E_NOTIMPL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4001u32 as i32);
const E_FAIL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005u32 as i32);
const E_OUTOFMEMORY: windows::core::HRESULT = windows::core::HRESULT(0x8007_000Eu32 as i32);
const CLASS_E_CLASSNOTAVAILABLE: windows::core::HRESULT =
    windows::core::HRESULT(0x8004_0111u32 as i32);
const GCS_VERBW: u32 = 4;
const GCS_HELPTEXTW: u32 = 5;

/// Number of COM objects the host still holds, plus IClassFactory locks.
/// `DllCanUnloadNow` must answer "no" while this is non-zero: saying "yes"
/// lets Explorer unload this DLL underneath live objects and crash on the
/// next call into the freed code.
static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

fn increment_live_objects() {
    LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
}

/// Underflow-guarded decrement: an object whose creation failed its
/// QueryInterface is released without ever being counted, so its Drop must
/// not take the counter below zero.
fn decrement_live_objects() {
    let _ = LIVE_OBJECTS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        count.checked_sub(1)
    });
}

#[implement(IShellExtInit, IContextMenu)]
struct ArchiveContextMenu {
    paths: Mutex<Vec<OsString>>,
    // The folder receiving a right-drag. It is supplied through
    // IShellExtInit::Initialize's pidlFolder and is only populated for the
    // drag-and-drop handler registration.
    target_folder: Mutex<Option<PathBuf>>,
    active_verbs: Mutex<Vec<Verb>>,
    // Classic-menu bitmap shown in front of the verbs; released in Drop.
    menu_bitmap: Mutex<Option<HBITMAP>>,
}

impl Drop for ArchiveContextMenu {
    fn drop(&mut self) {
        // The classic menu keeps using the bitmap until it is destroyed, and
        // Explorer destroys the menu before releasing this handler, so Drop
        // is the right moment to release the bitmap.
        if let Some(bitmap) = self
            .menu_bitmap
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            // SAFETY: the bitmap was created by us and the menu referencing it
            // is gone by the time the handler is released.
            let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        }
        decrement_live_objects();
    }
}

impl ArchiveContextMenu {
    fn new() -> Self {
        Self {
            paths: Mutex::new(Vec::new()),
            target_folder: Mutex::new(None),
            active_verbs: Mutex::new(Vec::new()),
            menu_bitmap: Mutex::new(None),
        }
    }

    fn selected_paths(&self) -> Vec<OsString> {
        self.paths
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default()
    }

    fn target_folder(&self) -> Option<PathBuf> {
        self.target_folder
            .lock()
            .ok()
            .and_then(|path| path.clone())
    }
}

impl IShellExtInit_Impl for ArchiveContextMenu_Impl {
    fn Initialize(
        &self,
        pidl_folder: *const ITEMIDLIST,
        data_obj: Ref<'_, IDataObject>,
        _hkey_prog_id: HKEY,
    ) -> WinResult<()> {
        if let Ok(mut guard) = self.target_folder.lock() {
            *guard = item_id_list_filesystem_path(pidl_folder).map(PathBuf::from);
        }
        let paths = data_obj
            .as_ref()
            .and_then(|data| read_hdrop_paths(data).ok())
            .unwrap_or_default();
        if let Ok(mut guard) = self.paths.lock() {
            *guard = paths;
        }
        Ok(())
    }
}

impl IContextMenu_Impl for ArchiveContextMenu_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        index_menu: u32,
        id_cmd_first: u32,
        id_cmd_last: u32,
        u_flags: u32,
    ) -> windows::core::HRESULT {
        if let Ok(mut guard) = self.active_verbs.lock() {
            guard.clear();
        }
        if u_flags & CMF_DEFAULTONLY != 0 {
            return S_OK;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return S_OK;
        }
        let paths_buf: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let verbs = menu_verbs(&paths_buf);

        let mut next_id = id_cmd_first;
        let mut inserted = Vec::new();
        for (verb, label) in &verbs {
            if next_id > id_cmd_last {
                break;
            }
            if let Some(wide_label) = wide(label) {
                // Explorer tells each context-menu handler where its first item
                // belongs through index_menu.  Appending to the HMENU ignores
                // that merge position and forces our commands to the very end.
                let insert_at = index_menu.saturating_add(inserted.len() as u32);
                // SAFETY: hmenu is valid for the duration of the menu and the
                // label buffer stays alive for the call.
                if unsafe {
                    InsertMenuW(
                        hmenu,
                        insert_at,
                        MF_BYPOSITION | MF_STRING,
                        next_id as usize,
                        PCWSTR(wide_label.as_ptr()),
                    )
                }
                .is_ok()
                {
                    inserted.push(*verb);
                    next_id += 1;
                }
            }
        }
        let count = inserted.len() as u32;
        if let Ok(mut guard) = self.active_verbs.lock() {
            *guard = inserted;
        }
        // Show the app icon in front of the classic-menu verbs. SetMenuItem-
        // Bitmaps only accepts monochrome masks; passing the color bitmap
        // returned by GetIconInfo makes its transparent pixels render black.
        // Use MIIM_BITMAP with an alpha-capable DIB instead.
        if let Some(exe) = find_exe_path() {
            if let Some(bitmap) = load_menu_icon_bitmap(&exe) {
                for offset in 0..count {
                    let item = index_menu.saturating_add(offset);
                    let mut menu_item = MENUITEMINFOW {
                        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_BITMAP,
                        hbmpItem: bitmap,
                        ..Default::default()
                    };
                    // SAFETY: hmenu is valid, `item` is a position we just
                    // inserted at, and the bitmap outlives the menu.
                    let _ = unsafe { SetMenuItemInfoW(hmenu, item, true, &mut menu_item) };
                }
                if let Ok(mut guard) = self.menu_bitmap.lock() {
                    if let Some(previous) = guard.replace(bitmap) {
                        // SAFETY: a previous bitmap belongs to a menu that has
                        // already been dismissed and can be released now.
                        let _ = unsafe { DeleteObject(HGDIOBJ(previous.0)) };
                    }
                }
            }
        }
        windows::core::HRESULT(count as i32)
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> WinResult<()> {
        if pici.is_null() {
            return Err(E_POINTER.into());
        }
        // For a numeric verb, Explorer passes the zero-based command offset
        // directly in the low word of lpVerb; it is NOT the absolute menu id.
        let raw_verb = unsafe { (*pici).lpVerb.0 as usize };
        if raw_verb >> 16 != 0 {
            // A string verb belongs to some other handler: this extension only
            // advertises numeric command offsets from QueryContextMenu.  Never
            // return S_OK for an unknown verb, because doing so tells the Shell
            // that the command was handled and can swallow unrelated commands
            // (including commands invoked from shell-owned menus such as Win+X).
            return Err(E_FAIL.into());
        }
        let offset = raw_verb & 0xFFFF;
        let verb = self
            .active_verbs
            .lock()
            .ok()
            .and_then(|verbs| verbs.get(offset).copied());
        let Some(verb) = verb else {
            // The numeric offset is outside the range we inserted.  Do not
            // claim success for a command that is not ours.
            return Err(E_FAIL.into());
        };
        let paths = self.selected_paths();
        if paths.is_empty() {
            return Ok(());
        }
        let Some(exe) = find_exe_path() else {
            return Ok(());
        };
        let args = self
            .target_folder()
            .map(|destination| build_targeted_args(verb, &destination, &paths))
            .unwrap_or_else(|| build_args(verb.subcommand(), &paths));
        run_exe(&exe, &args);
        Ok(())
    }

    fn GetCommandString(
        &self,
        idcmd: usize,
        uflags: u32,
        _reserved: *const u32,
        commandstring: PSTR,
        cch: u32,
    ) -> WinResult<()> {
        if uflags != GCS_VERBW && uflags != GCS_HELPTEXTW {
            return Err(E_NOTIMPL.into());
        }
        let verb = self
            .active_verbs
            .lock()
            .ok()
            .and_then(|verbs| verbs.get(idcmd).copied())
            .ok_or(E_FAIL)?;
        let paths: Vec<PathBuf> = self.selected_paths().iter().map(PathBuf::from).collect();
        let label = menu_verbs(&paths)
            .into_iter()
            .find_map(|(candidate, label)| (candidate == verb).then_some(label))
            .ok_or(E_FAIL)?;
        let text = if uflags == GCS_VERBW {
            // GCS_VERBW is the canonical invocation string, not the visible
            // menu caption. Returning the caption here makes Explorer cache a
            // broken verb and results in incorrect help/tooltips.
            verb.subcommand().to_owned()
        } else {
            format!("ArchiveRclick: {label}")
        };
        write_wide_buffer(&text, commandstring, cch)
    }
}

/// The actions the shell extension can run on a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Extract,
    Zip,
    SevenZip,
    ZipEach,
    SevenZipEach,
}

impl Verb {
    fn subcommand(self) -> &'static str {
        match self {
            Verb::Extract => "extract",
            Verb::Zip => "zip",
            Verb::SevenZip => "7z",
            Verb::ZipEach => "zip-each",
            Verb::SevenZipEach => "7z-each",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Extract => "Extract the selected archive(s)",
            Self::Zip => "Create a ZIP archive from the selected items",
            Self::SevenZip => "Create a 7z archive from the selected items",
            Self::ZipEach => "Create one ZIP archive for each selected folder",
            Self::SevenZipEach => "Create one 7z archive for each selected folder",
        }
    }
}

/// (verb, label) pairs for the current selection, in menu order. Labels are
/// computed from the real file names so each entry stays per-file, e.g.
/// "보고서.zip으로 압축하기", "보고서.7z로 압축하기", "보고서\ 에 풀기". Long
/// file names are shortened to 30 characters for display.
fn menu_verbs(paths: &[PathBuf]) -> Vec<(Verb, String)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let all_archives = paths.iter().all(|path| is_archive_path(path));
    if all_archives {
        if paths.len() == 1 {
            // 실제 해제 폴더명 (이미 있으면 _2, _3 ...): "보고서\ 에 풀기"
            let base = archive_stem(&paths[0]);
            let parent = paths[0].parent().unwrap_or_else(|| Path::new("."));
            let final_name = unique_path(&parent.join(&base))
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(base);
            vec![(
                Verb::Extract,
                format!("{}\\ 에 풀기", shorten_menu_name(&final_name)),
            )]
        } else {
            // 여러 압축파일을 선택하면 각각 자기 이름의 폴더에 푼다.
            let mut verbs = vec![(Verb::Extract, "각각의 폴더에 풀기".to_owned())];
            // 압축파일만 여러 개 선택한 경우에도 새 압축파일로 묶을 수
            // 있도록 압축 메뉴를 함께 표시한다.
            verbs.extend(compression_verbs(paths));
            verbs
        }
    } else {
        compression_verbs(paths)
    }
}

fn compression_verbs(paths: &[PathBuf]) -> Vec<(Verb, String)> {
    if is_multi_folder_selection(paths) {
        return vec![
            (Verb::Zip, "선택한 폴더를 하나의 ZIP으로 압축하기".to_owned()),
            (Verb::SevenZip, "선택한 폴더를 하나의 7z으로 압축하기".to_owned()),
            (Verb::ZipEach, "각 폴더를 각각 ZIP으로 압축하기".to_owned()),
            (Verb::SevenZipEach, "각 폴더를 각각 7z으로 압축하기".to_owned()),
        ];
    }

    // 실제 압축 파일명 (이미 있으면 _2, _3 ...)
    let zip_name = unique_path(&cli_archive_destination(paths, CreateFormat::Zip))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.zip".to_owned());
    let seven_name = unique_path(&cli_archive_destination(paths, CreateFormat::SevenZip))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.7z".to_owned());
    vec![
        (
            Verb::Zip,
            format!("{}으로 압축하기", shorten_compression_name(&zip_name)),
        ),
        (
            Verb::SevenZip,
            format!("{}로 압축하기", shorten_compression_name(&seven_name)),
        ),
    ]
}

fn is_multi_folder_selection(paths: &[PathBuf]) -> bool {
    paths.len() > 1 && paths.iter().all(|path| path.is_dir())
}

/// Maximum number of characters shown for a file name in the context menu.
const MAX_MENU_NAME_CHARS: usize = 30;

/// Shortens an extraction destination name for the context menu.
///
/// Extraction destinations are based on an archive's stem, so a dot in the
/// original name is part of the stem rather than an extension to preserve.
fn shorten_menu_name(name: &str) -> String {
    shorten_menu_name_with_suffix(name, None)
}

/// Shortens a generated compression destination while keeping its `.zip` or
/// `.7z` extension visible.
fn shorten_compression_name(name: &str) -> String {
    shorten_menu_name_with_suffix(name, compression_suffix(name))
}

fn shorten_menu_name_with_suffix(name: &str, suffix: Option<&str>) -> String {
    if name.chars().count() <= MAX_MENU_NAME_CHARS {
        return name.to_owned();
    }
    let suffix = suffix.unwrap_or_default();
    let suffix_len = suffix.chars().count();
    if suffix_len >= MAX_MENU_NAME_CHARS {
        // Keep the complete allowed extension even when it is the whole
        // label. This branch is only reachable for an unusually long suffix.
        return suffix.to_owned();
    }
    let prefix_len = MAX_MENU_NAME_CHARS - suffix_len - 1;
    let prefix: String = name.chars().take(prefix_len).collect();
    format!("{prefix}…{suffix}")
}

fn compression_suffix(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    for extension in [".zip", ".7z"] {
        if lower.ends_with(extension) {
            return name.get(name.len().saturating_sub(extension.len())..);
        }
    }
    None
}

/// One top-level IExplorerCommand ("압축하기"/"풀기"). Explorer asks for the
/// title before it can display the item, so the selection is captured there
/// and reused by Invoke; Invoke also re-reads its own argument as a fallback.
#[implement(IExplorerCommand)]
struct ArchiveVerbCommand {
    verb: Verb,
    paths: Mutex<Vec<OsString>>,
}

impl ArchiveVerbCommand {
    fn new(verb: Verb) -> Self {
        Self {
            verb,
            paths: Mutex::new(Vec::new()),
        }
    }

    fn label(&self, paths: &[PathBuf]) -> Option<String> {
        menu_verbs(paths)
            .into_iter()
            .find_map(|(verb, label)| (verb == self.verb).then_some(label))
    }

    fn stash_paths(&self, paths: Vec<OsString>) {
        if let Ok(mut guard) = self.paths.lock() {
            *guard = paths;
        }
    }
}

impl Drop for ArchiveVerbCommand {
    fn drop(&mut self) {
        decrement_live_objects();
    }
}

impl IExplorerCommand_Impl for ArchiveVerbCommand_Impl {
    fn GetTitle(&self, psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        let paths = psiitemarray
            .as_ref()
            .map(item_array_paths)
            .unwrap_or_default();
        self.stash_paths(paths.clone());
        let paths_buf: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let Some(label) = self.label(&paths_buf) else {
            return Ok(PWSTR::null());
        };
        wide_alloc(&label).ok_or_else(|| E_OUTOFMEMORY.into())
    }

    fn GetIcon(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        let Some(exe) = find_exe_path() else {
            return Err(E_NOTIMPL.into());
        };
        // Explorer resolves "path,index" against the executable's icon
        // resources, so the Windows 11 default menu shows the app icon.
        wide_alloc(&format!("{},0", exe.to_string_lossy())).ok_or_else(|| E_OUTOFMEMORY.into())
    }

    fn GetToolTip(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        let paths: Vec<PathBuf> = self
            .paths
            .lock()
            .map(|paths| paths.iter().map(PathBuf::from).collect())
            .unwrap_or_default();
        let label = self
            .label(&paths)
            .unwrap_or_else(|| self.verb.tooltip().to_owned());
        wide_alloc(&format!("ArchiveRclick: {label}")).ok_or_else(|| E_OUTOFMEMORY.into())
    }

    fn GetCanonicalName(&self) -> WinResult<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(
        &self,
        psiitemarray: Ref<'_, IShellItemArray>,
        _foktobeslow: BOOL,
    ) -> WinResult<u32> {
        let visible = psiitemarray.as_ref().is_some_and(|array| {
            let paths_buf: Vec<PathBuf> =
                item_array_paths(array).iter().map(PathBuf::from).collect();
            !paths_buf.is_empty() && self.label(&paths_buf).is_some()
        });
        Ok(if visible {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(
        &self,
        psiitemarray: Ref<'_, IShellItemArray>,
        _pbc: Ref<'_, IBindCtx>,
    ) -> WinResult<()> {
        let paths = psiitemarray
            .as_ref()
            .map(item_array_paths)
            .unwrap_or_default();
        self.stash_paths(paths.clone());
        if paths.is_empty() {
            return Ok(());
        }
        let Some(exe) = find_exe_path() else {
            return Ok(());
        };
        run_exe(&exe, &build_args(self.verb.subcommand(), &paths));
        Ok(())
    }

    fn GetFlags(&self) -> WinResult<u32> {
        Ok(0)
    }

    fn EnumSubCommands(&self) -> WinResult<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

// ---------------------------------------------------------------------------
// Class factory
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum FactoryKind {
    Classic,
    ExplorerCommand(Verb),
}

#[implement(IClassFactory)]
struct ShellExtFactory {
    kind: FactoryKind,
}

impl ShellExtFactory {
    fn new(kind: FactoryKind) -> Self {
        Self { kind }
    }
}

impl Drop for ShellExtFactory {
    fn drop(&mut self) {
        decrement_live_objects();
    }
}

impl IClassFactory_Impl for ShellExtFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobj: *mut *mut c_void,
    ) -> WinResult<()> {
        if !punkouter.is_null() {
            return Err(E_NOINTERFACE.into());
        }
        if ppvobj.is_null() {
            return Err(E_POINTER.into());
        }
        // Count the COM object from creation until its final Release/Drop.
        // This keeps DllCanUnloadNow from unloading the DLL underneath it.
        increment_live_objects();
        match self.kind {
            FactoryKind::Classic => {
                let handler: IContextMenu = ArchiveContextMenu::new().into();
                // SAFETY: riid/ppvobj are out-parameters provided by the caller and
                // the object's vtable starts with the standard IUnknown methods.
                unsafe {
                    let object = handler.as_raw() as *mut c_void;
                    let vtbl = *(object as *const *const windows::core::IUnknown_Vtbl);
                    ((*vtbl).QueryInterface)(object, riid, ppvobj).ok()
                }
            }
            FactoryKind::ExplorerCommand(verb) => {
                let handler: IExplorerCommand = ArchiveVerbCommand::new(verb).into();
                // SAFETY: riid/ppvobj are out-parameters provided by the caller and
                // the object's vtable starts with the standard IUnknown methods.
                unsafe {
                    let object = handler.as_raw() as *mut c_void;
                    let vtbl = *(object as *const *const windows::core::IUnknown_Vtbl);
                    ((*vtbl).QueryInterface)(object, riid, ppvobj).ok()
                }
            }
        }
    }

    fn LockServer(&self, flock: BOOL) -> WinResult<()> {
        if flock.as_bool() {
            increment_live_objects();
        } else {
            decrement_live_objects();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DLL exports
// ---------------------------------------------------------------------------

/// # Safety
/// Standard COM export contract: all pointers are validated before use.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> windows::core::HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: rclsid was validated non-null above.
    let kind = match unsafe { *rclsid } {
        CLSID_SHELL_EXT => FactoryKind::Classic,
        CLSID_EXTRACT_COMMAND => FactoryKind::ExplorerCommand(Verb::Extract),
        CLSID_ZIP_COMMAND => FactoryKind::ExplorerCommand(Verb::Zip),
        CLSID_SEVENZIP_COMMAND => FactoryKind::ExplorerCommand(Verb::SevenZip),
        CLSID_ZIP_EACH_COMMAND => FactoryKind::ExplorerCommand(Verb::ZipEach),
        CLSID_SEVENZIP_EACH_COMMAND => FactoryKind::ExplorerCommand(Verb::SevenZipEach),
        _ => return CLASS_E_CLASSNOTAVAILABLE,
    };
    // The class factory itself is a live COM object too.  If it is omitted
    // from the count, DllCanUnloadNow may return S_OK while Explorer still
    // holds the factory and later call into an unloaded DLL.
    increment_live_objects();
    let factory: IClassFactory = ShellExtFactory::new(kind).into();
    // SAFETY: riid/ppv are out-parameters provided by the caller and the
    // object's vtable starts with the standard IUnknown methods.
    unsafe {
        let object = factory.as_raw() as *mut c_void;
        let vtbl = *(object as *const *const windows::core::IUnknown_Vtbl);
        ((*vtbl).QueryInterface)(object, riid, ppv)
    }
}

/// # Safety
/// COM export: takes no arguments.
///
/// Never claim "unloadable" while the host still holds objects or locks:
/// Explorer would unload this DLL underneath them and crash on the next call.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllCanUnloadNow() -> windows::core::HRESULT {
    if LIVE_OBJECTS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// # Safety
/// COM export: takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllRegisterServer() -> windows::core::HRESULT {
    let Some(dll) = module_file_name() else {
        return E_FAIL;
    };
    match register_context_menu(Path::new(&dll)) {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

/// # Safety
/// COM export: takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllUnregisterServer() -> windows::core::HRESULT {
    match unregister_context_menu() {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_archive_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    ARCHIVE_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        || split_volume_base_name(&name).is_some()
}

fn split_volume_base_name(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('.')?;
    let base_lower = base.to_ascii_lowercase();
    (base_lower.ends_with(".zip") || base_lower.ends_with(".7z"))
        .then_some(())
        .filter(|_| suffix.len() >= 3 && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|_| base)
}

fn archive_stem(path: &Path) -> String {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "압축파일".to_owned());
    if let Some(base_length) = split_volume_base_name(&name).map(str::len) {
        name.truncate(base_length);
    }
    for extension in [".tar.zst", ".tar.xz", ".tar.gz", ".tar.bz2"] {
        if name.to_ascii_lowercase().ends_with(extension) {
            name.truncate(name.len() - extension.len());
            return if name.is_empty() {
                "압축파일".to_owned()
            } else {
                name
            };
        }
    }
    Path::new(&name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "압축파일".to_owned())
}

fn build_args(subcommand: &str, paths: &[OsString]) -> String {
    // Explorer can return a root folder with a trailing backslash (for
    // example, `C:\`).  Hand-quoting paths leaves that backslash to escape the
    // closing quote in CommandLineToArgvW, which corrupts the argument and can
    // make the child process fail before it reaches its permission handling.
    // Use the same Windows quoting rules as the UAC relaunch path.
    let mut args = quote_windows_arg(std::ffi::OsStr::new(subcommand));
    for path in paths {
        args.push(' ');
        args.push_str(&quote_windows_arg(path));
    }
    args
}

/// Builds a private target-aware command used by the right-drag handler. The
/// destination is the folder that received the drop, so it must be passed
/// separately from the source paths rather than inferred from their original
/// parent folders.
fn build_targeted_args(verb: Verb, destination: &Path, paths: &[OsString]) -> String {
    let subcommand = match verb {
        Verb::Extract => "extract-to",
        Verb::Zip => "zip-to",
        Verb::SevenZip => "7z-to",
        Verb::ZipEach => "zip-each-to",
        Verb::SevenZipEach => "7z-each-to",
    };
    build_destination_args(subcommand, destination, paths)
}

fn build_destination_args(
    subcommand: &str,
    destination: &Path,
    paths: &[OsString],
) -> String {
    let mut args = quote_windows_arg(std::ffi::OsStr::new(subcommand));
    args.push(' ');
    args.push_str(&quote_windows_arg(destination.as_os_str()));
    for path in paths {
        args.push(' ');
        args.push_str(&quote_windows_arg(path));
    }
    args
}

fn run_exe(exe: &OsString, args: &str) {
    let Some(exe_wide) = wide(&exe.to_string_lossy()) else {
        return;
    };
    let Some(args_wide) = wide(args) else {
        return;
    };
    // SAFETY: both strings stay alive for the call and contain a terminator.
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe_wide.as_ptr()),
            PCWSTR(args_wide.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Reads HDROP paths out of an IDataObject supplied by Explorer.
fn read_hdrop_paths(data_obj: &IDataObject) -> WinResult<Vec<OsString>> {
    // CF_HDROP = 15, DVASPECT_CONTENT = 1, TYMED_HGLOBAL = 1 (windows 0.61
    // exposes these FORMATETC fields as plain integers).
    let format = FORMATETC {
        cfFormat: 15,
        ptd: ptr::null_mut(),
        dwAspect: 1,
        lindex: -1,
        tymed: 1,
    };
    // SAFETY: `format` is valid for the call; the returned medium is
    // released below.
    let mut medium = unsafe { data_obj.GetData(&format) }?;
    let result = read_hdrop(&medium);
    // SAFETY: `medium` was filled by GetData and must be released.
    unsafe { ReleaseStgMedium(&mut medium) };
    result
}

fn read_hdrop(medium: &STGMEDIUM) -> WinResult<Vec<OsString>> {
    let hdrop = HDROP(unsafe { medium.u.hGlobal.0 });
    // SAFETY: hdrop is a valid HGLOBAL from the shell's IDataObject.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: index is within the drop count reported by the shell.
        let length = unsafe { DragQueryFileW(hdrop, index, None) };
        let mut buffer = vec![0u16; (length + 1) as usize];
        // SAFETY: buffer is sized for the returned length plus terminator.
        unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
        buffer.truncate(length as usize);
        paths.push(OsString::from_wide(&buffer));
    }
    Ok(paths)
}

fn find_exe_path() -> Option<OsString> {
    if let Some(configured) = read_registry_string(HKEY_CURRENT_USER, SETTINGS_KEY, EXE_PATH_VALUE)
    {
        if Path::new(&configured).is_file() {
            return Some(configured);
        }
    }
    let module = module_file_name()?;
    let directory = Path::new(&module).parent()?;
    let candidate = directory.join(EXE_NAME);
    candidate.is_file().then(|| candidate.into_os_string())
}

/// Loads the small app icon from the executable into a transparent 32bpp DIB
/// for the classic Explorer menu. The caller owns the returned bitmap.
fn load_menu_icon_bitmap(exe: &OsString) -> Option<HBITMAP> {
    let exe_wide = wide(&exe.to_string_lossy())?;
    // LoadImageW with LR_LOADFROMFILE cannot extract icons from .exe files
    // (it only reads .ico/.cur/.ani files), so use ExtractIconExW, the API
    // the shell itself uses to resolve file icons.
    let mut small = HICON(ptr::null_mut());
    // SAFETY: exe_wide is NUL-terminated and phiconsmall is an out-parameter.
    let count = unsafe { ExtractIconExW(PCWSTR(exe_wide.as_ptr()), 0, None, Some(&mut small), 1) };
    if count == 0 || small.0.is_null() {
        return None;
    }
    let width = unsafe { GetSystemMetrics(SM_CXMENUCHECK) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYMENUCHECK) }.max(1);
    let bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // A top-down DIB keeps DrawIconEx's origin intuitive and avoids
            // having to reverse the rows before Explorer consumes the bitmap.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels: *mut c_void = ptr::null_mut();
    let bitmap =
        unsafe { CreateDIBSection(None, &bitmap_info, DIB_RGB_COLORS, &mut pixels, None, 0) }
            .ok()?;
    if pixels.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        let _ = unsafe { DestroyIcon(small) };
        return None;
    }
    // A DIB section is not required to be zero-initialized. Clear it before
    // drawing so pixels outside the icon remain fully transparent.
    let pixel_count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    unsafe { ptr::write_bytes(pixels.cast::<u8>(), 0, pixel_count) };

    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        let _ = unsafe { DestroyIcon(small) };
        return None;
    }
    let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    let draw_result = unsafe { DrawIconEx(dc, 0, 0, small, width, height, 0, None, DI_NORMAL) };
    if !previous.0.is_null() {
        let _ = unsafe { SelectObject(dc, previous) };
    }
    let _ = unsafe { DeleteDC(dc) };
    let _ = unsafe { DestroyIcon(small) };
    if draw_result.is_err() {
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        None
    } else {
        Some(bitmap)
    }
}

fn module_file_name() -> Option<OsString> {
    // GetModuleHandleW(None) returns the host executable (explorer.exe or
    // regsvr32.exe), not this in-process shell-extension DLL. Resolve the
    // module that contains one of our exported functions instead.
    let mut module = HMODULE::default();
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(DllGetClassObject as *const () as *const u16),
            &mut module,
        )
    }
    .ok()?;

    // Grow the buffer if a long path does not fit.
    let mut capacity = 512usize;
    loop {
        let mut buffer = vec![0u16; capacity];
        // SAFETY: buffer is writable and module is the handle returned above.
        let length = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
        if length == 0 {
            return None;
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Some(OsString::from_wide(&buffer));
        }
        capacity = capacity.checked_mul(2)?;
        if capacity > 32768 {
            return None;
        }
    }
}

fn read_registry_string(hive: HKEY, key_path: &str, value: &str) -> Option<OsString> {
    let key_name = HSTRING::from(key_path);
    let value_name = HSTRING::from(value);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status = unsafe { RegOpenKeyExW(hive, &key_name, None, KEY_READ, &mut raw) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let mut size = 0u32;
    // SAFETY: size is an out-parameter for the first probe.
    let status = unsafe {
        RegQueryValueExW(
            raw,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        // SAFETY: handle came from RegOpenKeyExW.
        let _ = unsafe { RegCloseKey(raw) };
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: data has the exact size the API requested.
    let status = unsafe {
        RegQueryValueExW(
            raw,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    // SAFETY: handle came from RegOpenKeyExW.
    let _ = unsafe { RegCloseKey(raw) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units: Vec<u16> = data[..size as usize]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    let text = text.trim_end_matches('\0');
    (!text.is_empty()).then(|| OsString::from(text))
}

/// Writes the per-user registry entries that attach this shell extension to
/// Explorer's right-click menu. `dll_path` is the registered InprocServer32
/// module (archive_rclick_core.dll next to the app executable).
pub fn register_context_menu(dll_path: &Path) -> Result<(), String> {
    if !dll_path.is_file() {
        return Err(format!(
            "The shell extension DLL does not exist: {}",
            dll_path.display()
        ));
    }
    let directory = dll_path
        .parent()
        .ok_or_else(|| "could not resolve the shell extension directory".to_owned())?;
    let exe = directory.join(EXE_NAME);
    // Register only the dedicated IExplorerCommand entries below. Older
    // versions registered the same operation through IContextMenu as well,
    // which makes Windows 11 show a second, duplicate Extract item.
    for key in [
        LEGACY_CONTEXT_MENU_KEY,
        LEGACY_DIRECTORY_CONTEXT_MENU_KEY,
        LEGACY_MODERN_MENU_KEY,
        LEGACY_MODERN_DIRECTORY_MENU_KEY,
        LEGACY_BACKGROUND_DRAG_DROP_HANDLER_KEY,
    ] {
        delete_registry_tree(HKEY_CURRENT_USER, key)?;
    }
    // The classic handler is still needed for Explorer's right-drag menu.
    // Unlike the normal file/folder menu, that menu is populated through the
    // DragDropHandlers registration and passes the dragged files as CF_HDROP.
    let shell_ext_clsid = guid_string_for(CLSID_SHELL_EXT);
    let shell_ext_clsid_key = format!(r"Software\Classes\CLSID\{shell_ext_clsid}");
    let shell_ext_inproc = format!(r"{shell_ext_clsid_key}\InprocServer32");
    set_registry_string(
        HKEY_CURRENT_USER,
        &shell_ext_clsid_key,
        None,
        "ArchiveRclick drag-and-drop command",
    )?;
    set_registry_string(
        HKEY_CURRENT_USER,
        &shell_ext_inproc,
        None,
        &dll_path.to_string_lossy(),
    )?;
    set_registry_string(
        HKEY_CURRENT_USER,
        &shell_ext_inproc,
        Some("ThreadingModel"),
        "Apartment",
    )?;
    set_registry_string(
        HKEY_CURRENT_USER,
        DRAG_DROP_HANDLER_KEY,
        None,
        &shell_ext_clsid,
    )?;
    for (command_clsid, description) in [
        (CLSID_EXTRACT_COMMAND, "ArchiveRclick extract command"),
        (CLSID_ZIP_COMMAND, "ArchiveRclick ZIP command"),
        (CLSID_SEVENZIP_COMMAND, "ArchiveRclick 7z command"),
        (CLSID_ZIP_EACH_COMMAND, "ArchiveRclick per-folder ZIP command"),
        (
            CLSID_SEVENZIP_EACH_COMMAND,
            "ArchiveRclick per-folder 7z command",
        ),
    ] {
        let command_guid = guid_string_for(command_clsid);
        let command_clsid_key = format!(r"Software\Classes\CLSID\{command_guid}");
        let command_inproc = format!(r"{command_clsid_key}\InprocServer32");
        set_registry_string(HKEY_CURRENT_USER, &command_clsid_key, None, description)?;
        set_registry_string(
            HKEY_CURRENT_USER,
            &command_inproc,
            None,
            &dll_path.to_string_lossy(),
        )?;
        set_registry_string(
            HKEY_CURRENT_USER,
            &command_inproc,
            Some("ThreadingModel"),
            "Apartment",
        )?;
    }
    let icon = format!("{},0", exe.to_string_lossy());
    for (key, command_clsid) in [
        (MODERN_EXTRACT_MENU_KEY, CLSID_EXTRACT_COMMAND),
        (MODERN_ZIP_MENU_KEY, CLSID_ZIP_COMMAND),
        (MODERN_SEVENZIP_MENU_KEY, CLSID_SEVENZIP_COMMAND),
        (MODERN_ZIP_EACH_MENU_KEY, CLSID_ZIP_EACH_COMMAND),
        (
            MODERN_SEVENZIP_EACH_MENU_KEY,
            CLSID_SEVENZIP_EACH_COMMAND,
        ),
    ] {
        let command_guid = guid_string_for(command_clsid);
        set_registry_string(
            HKEY_CURRENT_USER,
            key,
            Some(EXPLORER_COMMAND_HANDLER_VALUE),
            &command_guid,
        )?;
        set_registry_string(HKEY_CURRENT_USER, key, Some("CommandStateSync"), "")?;
        set_registry_string(HKEY_CURRENT_USER, key, Some("Icon"), &icon)?;
    }
    for (key, command_clsid) in [
        (MODERN_EXTRACT_DIRECTORY_MENU_KEY, CLSID_EXTRACT_COMMAND),
        (MODERN_ZIP_DIRECTORY_MENU_KEY, CLSID_ZIP_COMMAND),
        (MODERN_SEVENZIP_DIRECTORY_MENU_KEY, CLSID_SEVENZIP_COMMAND),
        (
            MODERN_ZIP_EACH_DIRECTORY_MENU_KEY,
            CLSID_ZIP_EACH_COMMAND,
        ),
        (
            MODERN_SEVENZIP_EACH_DIRECTORY_MENU_KEY,
            CLSID_SEVENZIP_EACH_COMMAND,
        ),
    ] {
        let command_guid = guid_string_for(command_clsid);
        set_registry_string(
            HKEY_CURRENT_USER,
            key,
            Some(EXPLORER_COMMAND_HANDLER_VALUE),
            &command_guid,
        )?;
        set_registry_string(HKEY_CURRENT_USER, key, Some("CommandStateSync"), "")?;
        set_registry_string(HKEY_CURRENT_USER, key, Some("Icon"), &icon)?;
    }
    set_registry_string(
        HKEY_CURRENT_USER,
        SETTINGS_KEY,
        Some(EXE_PATH_VALUE),
        &exe.to_string_lossy(),
    )?;
    notify_shell_change();
    Ok(())
}

/// Removes the registry entries written by [`register_context_menu`]. The
/// `Software\ArchiveRclick` key itself is kept because it also holds the app's
/// settings; only the recorded executable path is deleted.
pub fn unregister_context_menu() -> Result<(), String> {
    for clsid in [
        CLSID_SHELL_EXT,
        CLSID_EXTRACT_COMMAND,
        CLSID_ZIP_COMMAND,
        CLSID_SEVENZIP_COMMAND,
        CLSID_ZIP_EACH_COMMAND,
        CLSID_SEVENZIP_EACH_COMMAND,
    ] {
        delete_registry_tree(
            HKEY_CURRENT_USER,
            &format!(r"Software\Classes\CLSID\{}", guid_string_for(clsid)),
        )?;
    }
    for key in [
        LEGACY_MODERN_MENU_KEY,
        LEGACY_MODERN_DIRECTORY_MENU_KEY,
        MODERN_EXTRACT_MENU_KEY,
        MODERN_EXTRACT_DIRECTORY_MENU_KEY,
        MODERN_ZIP_MENU_KEY,
        MODERN_ZIP_DIRECTORY_MENU_KEY,
        MODERN_SEVENZIP_MENU_KEY,
        MODERN_SEVENZIP_DIRECTORY_MENU_KEY,
        MODERN_ZIP_EACH_MENU_KEY,
        MODERN_ZIP_EACH_DIRECTORY_MENU_KEY,
        MODERN_SEVENZIP_EACH_MENU_KEY,
        MODERN_SEVENZIP_EACH_DIRECTORY_MENU_KEY,
        LEGACY_CONTEXT_MENU_KEY,
        LEGACY_DIRECTORY_CONTEXT_MENU_KEY,
        LEGACY_BACKGROUND_DRAG_DROP_HANDLER_KEY,
        DRAG_DROP_HANDLER_KEY,
    ] {
        delete_registry_tree(HKEY_CURRENT_USER, key)?;
    }
    delete_registry_value(HKEY_CURRENT_USER, SETTINGS_KEY, EXE_PATH_VALUE)?;
    notify_shell_change();
    Ok(())
}

/// Tells Explorer to pick up the changed context-menu registration without
/// waiting for a shell restart.
fn notify_shell_change() {
    // SAFETY: SHCNE_ASSOCCHANGED with SHCNF_IDLIST requires no item pointers.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Whether the context-menu handler is currently registered for this user.
pub fn is_context_menu_registered() -> bool {
    let drag_drop_registered = registry_key_exists(
        HKEY_CURRENT_USER,
        &format!(
            r"Software\Classes\CLSID\{}\InprocServer32",
            guid_string_for(CLSID_SHELL_EXT)
        ),
    ) && registry_key_exists(HKEY_CURRENT_USER, DRAG_DROP_HANDLER_KEY);
    let classes_registered = [
        CLSID_EXTRACT_COMMAND,
        CLSID_ZIP_COMMAND,
        CLSID_SEVENZIP_COMMAND,
        CLSID_ZIP_EACH_COMMAND,
        CLSID_SEVENZIP_EACH_COMMAND,
    ]
    .into_iter()
    .all(|clsid| {
        registry_key_exists(
            HKEY_CURRENT_USER,
            &format!(
                r"Software\Classes\CLSID\{}\InprocServer32",
                guid_string_for(clsid)
            ),
        )
    });
    let modern_entries_present = [
        MODERN_EXTRACT_MENU_KEY,
        MODERN_EXTRACT_DIRECTORY_MENU_KEY,
        MODERN_ZIP_MENU_KEY,
        MODERN_ZIP_DIRECTORY_MENU_KEY,
        MODERN_SEVENZIP_MENU_KEY,
        MODERN_SEVENZIP_DIRECTORY_MENU_KEY,
        MODERN_ZIP_EACH_MENU_KEY,
        MODERN_ZIP_EACH_DIRECTORY_MENU_KEY,
        MODERN_SEVENZIP_EACH_MENU_KEY,
        MODERN_SEVENZIP_EACH_DIRECTORY_MENU_KEY,
    ]
    .into_iter()
    .all(|key| registry_key_exists(HKEY_CURRENT_USER, key));
    let old_cascade_present = registry_key_exists(HKEY_CURRENT_USER, LEGACY_MODERN_MENU_KEY)
        || registry_key_exists(HKEY_CURRENT_USER, LEGACY_MODERN_DIRECTORY_MENU_KEY);
    let old_legacy_handler_present =
        registry_key_exists(HKEY_CURRENT_USER, LEGACY_CONTEXT_MENU_KEY)
            || registry_key_exists(HKEY_CURRENT_USER, LEGACY_DIRECTORY_CONTEXT_MENU_KEY);

    drag_drop_registered
        && classes_registered
        && modern_entries_present
        && !old_cascade_present
        && !old_legacy_handler_present
}

fn registry_key_exists(hive: HKEY, key_path: &str) -> bool {
    let key_name = HSTRING::from(key_path);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status = unsafe { RegOpenKeyExW(hive, &key_name, None, KEY_READ, &mut raw) };
    if status != ERROR_SUCCESS {
        return false;
    }
    // SAFETY: handle came from RegOpenKeyExW.
    let _ = unsafe { RegCloseKey(raw) };
    true
}

fn set_registry_string(
    hive: HKEY,
    key_path: &str,
    value_name: Option<&str>,
    value: &str,
) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status = unsafe { RegCreateKeyW(hive, &key_name, &mut raw) };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "could not open registry key {key_path} (error {status:?})"
        ));
    }
    let name = value_name.map(HSTRING::from);
    let name_ptr = name
        .as_ref()
        .map(|name| PCWSTR(name.as_ptr()))
        .unwrap_or_else(PCWSTR::null);
    let data = utf16_bytes(value);
    // SAFETY: key is live and data is valid UTF-16 including its terminator.
    let status = unsafe { RegSetValueExW(raw, name_ptr, None, REG_SZ, Some(&data)) };
    // SAFETY: handle came from RegCreateKeyW.
    let _ = unsafe { RegCloseKey(raw) };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "could not write registry value {key_path} (error {status:?})"
        ));
    }
    Ok(())
}

fn delete_registry_tree(hive: HKEY, key_path: &str) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    // SAFETY: key name stays live for the call.
    let status = unsafe { RegDeleteTreeW(hive, &key_name) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "could not delete registry key {key_path} (error {status:?})"
        ))
    }
}

fn delete_registry_value(hive: HKEY, key_path: &str, value_name: &str) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status =
        unsafe { RegOpenKeyExW(hive, &key_name, None, KEY_READ | KEY_SET_VALUE, &mut raw) };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "could not open registry key {key_path} (error {status:?})"
        ));
    }
    let value = HSTRING::from(value_name);
    // SAFETY: key is live and the value name stays live for the call.
    let status = unsafe { RegDeleteValueW(raw, PCWSTR(value.as_ptr())) };
    // SAFETY: handle came from RegOpenKeyExW.
    let _ = unsafe { RegCloseKey(raw) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "could not delete registry value {key_path}\\{value_name} (error {status:?})"
        ))
    }
}

fn guid_string_for(guid: GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    )
}

fn wide(text: &str) -> Option<Vec<u16>> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    (!units.is_empty()).then_some(units)
}

/// Reads filesystem paths out of the IShellItemArray Explorer passes to the
/// IExplorerCommand methods.
fn item_array_paths(array: &IShellItemArray) -> Vec<OsString> {
    // SAFETY: array is live for the duration of the call.
    let Ok(items) = (unsafe { array.EnumItems() }) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    loop {
        let mut batch: [Option<IShellItem>; 8] = std::array::from_fn(|_| None);
        let mut fetched = 0u32;
        // SAFETY: batch is writable storage for up to 8 interface pointers and
        // fetched is an out-parameter.
        let _ = unsafe { items.Next(&mut batch, Some(&mut fetched)) };
        if fetched == 0 {
            break;
        }
        for item in batch.into_iter().take(fetched as usize).flatten() {
            if let Some(path) = item_filesystem_path(&item) {
                paths.push(path);
            }
        }
        if fetched < 8 {
            break;
        }
    }
    paths
}

/// Resolves the target folder PIDL supplied to a drag-and-drop handler.
fn item_id_list_filesystem_path(pidl: *const ITEMIDLIST) -> Option<OsString> {
    if pidl.is_null() {
        return None;
    }
    // SAFETY: Explorer owns and keeps `pidl` valid for the Initialize call.
    let item: IShellItem = unsafe { SHCreateItemFromIDList(pidl).ok()? };
    item_filesystem_path(&item)
}

/// Resolves a shell item to its filesystem path (SIGDN_FILESYSPATH).
fn item_filesystem_path(item: &IShellItem) -> Option<OsString> {
    // SAFETY: item is live; the returned name must be freed with CoTaskMemFree.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    if name.is_null() {
        return None;
    }
    // SAFETY: the shell returned a NUL-terminated UTF-16 string.
    let units = unsafe { name.as_wide() };
    let path = (!units.is_empty()).then(|| OsString::from_wide(units));
    // SAFETY: the string was allocated by the shell and must be released with
    // CoTaskMemFree.
    unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };
    path
}

/// Copies `text` into a CoTaskMem-allocated NUL-terminated UTF-16 buffer, the
/// allocation the shell expects for IExplorerCommand string out-parameters.
fn wide_alloc(text: &str) -> Option<PWSTR> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let byte_count = (units.len() + 1) * std::mem::size_of::<u16>();
    // SAFETY: byte_count is the exact allocation size requested.
    let raw = unsafe { CoTaskMemAlloc(byte_count) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: raw points to byte_count writable bytes; the copy writes exactly
    // the encoded units plus the terminating NUL.
    unsafe {
        raw.copy_from_nonoverlapping(units.as_ptr() as *const c_void, units.len() * 2);
        raw.add(byte_count - 2).write_bytes(0, 2);
    }
    Some(PWSTR(raw as *mut u16))
}

fn write_wide_buffer(text: &str, buffer: PSTR, cch: u32) -> WinResult<()> {
    if buffer.is_null() || cch == 0 {
        return Err(E_POINTER.into());
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let capacity = cch as usize;
    let count = units.len().min(capacity.saturating_sub(1));
    // SAFETY: the shell supplied a writable buffer of cch characters; count
    // leaves room for the terminating NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), buffer.0.cast::<u16>(), count);
        *buffer.0.cast::<u16>().add(count) = 0;
    }
    Ok(())
}

fn utf16_bytes(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use super::{
        Verb, build_args, build_targeted_args, menu_verbs, shorten_compression_name,
        shorten_menu_name,
    };

    #[test]
    fn command_line_quoting_keeps_a_root_folder_argument_intact() {
        assert_eq!(
            build_args("zip", &[OsString::from(r"C:\")]),
            r#""zip" "C:\\""#
        );
    }

    #[test]
    fn right_drag_extract_arguments_use_the_drop_destination() {
        let args = build_targeted_args(
            Verb::Extract,
            Path::new(r"C:\Drop Folder"),
            &[OsString::from(r"C:\Source Folder\sample.zip")],
        );
        assert_eq!(
            args,
            r#""extract-to" "C:\Drop Folder" "C:\Source Folder\sample.zip""#
        );
    }

    #[test]
    fn right_drag_compress_arguments_use_the_drop_destination() {
        let args = build_targeted_args(
            Verb::Zip,
            Path::new(r"C:\Drop Folder"),
            &[OsString::from(r"C:\Source Folder\sample")],
        );
        assert_eq!(
            args,
            r#""zip-to" "C:\Drop Folder" "C:\Source Folder\sample""#
        );
    }

    #[test]
    fn short_names_are_kept_whole() {
        assert_eq!(shorten_menu_name("보고서.zip"), "보고서.zip");
        assert_eq!(shorten_menu_name("a".repeat(30).as_str()), "a".repeat(30));
    }

    #[test]
    fn long_names_are_cut_to_30_characters() {
        let long = "이것은매우긴파일이름입니다정말정말정말정말정말긴파일이름입니다";
        let shortened = shorten_menu_name(long);
        assert_eq!(shortened.chars().count(), 30);
        let expected: String = long.chars().take(29).collect::<String>() + "…";
        assert_eq!(shortened, expected);
    }

    #[test]
    fn extraction_names_with_dots_are_cut_without_preserving_the_suffix() {
        let long = "very-long-file-name-that-goes-on-and-on-and-on.zip";
        let shortened = shorten_menu_name(long);
        assert_eq!(shortened.chars().count(), 30);
        let expected: String = long.chars().take(29).collect::<String>() + "…";
        assert_eq!(shortened, expected);
    }

    #[test]
    fn long_7z_names_keep_the_extension_and_original_case() {
        let long = "very-long-file-name-that-goes-on-and-on-and-on.7Z";
        let shortened = shorten_compression_name(long);
        assert_eq!(shortened.chars().count(), 30);
        assert!(shortened.ends_with(".7Z"));
        let expected: String = long.chars().take(26).collect::<String>() + "…" + ".7Z";
        assert_eq!(shortened, expected);
    }

    #[test]
    fn long_zip_names_keep_the_extension_visible() {
        let long = "archive-with-a-name-that-is-longer-than-the-menu-limit.zip";
        let shortened = shorten_compression_name(long);
        assert_eq!(shortened.chars().count(), 30);
        assert!(shortened.ends_with(".zip"));
        let expected: String = long.chars().take(25).collect::<String>() + "…" + ".zip";
        assert_eq!(shortened, expected);
    }

    #[test]
    fn other_suffixes_are_not_preserved_for_compression_labels() {
        let extension = format!(".{}", "x".repeat(29));
        let long = format!("archive{extension}");
        let shortened = shorten_compression_name(&long);
        let expected: String = long.chars().take(29).collect::<String>() + "…";
        assert_eq!(shortened, expected);
    }

    #[test]
    fn multiple_archives_show_extract_and_compress_actions() {
        let root =
            std::env::temp_dir().join(format!("archive-rclick-shell-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temporary shell-extension test folder");
        let first = root.join("one.zip");
        let second = root.join("two.7z");
        std::fs::File::create(&first).expect("create first archive placeholder");
        std::fs::File::create(&second).expect("create second archive placeholder");

        let single = menu_verbs(std::slice::from_ref(&first));
        assert_eq!(
            single.iter().map(|(verb, _)| *verb).collect::<Vec<_>>(),
            vec![Verb::Extract]
        );

        let split = root.join("split.7z.001");
        std::fs::File::create(&split).expect("create split archive volume placeholder");
        let split_verbs = menu_verbs(std::slice::from_ref(&split));
        assert_eq!(
            split_verbs.iter().map(|(verb, _)| *verb).collect::<Vec<_>>(),
            vec![Verb::Extract]
        );

        let iso = root.join("installer.iso");
        std::fs::File::create(&iso).expect("create ISO placeholder");
        let iso_verbs = menu_verbs(std::slice::from_ref(&iso));
        assert_eq!(
            iso_verbs.iter().map(|(verb, _)| *verb).collect::<Vec<_>>(),
            vec![Verb::Extract]
        );

        let multiple = menu_verbs(&[first, second]);
        assert_eq!(
            multiple.iter().map(|(verb, _)| *verb).collect::<Vec<_>>(),
            vec![Verb::Extract, Verb::Zip, Verb::SevenZip]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn multiple_folders_use_per_folder_compression_commands() {
        let root =
            std::env::temp_dir().join(format!("archive-rclick-shell-ext-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = root.join("one");
        let second = root.join("two");
        std::fs::create_dir_all(&first).expect("create first folder");
        std::fs::create_dir_all(&second).expect("create second folder");

        let verbs = menu_verbs(&[first.clone(), second.clone()]);
        assert_eq!(
            verbs.iter().map(|(verb, _)| *verb).collect::<Vec<_>>(),
            vec![Verb::Zip, Verb::SevenZip, Verb::ZipEach, Verb::SevenZipEach]
        );
        assert_eq!(verbs[0].1, "선택한 폴더를 하나의 ZIP으로 압축하기");
        assert_eq!(verbs[1].1, "선택한 폴더를 하나의 7z으로 압축하기");
        assert_eq!(verbs[2].1, "각 폴더를 각각 ZIP으로 압축하기");
        assert_eq!(verbs[3].1, "각 폴더를 각각 7z으로 압축하기");
        assert_eq!(Verb::Zip.subcommand(), "zip");
        assert_eq!(Verb::SevenZip.subcommand(), "7z");
        assert_eq!(Verb::ZipEach.subcommand(), "zip-each");
        assert_eq!(Verb::SevenZipEach.subcommand(), "7z-each");

        let _ = std::fs::remove_dir_all(root);
    }
}
