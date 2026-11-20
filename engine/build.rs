//! Embed the O11A1 icon in the Electron sidecar executable. It links the
//! native player as a library, so the resource has to be added here as well as
//! in the standalone `native_player` crate; the runtime window icon is set in
//! `native_player::window`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let icon = manifest.join("../assets/o11a1.ico");
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.is_file() {
        println!("cargo:warning=O11A1 icon missing at {}", icon.display());
        return;
    }

    let icon_copy = out.join("o11a1.ico");
    fs::copy(&icon, &icon_copy).expect("copy app icon");
    let rc_file = out.join("o11a1.rc");
    fs::write(&rc_file, "1 ICON \"o11a1.ico\"\n").expect("write rc");
    let res_file = out.join("o11a1.res");

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; executable icon stays default");
        return;
    };
    let status = Command::new(rc)
        .current_dir(&out)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status()
        .expect("run rc.exe");
    if !status.success() {
        panic!("rc.exe failed with {status}");
    }
    println!("cargo:rustc-link-arg={}", res_file.display());
}

fn find_rc() -> Option<PathBuf> {
    if let Ok(value) = env::var("RC") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(kits)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    for version in versions.into_iter().rev() {
        let candidate = version.join("x64").join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    Some(PathBuf::from("rc.exe"))
}
