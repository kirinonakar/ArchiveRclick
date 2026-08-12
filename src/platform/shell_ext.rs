//! Explorer context-menu shell extension (IContextMenu + IExplorerCommand).
//!
//! Static registry verbs cannot show per-file labels, so this DLL computes
//! the menu items from the selected paths and displays the real names, e.g.
//! "보고서.zip으로 압축하기", "보고서.7z로 압축하기" and "보고서\ 에 풀기".
//! IContextMenu feeds the classic menu and Windows 11's "추가 옵션 표시";
//! IExplorerCommand shows the same verbs in the Windows 11 default menu.
//! Invoking an item launches archive-rclick.exe directly with Unicode
//! arguments (no PowerShell, no cmd), and the app shows its progress window.
#![cfg(windows)]

use std::{
    ffi::{c_void, OsString},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
    sync::Mutex,
};

use windows::{
    core::{implement, w, BOOL, IUnknown, Interface, Ref, GUID, HSTRING, PCWSTR, PSTR, PWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::{
            Com::{
                CoTaskMemAlloc, CoTaskMemFree, FORMATETC, IClassFactory, IClassFactory_Impl,
                IBindCtx, IDataObject, STGMEDIUM,
            },
            LibraryLoader::{GetModuleFileNameW, GetModuleHandleW},
            Ole::ReleaseStgMedium,
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RegCloseKey,
                RegCreateKeyW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW,
            },
        },
        UI::{
            Shell::{
                CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, DragQueryFileW, ECF_HASSUBCOMMANDS,
                ECS_ENABLED, ECS_HIDDEN, HDROP, IContextMenu, IContextMenu_Impl,
                IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand,
                IExplorerCommand_Impl, IShellExtInit, IShellExtInit_Impl, IShellItem,
                IShellItemArray, SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
                SIGDN_FILESYSPATH, ShellExecuteW,
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
const S_FALSE: windows::core::HRESULT = windows::core::HRESULT(1);
const E_POINTER: windows::core::HRESULT = windows::core::HRESULT(0x8000_4003u32 as i32);
const E_NOINTERFACE: windows::core::HRESULT = windows::core::HRESULT(0x8000_4002u32 as i32);
const E_NOTIMPL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4001u32 as i32);
const E_FAIL: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005u32 as i32);
const E_OUTOFMEMORY: windows::core::HRESULT = windows::core::HRESULT(0x8007_000Eu32 as i32);
const CLASS_E_CLASSNOTAVAILABLE: windows::core::HRESULT = windows::core::HRESULT(0x8004_0111u32 as i32);

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

#[implement(IShellExtInit, IContextMenu, IExplorerCommand)]
struct ArchiveContextMenu {
    paths: Mutex<Vec<OsString>>,
    cmd_first: Mutex<u32>,
}

impl Drop for ArchiveContextMenu {
    fn drop(&mut self) {
        decrement_live_objects();
    }
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
        let paths_buf: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let verbs = menu_verbs(&paths_buf);

        let mut next_id = id_cmd_first;
        let mut count = 0u32;
        for (_, label) in &verbs {
            if next_id > id_cmd_last {
                break;
            }
            if let Some(wide_label) = wide(label) {
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
            OFF_EXTRACT => run_exe(&exe, &build_args("extract", &paths)),
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
// IExplorerCommand: the Windows 11 default menu reads this interface from the
// same CLSID. The root item hosts a cascade whose children carry the verbs,
// so the per-file labels ("보고서.zip으로 압축하기" ...) appear in the new
// menu as well.
// ---------------------------------------------------------------------------

impl IExplorerCommand_Impl for ArchiveContextMenu_Impl {
    fn GetTitle(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        wide_alloc("ArchiveRclick").ok_or_else(|| E_OUTOFMEMORY.into())
    }

    fn GetIcon(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> WinResult<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(
        &self,
        psiitemarray: Ref<'_, IShellItemArray>,
        _foktobeslow: BOOL,
    ) -> WinResult<u32> {
        let visible = psiitemarray
            .as_ref()
            .is_some_and(|array| !item_array_paths(array).is_empty());
        Ok(if visible { ECS_ENABLED.0 as u32 } else { ECS_HIDDEN.0 as u32 })
    }

    fn Invoke(
        &self,
        _psiitemarray: Ref<'_, IShellItemArray>,
        _pbc: Ref<'_, IBindCtx>,
    ) -> WinResult<()> {
        // The root item only hosts the cascade; each verb invokes the app.
        Ok(())
    }

    fn GetFlags(&self) -> WinResult<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> WinResult<IEnumExplorerCommand> {
        let commands: Vec<IExplorerCommand> = [Verb::Extract, Verb::Zip, Verb::SevenZip]
            .into_iter()
            .map(|verb| {
                increment_live_objects();
                ArchiveVerbCommand::new(verb).into()
            })
            .collect();
        increment_live_objects();
        Ok(VerbEnumerator::new(commands).into())
    }
}

/// The three actions the shell extension can run on a selection.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Extract,
    Zip,
    SevenZip,
}

impl Verb {
    fn subcommand(self) -> &'static str {
        match self {
            Verb::Extract => "extract",
            Verb::Zip => "zip",
            Verb::SevenZip => "7z",
        }
    }
}

/// (verb, label) pairs for the current selection, in menu order. Labels are
/// computed from the real file names so each entry stays per-file, e.g.
/// "보고서.zip으로 압축하기", "보고서.7z로 압축하기", "보고서\ 에 풀기".
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
            vec![(Verb::Extract, format!("{final_name}\\ 에 풀기"))]
        } else {
            // 여러 압축파일을 선택하면 각각 자기 이름의 폴더에 푼다.
            vec![(Verb::Extract, "각각의 폴더에 풀기".to_owned())]
        }
    } else {
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
            (Verb::Zip, format!("{zip_name}으로 압축하기")),
            (Verb::SevenZip, format!("{seven_name}로 압축하기")),
        ]
    }
}

/// One IExplorerCommand subcommand ("압축하기"/"풀기"). Explorer asks for the
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
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _psiitemarray: Ref<'_, IShellItemArray>) -> WinResult<PWSTR> {
        Err(E_NOTIMPL.into())
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
        Ok(if visible { ECS_ENABLED.0 as u32 } else { ECS_HIDDEN.0 as u32 })
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

#[implement(IEnumExplorerCommand)]
struct VerbEnumerator {
    commands: Vec<IExplorerCommand>,
    cursor: Mutex<usize>,
}

impl VerbEnumerator {
    fn new(commands: Vec<IExplorerCommand>) -> Self {
        Self {
            commands,
            cursor: Mutex::new(0),
        }
    }
}

impl Drop for VerbEnumerator {
    fn drop(&mut self) {
        decrement_live_objects();
    }
}

impl IEnumExplorerCommand_Impl for VerbEnumerator_Impl {
    fn Next(
        &self,
        celt: u32,
        puicommand: *mut Option<IExplorerCommand>,
        pceltfetched: *mut u32,
    ) -> windows::core::HRESULT {
        if puicommand.is_null() {
            return E_POINTER;
        }
        let Ok(mut cursor) = self.cursor.lock() else {
            return E_FAIL;
        };
        let mut fetched = 0u32;
        while fetched < celt {
            let Some(command) = self.commands.get(*cursor).cloned() else {
                break;
            };
            // SAFETY: puicommand points to writable storage for celt interface
            // pointers and fetched is always below celt here.
            unsafe { puicommand.add(fetched as usize).write(Some(command)) };
            *cursor += 1;
            fetched += 1;
        }
        if !pceltfetched.is_null() {
            // SAFETY: pceltfetched is the out-parameter provided by the caller.
            unsafe { *pceltfetched = fetched };
        }
        if fetched == celt {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, celt: u32) -> WinResult<()> {
        let Ok(mut cursor) = self.cursor.lock() else {
            return Err(E_FAIL.into());
        };
        *cursor = cursor.saturating_add(celt as usize);
        Ok(())
    }

    fn Reset(&self) -> WinResult<()> {
        let Ok(mut cursor) = self.cursor.lock() else {
            return Err(E_FAIL.into());
        };
        *cursor = 0;
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumExplorerCommand> {
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
            let result = ((*vtbl).QueryInterface)(object, riid, ppvobj);
            if result.is_ok() {
                // The caller now holds a reference: keep the DLL resident
                // until the object is released.
                increment_live_objects();
            }
            result.ok()
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
    let guid = guid_string();
    let inproc = format!(r"Software\Classes\CLSID\{guid}\InprocServer32");
    set_registry_string(HKEY_CURRENT_USER, &inproc, None, &dll_path.to_string_lossy())?;
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
    notify_shell_change();
    Ok(())
}

/// Removes the registry entries written by [`register_context_menu`]. The
/// `Software\ArchiveRclick` key itself is kept because it also holds the app's
/// settings; only the recorded executable path is deleted.
pub fn unregister_context_menu() -> Result<(), String> {
    delete_registry_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\CLSID\{}", guid_string()))?;
    delete_registry_tree(HKEY_CURRENT_USER, r"Software\Classes\*\shellex\ContextMenuHandlers\ArchiveRclick")?;
    delete_registry_tree(HKEY_CURRENT_USER, r"Software\Classes\Directory\shellex\ContextMenuHandlers\ArchiveRclick")?;
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
    registry_key_exists(
        HKEY_CURRENT_USER,
        &format!(r"Software\Classes\CLSID\{}\InprocServer32", guid_string()),
    )
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
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not delete registry key {key_path} (error {status:?})"))
    }
}

fn delete_registry_value(hive: HKEY, key_path: &str, value_name: &str) -> Result<(), String> {
    let key_name = HSTRING::from(key_path);
    let mut raw = HKEY(ptr::null_mut());
    // SAFETY: key name stays live and `raw` is an out-parameter.
    let status = unsafe { RegOpenKeyExW(hive, &key_name, None, KEY_READ | KEY_SET_VALUE, &mut raw) };
    if status != ERROR_SUCCESS {
        return Err(format!("could not open registry key {key_path} (error {status:?})"));
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

fn utf16_bytes(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
