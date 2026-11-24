use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=platform/windows_startup.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let target = env::var("TARGET").unwrap();
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("clipwipe-startup.exe");
    let mut rustc = Command::new(env::var_os("RUSTC").unwrap());
    rustc.args([
        "--edition=2021",
        "--target",
        &target,
        "--crate-name",
        "clipwipe_startup",
    ]);
    rustc.args([
        "-C",
        "opt-level=z",
        "-C",
        "panic=abort",
        "-C",
        "strip=symbols",
    ]);
    if let Some(linker) = env::var_os("RUSTC_LINKER") {
        let mut flag = std::ffi::OsString::from("linker=");
        flag.push(linker);
        rustc.arg("-C").arg(flag);
    }
    let status = rustc
        .arg("platform/windows_startup.rs")
        .arg("-o")
        .arg(output)
        .status()
        .expect("failed to launch rustc for Windows login helper");
    assert!(status.success(), "Windows login helper compilation failed");
}
