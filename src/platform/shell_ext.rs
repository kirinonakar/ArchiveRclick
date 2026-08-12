//! Explorer context-menu shell extension (IContextMenu).
//!
//! Static registry verbs cannot show per-file labels, so this DLL computes
//! the menu items from the selected paths and displays the real names, e.g.
//! "보고서.zip으로 압축하기", "보고서.7z로 압축하기" and "보고서\ 에 풀기".
//! Invoking an item launches archive-rclick.exe directly with Unicode
//! arguments (no PowerShell, no cmd), and the app shows its progress window.
#![cfg(windows)]

use std::{
    ffi::{c_void, OsString},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
};

use windows::{
    core::{implement, w, BOOL, IUnknown, Interface, Ref, GUID, HSTRING, PCWSTR, PSTR},
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::{
            Com::{FORMATETC, IClassFactory, IClassFactory_Impl, IDataObject, STGMEDIUM},
            LibraryLoader::{GetModuleFileNameW, GetModuleHandleW},
            Ole::ReleaseStgMedium,
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegCreateKeyW,
                RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
        },
        UI::{
            Shell::{
                CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, DragQueryFileW, HDROP, IContextMenu,
                IContextMenu_Impl, IShellExtInit, IShellExtInit_Impl, ShellExecuteW,
            },
            WindowsAndMessaging::{AppendMenuW, HMENU, MF_STRING, SW_SHOWNORMAL},
        },
    },
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;

use windows::core::Result as WinResult;

use crate::{
    app::commands::{cli_archive_destination, unique_path},
    archive::CreateFormat,
};

const CLSID_SHELL_EXT: GUID = GUID {
    data1: 0x6B2B1C4A,
    data2: 0x9D3E,
    data3: 0x4F5A,
    data4: [0x8C, 0x7B, 0x1E, 0x2F, 0x3A, 0x4B, 0x5C, 0x6D],
};

const ARCHIVE_EXTENSIONS: &[&str] = &[
    ".zip", ".zipx", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz", ".zst", ".cab", ".lha", ".lzh",
    ".tgz", ".tbz2", ".txz",
];
const EXE_NAME: &str = "archive-rclick.exe";
const SETTINGS_KEY: &str = r"Software\ArchiveRclick";
const EXE_PATH_VALUE: &str = "ExePath";

// Menu item offsets (relative to id_cmd_first).
const OFF_EXTRACT: usize = 0;
const OFF_ZIP: usize = 1;
const OFF_7Z: usize = 2;

// HRESULT constants kept local to avoid feature-flag surprises.
const S_OK: windows::core::HRESULT = windows::core::HRESULT(0);
const E_POINTER: windows::core::HRESULT = windows::core::HRESULT(0x8000_4003u32 as i32);
const E_NOINTERFACE: windows::core::HRESULT = windows::core::HRESULT(0x8000_4002u32 as i32);
const E_NOTIMPL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4001u32 as i32);
const E_FAIL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005u32 as i32);
const CLASS_E_CLASSNOTAVAILABLE: windows::core::HRESULT = windows::core::HRESULT(0x8004_0111u32 as i32);

#[implement(IShellExtInit, IContextMenu)]
struct ArchiveContextMenu {
    paths: Mutex<Vec<OsString>>,
    cmd_first: Mutex<u32>,
}

impl ArchiveContextMenu {
    fn new() -> Self {
        Self {
            paths: Mutex::new(Vec::new()),
            cmd_first: Mutex::new(0),
        }
    }

    fn selected_paths(&self) -> Vec<OsString> {
        self.paths.lock().map(|paths| paths.clone()).unwrap_or_default()
    }
}

impl IShellExtInit_Impl for ArchiveContextMenu_Impl {
    fn Initialize(
        &self,
        _pidl_folder: *const ITEMIDLIST,
        data_obj: Ref<'_, IDataObject>,
        _hkey_prog_id: HKEY,
    ) -> WinResult<()> {
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
        _index_menu: u32,
        id_cmd_first: u32,
        id_cmd_last: u32,
        u_flags: u32,
    ) -> windows::core::HRESULT {
        if u_flags & CMF_DEFAULTONLY != 0 {
            return S_OK;
        }
        if let Ok(mut guard) = self.cmd_first.lock() {
            *guard = id_cmd_first;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return S_OK;
        }
        let all_archives = paths.iter().all(|path| is_archive_path(Path::new(path)));

        let mut next_id = id_cmd_first;
        let mut count = 0u32;
        let paths_buf: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let labels: Vec<String> = if all_archives {
            // 실제 해제 폴더명 (이미 있으면 _2, _3 ...): "보고서\ 에 풀기"
            let base = extract_destination_name(&paths);
            let final_name = if paths_buf.len() == 1 {
                let parent = paths_buf[0].parent().unwrap_or_else(|| Path::new("."));
                unique_path(&parent.join(&base))
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or(base)
            } else {
                base
            };
            vec![format!("{final_name}\\ 에 풀기")]
        } else {
            // 실제 압축 파일명 (이미 있으면 _2, _3 ...)
            let zip_name = unique_path(&cli_archive_destination(&paths_buf, CreateFormat::Zip))
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive.zip".to_owned());
            let seven_name =
                unique_path(&cli_archive_destination(&paths_buf, CreateFormat::SevenZip))
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "archive.7z".to_owned());
            vec![
                format!("{zip_name}으로 압축하기"),
                format!("{seven_name}로 압축하기"),
            ]
        };
        for label in labels {
            if next_id > id_cmd_last {
                break;
            }
            if let Some(wide_label) = wide(&label) {
                // SAFETY: hmenu is valid for the duration of the menu and the
                // label buffer stays alive for the call.
                let _ = unsafe { AppendMenuW(hmenu, MF_STRING, next_id as usize, PCWSTR(wide_label.as_ptr())) };
                next_id += 1;
                count += 1;
            }
        }
        windows::core::HRESULT(count as i32)
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> WinResult<()> {
        if pici.is_null() {
            return Err(E_POINTER.into());
        }
        // Numeric verb ids are passed in the low word of lpVerb.
        let verb = unsafe { (*pici).lpVerb.0 as usize };
        if verb >> 16 != 0 {
            return Ok(());
        }
        let first = self.cmd_first.lock().map(|guard| *guard).unwrap_or(0);
        let offset = verb.wrapping_sub(first as usize);
        let paths = self.selected_paths();
        if paths.is_empty() {
            return Ok(());
        }
        let Some(exe) = find_exe_path() else {
            return Ok(());
        };
        match offset {
            OFF_EXTRACT => {
                for path in &paths {
                    let args = format!("extract \"{}\"", path.to_string_lossy());
                    run_exe(&exe, &args);
                }
            }
            OFF_ZIP => run_exe(&exe, &build_args("zip", &paths)),
            OFF_7Z => run_exe(&exe, &build_args("7z", &paths)),
            _ => {}
        }
        Ok(())
    }

    fn GetCommandString(
        &self,
        _idcmd: usize,
        _uflags: u32,
        _reserved: *const u32,
        _commandstring: PSTR,
        _cch: u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
}

// ---------------------------------------------------------------------------
// Class factory
// ---------------------------------------------------------------------------

#[implement(IClassFactory)]
struct ShellExtFactory;

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
        let handler: IContextMenu = ArchiveContextMenu::new().into();
        // SAFETY: riid/ppvobj are out-parameters provided by the caller and
        // the object's vtable starts with the standard IUnknown methods.
        unsafe {
            let object = handler.as_raw() as *mut c_void;
            let vtbl = *(object as *const *const windows::core::IUnknown_Vtbl);
            ((*vtbl).QueryInterface)(object, riid, ppvobj).ok()
        }
    }

    fn LockServer(&self, _flock: BOOL) -> WinResult<()> {
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
    if unsafe { *rclsid } != CLSID_SHELL_EXT {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = ShellExtFactory.into();
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
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllCanUnloadNow() -> windows::core::HRESULT {
    S_OK
}

/// # Safety
/// COM export: takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllRegisterServer() -> windows::core::HRESULT {
    match register_server() {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

/// # Safety
/// COM export: takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllUnregisterServer() -> windows::core::HRESULT {
    match unregister_server() {
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
}


/// Folder that will receive the extraction: the archive's stem, or the common
/// parent folder name for several archives.
fn extract_destination_name(paths: &[OsString]) -> String {
    if paths.len() == 1 {
        archive_stem(Path::new(&paths[0]))
    } else {
        common_parent_name(paths)
    }
}

fn archive_stem(path: &Path) -> String {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "압축파일".to_owned());
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

fn common_parent_name(paths: &[OsString]) -> String {
    let mut common = Path::new(&paths[0])
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    for path in &paths[1..] {
        let mut ancestor = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
        while !common.starts_with(ancestor) {
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => return "선택항목".to_owned(),
            }
        }
        common = ancestor.to_path_buf();
    }
    common
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "선택항목".to_owned())
}

fn build_args(subcommand: &str, paths: &[OsString]) -> String {
    let mut args = String::from(subcommand);
    for path in paths {
        args.push_str(" \"");
        args.push_str(&path.to_string_lossy());
        args.push('"');
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
    if let Some(configured) = read_registry_string(HKEY_CURRENT_USER, SETTINGS_KEY, EXE_PATH_VALUE) {
        if Path::new(&configured).is_file() {
            return Some(configured);
        }
    }
    let module = module_file_name()?;
    let directory = Path::new(&module).parent()?;
    let candidate = directory.join(EXE_NAME);
    candidate.is_file().then(|| candidate.into_os_string())
}

fn module_file_name() -> Option<OsString> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let mut buffer = vec![0u16; 1024];
    // SAFETY: buffer is a valid writable buffer for the module handle.
    let length = unsafe { GetModuleFileNameW(Some(module), &mut buffer) };
    if length == 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(OsString::from_wide(&buffer))
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
    let status = unsafe { RegQueryValueExW(raw, PCWSTR(value_name.as_ptr()), None, None, None, Some(&mut size)) };
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

fn register_server() -> Result<(), String> {
    let dll = module_file_name().ok_or_else(|| "could not locate the shell extension DLL".to_owned())?;
    let directory = Path::new(&dll)
        .parent()
        .ok_or_else(|| "could not resolve the DLL directory".to_owned())?;
    let exe = directory.join(EXE_NAME);
    let guid = guid_string();
    let inproc = format!(r"Software\Classes\CLSID\{guid}\InprocServer32");
    set_registry_string(HKEY_CURRENT_USER, &inproc, None, &dll.to_string_lossy())?;
    set_registry_string(HKEY_CURRENT_USER, &inproc, Some("ThreadingModel"), "Apartment")?;
    set_registry_string(
        HKEY_CURRENT_USER,
        r"Software\Classes\*\shellex\ContextMenuHandlers\ArchiveRclick",
        None,
        &guid,
    )?;
    set_registry_string(
        HKEY_CURRENT_USER,
        r"Software\Classes\Directory\shellex\ContextMenuHandlers\ArchiveRclick",
        None,
        &guid,
    )?;
    set_registry_string(HKEY_CURRENT_USER, SETTINGS_KEY, Some(EXE_PATH_VALUE), &exe.to_string_lossy())?;
    Ok(())
}

fn unregister_server() -> Result<(), String> {
    delete_registry_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\CLSID\{}", guid_string()))?;
    delete_registry_tree(HKEY_CURRENT_USER, r"Software\Classes\*\shellex\ContextMenuHandlers\ArchiveRclick")?;
    delete_registry_tree(HKEY_CURRENT_USER, r"Software\Classes\Directory\shellex\ContextMenuHandlers\ArchiveRclick")?;
    delete_registry_tree(HKEY_CURRENT_USER, SETTINGS_KEY)
}

fn set_registry_string(hive: HKEY, key_path: &str, value_name: Option<&str>, value: &str) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status = unsafe { RegCreateKeyW(hive, &key_name, &mut raw) };
    if status != ERROR_SUCCESS {
        return Err(format!("could not open registry key {key_path} (error {status:?})"));
    }
    let name = value_name.map(HSTRING::from);
    let name_ptr = name.as_ref().map(|name| PCWSTR(name.as_ptr())).unwrap_or_else(PCWSTR::null);
    let data = utf16_bytes(value);
    // SAFETY: key is live and data is valid UTF-16 including its terminator.
    let status = unsafe { RegSetValueExW(raw, name_ptr, None, REG_SZ, Some(&data)) };
    // SAFETY: handle came from RegCreateKeyW.
    let _ = unsafe { RegCloseKey(raw) };
    if status != ERROR_SUCCESS {
        return Err(format!("could not write registry value {key_path} (error {status:?})"));
    }
    Ok(())
}

fn delete_registry_tree(hive: HKEY, key_path: &str) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    // SAFETY: key name stays live for the call.
    let status = unsafe { RegDeleteTreeW(hive, &key_name) };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("could not delete registry key {key_path} (error {status:?})"))
    }
}

fn guid_string() -> String {
    let guid = CLSID_SHELL_EXT;
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

fn utf16_bytes(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
