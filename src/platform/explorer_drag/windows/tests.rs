use super::*;
use ::windows::Win32::System::Com::{STREAM_SEEK_SET, StructuredStorage::CreateStreamOnHGlobal};
use ::windows::Win32::UI::Shell::IDataObjectAsyncCapability;
use std::{path::PathBuf, sync::atomic::AtomicUsize};

struct Fixture {
    payload: Arc<Payload>,
    object: IDataObject,
    calls: Arc<AtomicUsize>,
    thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    formats: Formats,
    _apartment: Apartment,
}

fn fixture() -> Fixture {
    fixture_with_files(vec![DragFile {
        archive_path: PathBuf::from("selected.txt"),
        name: PathBuf::from("선택한 파일.txt"),
        size: Some(6),
        directory: false,
        modified_unix_seconds: None,
    }])
}

fn fixture_with_files(files: Vec<DragFile>) -> Fixture {
    fixture_with_files_and_zone(files, None)
}

fn fixture_with_files_and_zone(files: Vec<DragFile>, zone: Option<Vec<u8>>) -> Fixture {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .unwrap();
    let apartment = Apartment(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let thread = Arc::new(Mutex::new(None));
    let extract_thread = Arc::clone(&thread);
    let staged_files = files.clone();
    let origin = zone.is_some();
    let payload = Arc::new(Payload {
        files,
        confirmed: Arc::new(AtomicBool::new(false)),
        cancelled: CancellationToken::new(),
        prepare: Mutex::new(Some(Box::new(move || {
            called.fetch_add(1, Ordering::SeqCst);
            *extract_thread.lock().unwrap() = Some(std::thread::current().id());
            let directory = tempfile::tempdir().unwrap();
            for file in &staged_files {
                let path = directory.path().join(&file.archive_path);
                if file.directory {
                    std::fs::create_dir_all(&path).unwrap();
                } else {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).unwrap();
                    }
                    std::fs::write(
                        &path,
                        if file.size == Some(0) {
                            &b""[..]
                        } else {
                            &b"abcdef"[..]
                        },
                    )
                    .unwrap();
                    if origin {
                        let mut ads = path.into_os_string();
                        ads.push(":Zone.Identifier");
                        std::fs::write(std::path::Path::new(&ads), b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/test.zip\r\n").unwrap();
                    }
                }
            }
            Ok(directory)
        }))),
        prepared: OnceLock::new(),
        error: Mutex::new(None),
    });
    let formats = Formats::register().unwrap();
    let mut private_formats = HashMap::new();
    if let Some(zone) = zone {
        private_formats.insert(formats.zone, zone);
    }
    let object: IDataObject = VirtualFiles {
        descriptors: descriptor_bytes(&payload.files).unwrap(),
        payload: Arc::clone(&payload),
        formats,
        private_formats: Mutex::new(private_formats),
    }
    .into();
    Fixture {
        _apartment: apartment,
        payload,
        object,
        calls,
        thread,
        formats,
    }
}

fn contents(fixture: &Fixture) -> IStream {
    unsafe {
        let mut medium = fixture
            .object
            .GetData(&fixture.formats.content_format(0))
            .unwrap();
        let stream = medium.u.pstm.as_ref().unwrap().clone();
        ReleaseStgMedium(&mut medium);
        stream
    }
}

#[test]
fn hover_and_mouse_release_do_not_extract_until_target_requests_contents() {
    let fixture = fixture();
    let source: IDropSource = DropSource {
        confirmed: Arc::clone(&fixture.payload.confirmed),
        cancel: fixture.payload.cancelled.clone(),
        input: None,
    }
    .into();
    unsafe {
        assert_eq!(source.QueryContinueDrag(false, MK_LBUTTON), S_OK);
        for _ in 0..3 {
            let mut medium = fixture
                .object
                .GetData(&fixture.formats.descriptor_format())
                .unwrap();
            let pointer = GlobalLock(medium.u.hGlobal).cast::<u8>();
            assert_eq!(pointer.cast::<u32>().read_unaligned(), 1);
            let descriptor = pointer.add(4).cast::<FILEDESCRIPTORW>().read_unaligned();
            let name = descriptor.cFileName;
            assert_eq!(
                String::from_utf16(&name[..name.iter().position(|c| *c == 0).unwrap()]).unwrap(),
                "선택한 파일.txt"
            );
            let _ = GlobalUnlock(medium.u.hGlobal);
            ReleaseStgMedium(&mut medium);
        }
        assert_eq!(
            fixture
                .object
                .GetData(&fixture.formats.content_format(0))
                .err()
                .unwrap()
                .code(),
            E_PENDING
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert!(fixture.payload.prepared.get().is_none());
        assert_eq!(
            source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)),
            DRAGDROP_S_DROP
        );
    }
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "release alone is not target acceptance"
    );
    let stream = contents(&fixture);
    let second = contents(&fixture);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_ne!(
        fixture.thread.lock().unwrap().unwrap(),
        std::thread::current().id(),
        "extraction must run on a worker"
    );
    let mut bytes = [0u8; 8];
    let mut read = 0;
    assert_eq!(
        unsafe { stream.Read(bytes.as_mut_ptr().cast(), 8, Some(&mut read)) },
        S_FALSE
    );
    assert_eq!(&bytes[..read as usize], b"abcdef");
    drop(second);
}

#[test]
fn escape_or_rejected_drop_never_creates_staging() {
    let fixture = fixture();
    let source: IDropSource = DropSource {
        confirmed: Arc::clone(&fixture.payload.confirmed),
        cancel: fixture.payload.cancelled.clone(),
        input: None,
    }
    .into();
    unsafe {
        assert_eq!(
            source.QueryContinueDrag(true, MK_LBUTTON),
            DRAGDROP_S_CANCEL
        );
        assert_eq!(
            fixture
                .object
                .GetData(&fixture.formats.content_format(0))
                .err()
                .unwrap()
                .code(),
            E_ABORT
        );
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.payload.prepared.get().is_none());
}

#[test]
fn returning_from_folder_to_the_app_cancels_without_extracting_or_creating_temp_files() {
    let fixture = fixture();
    let source = DropSource {
        confirmed: Arc::clone(&fixture.payload.confirmed),
        cancel: fixture.payload.cancelled.clone(),
        input: None,
    };
    // Hover a destination, then return to the source while still holding the
    // mouse. Even a CF_HDROP preview request must not materialize the archive.
    assert_eq!(source.continue_drag(false, MK_LBUTTON, false), S_OK);
    assert_eq!(source.continue_drag(false, MK_LBUTTON, true), S_OK);
    let paths = format(15, TYMED_HGLOBAL.0 as u32, -1);
    assert_eq!(
        unsafe { fixture.object.GetData(&paths) }
            .err()
            .unwrap()
            .code(),
        E_PENDING
    );
    assert_eq!(
        source.continue_drag(false, MODIFIERKEYS_FLAGS(0), true),
        DRAGDROP_S_CANCEL
    );
    assert!(!fixture.payload.confirmed.load(Ordering::Acquire));
    assert_eq!(
        unsafe { fixture.object.GetData(&paths) }
            .err()
            .unwrap()
            .code(),
        E_ABORT
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.payload.prepared.get().is_none());
}

#[test]
fn native_drag_loop_uses_a_dedicated_sta_without_blocking_its_caller() {
    use ::windows::Win32::System::Com::{
        APTTYPE, APTTYPE_STA, APTTYPEQUALIFIER, CoGetApartmentType,
    };
    let caller = std::thread::current().id();
    let (on_error, errors) = mpsc::sync_channel(1);
    let (started, start) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let worker = spawn_sta_task(on_error, move || {
        assert_ne!(std::thread::current().id(), caller);
        let mut apartment = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) }.unwrap();
        assert_eq!(apartment, APTTYPE_STA);
        started.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(())
    })
    .unwrap();
    start.recv_timeout(Duration::from_secs(5)).unwrap();
    // These caller/UI-side actions run while the modal drag worker is blocked.
    assert!(matches!(errors.try_recv(), Err(mpsc::TryRecvError::Empty)));
    release.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn streams_survive_object_release_without_end_operation_and_support_seek_clone_copy() {
    let fixture = fixture();
    assert!(fixture.object.cast::<IDataObjectAsyncCapability>().is_err());
    fixture.payload.confirmed.store(true, Ordering::Release);
    let stream = contents(&fixture);
    let path = fixture
        .payload
        .prepared
        .get()
        .unwrap()
        .as_ref()
        .unwrap()
        .path()
        .to_owned();
    unsafe {
        stream.Seek(2, STREAM_SEEK_SET, None).unwrap();
    }
    let clone = unsafe { stream.Clone() }.unwrap();
    let memory = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }.unwrap();
    let mut read = 0;
    let mut written = 0;
    unsafe {
        clone
            .CopyTo(&memory, 100, Some(&mut read), Some(&mut written))
            .unwrap();
    }
    assert_eq!((read, written), (4, 4));
    let mut bytes = [0u8; 4];
    unsafe {
        memory.Seek(0, STREAM_SEEK_SET, None).unwrap();
        memory
            .Read(bytes.as_mut_ptr().cast(), 4, None)
            .ok()
            .unwrap();
    }
    assert_eq!(&bytes, b"cdef");
    assert!(fixture.payload.error.lock().unwrap().is_none());
    // Model a target which drops its IDataObject without EndOperation. Stream
    // references still own the staged data, and object release is not a failure.
    drop(fixture.object);
    drop(fixture.payload);
    assert!(path.exists());
    drop(clone);
    assert!(path.exists());
    drop(stream);
    assert!(!path.exists());
}

#[test]
fn virtual_descriptors_keep_folders_empty_files_unicode_and_large_sizes() {
    let files = vec![
        DragFile {
            archive_path: "root".into(),
            name: "폴더".into(),
            size: None,
            directory: true,
            modified_unix_seconds: None,
        },
        DragFile {
            archive_path: "root/empty.txt".into(),
            name: "폴더/empty.txt".into(),
            size: Some(0),
            directory: false,
            modified_unix_seconds: Some(0),
        },
        DragFile {
            archive_path: "root/big.bin".into(),
            name: "폴더/big.bin".into(),
            size: Some(9_000_000_000),
            directory: false,
            modified_unix_seconds: None,
        },
    ];
    let bytes = descriptor_bytes(&files).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 3);
    for (index, file) in files.iter().enumerate() {
        let descriptor = unsafe {
            bytes
                .as_ptr()
                .add(4 + index * std::mem::size_of::<FILEDESCRIPTORW>())
                .cast::<FILEDESCRIPTORW>()
                .read_unaligned()
        };
        assert_eq!(
            descriptor.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
            file.directory
        );
        if let Some(size) = file.size {
            assert_eq!(
                (u64::from(descriptor.nFileSizeHigh) << 32) | u64::from(descriptor.nFileSizeLow),
                size
            );
        }
    }
    let mut unsafe_files = files;
    unsafe_files[0].name = "../outside".into();
    assert!(descriptor_bytes(&unsafe_files).is_err());
}

#[test]
fn marshalled_payload_survives_async_object_release_without_end_operation() {
    unsafe { OleInitialize(None) }.unwrap();
    let _sta = Apartment(true);
    let (ready, receive) = mpsc::sync_channel(1);
    let (finished, native_finished) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let Fixture {
            payload,
            object,
            calls,
            thread,
            formats,
            _apartment,
        } = fixture();
        let stream =
            unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &object) }.unwrap();
        ready
            .send((
                stream.into_raw() as usize,
                Arc::clone(&payload.confirmed),
                formats,
            ))
            .unwrap();
        native_finished
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        drop(object);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&payload) > 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the target did not release its payload/streams"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let path = payload
            .prepared
            .get()
            .unwrap()
            .as_ref()
            .unwrap()
            .path()
            .to_owned();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(thread.lock().unwrap().is_some());
        assert!(payload.error.lock().unwrap().is_none());
        drop(payload);
        assert!(!path.exists());
        drop(_apartment);
    });
    let (raw, confirmed, formats) = receive.recv_timeout(Duration::from_secs(10)).unwrap();
    let marshalled = ManuallyDrop::new(unsafe { IStream::from_raw(raw as *mut _) });
    let object: IDataObject = unsafe { CoGetInterfaceAndReleaseStream(&*marshalled) }.unwrap();
    assert!(object.cast::<IDataObjectAsyncCapability>().is_err());
    unsafe {
        let mut descriptors = object.GetData(&formats.descriptor_format()).unwrap();
        ReleaseStgMedium(&mut descriptors);
    }
    confirmed.store(true, Ordering::Release);
    let mut medium = unsafe { object.GetData(&formats.content_format(0)) }.unwrap();
    let stream = unsafe { medium.u.pstm.as_ref().unwrap().clone() };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    drop(object);
    finished.send(()).unwrap();
    let mut bytes = [0u8; 6];
    unsafe {
        stream
            .Read(bytes.as_mut_ptr().cast(), 6, None)
            .ok()
            .unwrap();
    }
    assert_eq!(&bytes, b"abcdef");
    drop(stream);
    worker.join().unwrap();
}

#[test]
fn native_shell_folder_requests_contents_only_after_drop_and_copies_nested_files() {
    use ::windows::{
        Win32::{
            Foundation::POINTL,
            System::Ole::IDropTarget,
            UI::{
                Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName},
                WindowsAndMessaging::{
                    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
                },
            },
        },
        core::HSTRING,
    };
    unsafe { OleInitialize(None) }.unwrap();
    let _sta = Apartment(true);
    let destination = tempfile::tempdir().unwrap();
    let shell_item: IShellItem = unsafe {
        SHCreateItemFromParsingName(&HSTRING::from(destination.path().as_os_str()), None)
    }
    .unwrap();
    let target: IDropTarget = unsafe { shell_item.BindToHandler(None, &BHID_SFUIObject) }.unwrap();
    let (ready, receive) = mpsc::sync_channel(1);
    let (finished, native_finished) = mpsc::sync_channel(1);
    let (copied, copy_done) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let entry = |name: &str, archive: &str, directory, size| DragFile {
            archive_path: archive.into(),
            name: name.into(),
            directory,
            size,
            modified_unix_seconds: None,
        };
        let archive = tempfile::NamedTempFile::new().unwrap();
        let mut ads = archive.path().as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        std::fs::write(
            std::path::Path::new(&ads),
            b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/test.zip\r\n",
        )
        .unwrap();
        let zone = source_zone_metadata(archive.path()).unwrap();
        assert!(zone.is_some());
        let Fixture {
            payload,
            object,
            calls,
            thread: _,
            formats: _,
            _apartment,
        } = fixture_with_files_and_zone(
            vec![
                entry("chosen", "chosen", true, None),
                entry("chosen/empty", "chosen/empty", true, None),
                entry(
                    "chosen/선택한 파일.txt",
                    "chosen/선택한 파일.txt",
                    false,
                    Some(6),
                ),
                entry("chosen/zero.txt", "chosen/zero.txt", false, Some(0)),
            ],
            zone,
        );
        let stream =
            unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &object) }.unwrap();
        ready
            .send((
                stream.into_raw() as usize,
                Arc::clone(&payload.confirmed),
                Arc::clone(&calls),
            ))
            .unwrap();
        native_finished
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        drop(object);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while Arc::strong_count(&payload) > 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "Shell retained the transfer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            payload.error.lock().unwrap().is_none(),
            "{:?}",
            payload.error.lock().unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let path = payload
            .prepared
            .get()
            .unwrap()
            .as_ref()
            .unwrap()
            .path()
            .to_owned();
        drop(payload);
        assert!(!path.exists());
        drop(_apartment);
        copied.send(()).unwrap();
    });
    let (raw, confirmed, calls) = receive.recv_timeout(Duration::from_secs(15)).unwrap();
    let marshalled = ManuallyDrop::new(unsafe { IStream::from_raw(raw as *mut _) });
    let object: IDataObject = unsafe { CoGetInterfaceAndReleaseStream(&*marshalled) }.unwrap();
    let mut effect = DROPEFFECT_COPY;
    unsafe {
        target
            .DragEnter(&object, MK_LBUTTON, POINTL::default(), &mut effect)
            .unwrap();
        target
            .DragOver(MK_LBUTTON, POINTL::default(), &mut effect)
            .unwrap();
    }
    assert_eq!(effect, DROPEFFECT_COPY);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "Shell hover extracted files"
    );
    confirmed.store(true, Ordering::Release);
    unsafe {
        target.Drop(
            &object,
            MODIFIERKEYS_FLAGS(0),
            POINTL::default(),
            &mut effect,
        )
    }
    .unwrap();
    assert_eq!(effect, DROPEFFECT_COPY);
    drop(object);
    finished.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if copy_done.try_recv().is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native Shell copy did not finish"
        );
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.join().unwrap();
    drop(target);
    assert_eq!(
        std::fs::read(destination.path().join("chosen/선택한 파일.txt")).unwrap(),
        b"abcdef"
    );
    assert_eq!(
        std::fs::metadata(destination.path().join("chosen/zero.txt"))
            .unwrap()
            .len(),
        0
    );
    assert!(destination.path().join("chosen/empty").is_dir());
    let mut ads = destination
        .path()
        .join("chosen/선택한 파일.txt")
        .into_os_string();
    ads.push(":Zone.Identifier");
    let zone = std::fs::read_to_string(std::path::Path::new(&ads)).unwrap();
    assert!(
        zone.contains("ZoneId=3"),
        "origin metadata was lost: {zone}"
    );
    let mut empty_ads = destination.path().join("chosen/zero.txt").into_os_string();
    empty_ads.push(":Zone.Identifier");
    assert!(
        std::fs::read_to_string(std::path::Path::new(&empty_ads))
            .unwrap()
            .contains("ZoneId=3")
    );
}
