use std::{collections::BTreeMap, env, fs, path::PathBuf};

use sha2::{Digest, Sha256};

const RUNTIME_FILES: &[&str] = &[
    "archive.dll",
    "bz2.dll",
    "liblzma.dll",
    "lz4.dll",
    "vcruntime140.dll",
    "z.dll",
    "zstd.dll",
];
const LICENSE_FILES: &[&str] = &[
    "bzip2.txt",
    "libarchive.txt",
    "liblzma.txt",
    "lz4.txt",
    "zlib.txt",
    "zstd.txt",
];

fn main() {
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=runtime/x64");
    println!("cargo:rerun-if-changed=runtime/licenses");
    println!("cargo:rerun-if-changed=runtime/SHA256SUMS");
    println!("cargo:rerun-if-changed=runtime/THIRD-PARTY-NOTICES.md");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.ico");
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");
    slint_build::compile("ui/progress_window.slint")
        .expect("failed to compile Slint progress-window UI");

    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap_or_else(|error| panic!("failed to embed app icon resource: {error}"));
        let target_arch =
            env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo did not set CARGO_CFG_TARGET_ARCH");
        assert_eq!(
            target_arch, "x86_64",
            "ArchiveRclick currently bundles only the Windows x86_64 native runtime"
        );
        copy_runtime_bundle();
    }
}

fn copy_runtime_bundle() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo did not set CARGO_MANIFEST_DIR"),
    );
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected Cargo OUT_DIR layout");
    let runtime_dir = manifest_dir.join("runtime/x64");
    validate_runtime_bundle(&manifest_dir, &runtime_dir);

    for name in RUNTIME_FILES {
        let source = runtime_dir.join(name);
        let destination = profile_dir.join(name);
        fs::copy(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy native runtime {} to {}: {error}",
                source.display(),
                destination.display()
            )
        });
    }

    let notice = manifest_dir.join("runtime/THIRD-PARTY-NOTICES.md");
    let destination = profile_dir.join("THIRD-PARTY-NOTICES.md");
    fs::copy(&notice, &destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy native runtime notices {} to {}: {error}",
            notice.display(),
            destination.display()
        )
    });

    let license_source = manifest_dir.join("runtime/licenses");
    let license_destination = profile_dir.join("licenses");
    fs::create_dir_all(&license_destination).unwrap_or_else(|error| {
        panic!(
            "failed to create native runtime license directory {}: {error}",
            license_destination.display()
        )
    });
    for name in LICENSE_FILES {
        let source = license_source.join(name);
        let destination = license_destination.join(name);
        fs::copy(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy native runtime license {} to {}: {error}",
                source.display(),
                destination.display()
            )
        });
    }
}

fn validate_runtime_bundle(manifest_dir: &std::path::Path, runtime_dir: &std::path::Path) {
    let hashes_path = manifest_dir.join("runtime/SHA256SUMS");
    let hashes = fs::read_to_string(&hashes_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", hashes_path.display()));
    let mut expected = BTreeMap::new();
    for (line_number, line) in hashes.lines().enumerate() {
        let Some((hash, name)) = line.split_once("  ") else {
            panic!(
                "invalid SHA256SUMS entry at {}:{}",
                hashes_path.display(),
                line_number + 1
            );
        };
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            panic!(
                "invalid SHA-256 at {}:{}",
                hashes_path.display(),
                line_number + 1
            );
        }
        if expected.insert(name, hash.to_ascii_lowercase()).is_some() {
            panic!("duplicate runtime file {name} in {}", hashes_path.display());
        }
    }

    let declared = expected.keys().copied().collect::<Vec<_>>();
    let mut required = RUNTIME_FILES.to_vec();
    required.sort_unstable();
    if declared != required {
        panic!(
            "{} must declare exactly {:?}, found {:?}",
            hashes_path.display(),
            required,
            declared
        );
    }

    let mut actual_files = fs::read_dir(runtime_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", runtime_dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("failed to enumerate runtime bundle: {error}"))
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    actual_files.sort_unstable();
    if actual_files != required {
        panic!(
            "{} must contain exactly {:?}, found {:?}",
            runtime_dir.display(),
            required,
            actual_files
        );
    }

    for name in RUNTIME_FILES {
        let path = runtime_dir.join(name);
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let actual = format!("{:x}", Sha256::digest(bytes));
        let wanted = expected.get(name).expect("runtime file was checked above");
        if &actual != wanted {
            panic!(
                "SHA-256 mismatch for {}: expected {wanted}, got {actual}",
                path.display()
            );
        }
    }
}
