use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

#[allow(unused)]
#[derive(Debug)]
pub(crate) struct TargetDir {
    path: PathBuf,
    profile: String,
    profile_dir: PathBuf,
}

// Partly based on https://github.com/dtolnay/cxx/blob/master/gen/build/src/target.rs
//
// This is a workaround for issues like: https://github.com/rust-lang/cargo/issues/9661
//
// This is making assumptions about the relationship between OUT_DIR and the
// target directory but ¯\_(ツ)_/¯ Cargo doesn't give a nice way of handling this
// and this crate is very app-specific anyway.
//
pub(crate) fn find_target_dir(out_dir: &Path, target: &str) -> Option<TargetDir> {
    let target_canonical = if let Some(target_dir) = env::var_os("CARGO_TARGET_DIR") {
        let target_dir = Path::new(&target_dir);
        assert!(
            target_dir.is_absolute(),
            "Can't infer target directory and profile with a relative CARGO_TARGET_DIR path"
        );
        Some(target_dir.canonicalize().unwrap())
    } else {
        None
    };

    let mut popped = vec![];
    let mut dir = out_dir.to_owned();
    loop {
        if dir.join("CACHEDIR.TAG").exists()
            || target_canonical
                .as_ref()
                .is_some_and(|path| path == &dir.canonicalize().unwrap())
            || dir.file_name() == Some(OsStr::new("target"))
        {
            let prev = popped
                .pop()
                .expect("OUT_DIR not expected to be the target directory");
            if prev == target {
                let profile = popped
                    .pop()
                    .expect("OUT_DIR expected to be nested under target/<triple>/<profile>");
                let profile_dir = dir.join(target).join(&profile);
                return Some(TargetDir {
                    path: dir,
                    profile,
                    profile_dir,
                });
            } else {
                let profile_dir = dir.join(&prev);
                return Some(TargetDir {
                    path: dir,
                    profile: prev,
                    profile_dir,
                });
            }
        }
        popped.push(dir.file_name().unwrap().to_str().unwrap().to_owned());
        if dir.pop() {
            continue;
        }

        return None;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    windows::build!(
        Windows::Devices::Bluetooth::*,
        Windows::Devices::Bluetooth::Advertisement::*,
        Windows::Devices::Bluetooth::GenericAttributeProfile::*,
        Windows::Foundation::{
            IAsyncOperation,
            IReference,
            EventRegistrationToken,
            TypedEventHandler,
        },
        Windows::Foundation::Collections::{
            IVector,
            IVectorView,
        },
        Windows::Storage::Streams::{
            IBuffer,
            DataReader,
            DataWriter,
        },
        Windows::Win32::Foundation::*, // HRESULTS
    );

    let target_os = env::var("CARGO_CFG_TARGET_OS")?;
    if target_os == "android" {
        //let pkg_ver = env::var("CARGO_PKG_VERSION")?;
        let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
        let manifest_dir = Path::new(&manifest_dir);
        eprintln!("manifest dir = {manifest_dir:?}");
        let java_dir = manifest_dir.join("src").join("java");
        eprintln!("java dir = {java_dir:?}");

        let out_dir = env::var("OUT_DIR").unwrap();
        let target = std::env::var("TARGET").unwrap();
        let target_dir =
            find_target_dir(Path::new(&out_dir), &target).expect("Failed to find target directory");

        env::set_current_dir(&java_dir)?;

        eprintln!("target dir = {target_dir:?}");
        let local_maven_dir = target_dir.path.join("maven");
        eprintln!("local maven dir = {local_maven_dir:?}");

        println!("cargo:warning=Building Gradle project under {java_dir:?}");
        let gradlew_path = java_dir.join("gradlew");

        let mut gradle_command = Command::new(gradlew_path);

        gradle_command.arg("publish");
        gradle_command.arg("-Pcargo.targetDir={local_maven_dir}");

        println!("cargo:warning=Running {gradle_command:?}");
        //gradle_command.execute_check_exit_status_code(0).unwrap();
    }

    Ok(())
}
