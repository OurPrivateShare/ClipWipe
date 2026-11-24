#![windows_subsystem = "windows"]

use std::{
    env, io,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

fn main() -> io::Result<()> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let binary = env::current_exe()?.with_file_name("clipwipe.exe");
    Command::new(binary)
        .arg("watch")
        .args(env::args_os().skip(1))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
