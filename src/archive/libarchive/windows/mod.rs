use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{CStr, CString, OsString, c_char, c_int, c_long, c_void},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        },
    },
    core::{PCSTR, PCWSTR},
};

use crate::tasks::{CancellationToken, ProgressPhase, ProgressSnapshot, ThrottledProgress};

use super::super::{
    ArchiveEngine, ArchiveEntry, ArchiveEntryKind, ArchiveError, ArchiveListing, ArchiveResult,
    ConflictChoice, ConflictResolver, CreateFormat, CreateOptions, ExtractOptions,
    InitialConflictPolicy, OperationSummary, ProgressSink, encoding, ensure_no_reparse_ancestors,
    safe_relative_path,
};

mod api;
mod engine;
mod output;
mod reader;
mod source;
mod util;
mod writer;

use api::*;
use engine::{is_iso_path, is_lha_path};
use output::*;
use reader::*;
use source::*;
use util::*;
use writer::*;

pub use engine::{LibArchiveEngine, load};

#[cfg(test)]
mod tests;
