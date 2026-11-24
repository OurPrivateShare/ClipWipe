use crate::{Result, WatchOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy)]
pub(crate) enum WatchStart {
    Foreground,
    Background,
}

impl WatchStart {
    pub(crate) fn ready(self) -> Result<()> {
        if matches!(self, Self::Background) {
            reply("READY")?;
        }
        Ok(())
    }

    pub(crate) fn already_running(self, message: &str) -> Result<()> {
        match self {
            Self::Foreground => Err(message.into()),
            Self::Background => Ok(reply("ALREADY")?),
        }
    }
}

fn reply(message: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "{message}")?;
    output.flush()
}

pub(crate) fn watch(options: WatchOptions) -> Result<()> {
    let result = crate::clipboard::watch(options, WatchStart::Background);
    if let Err(error) = &result {
        let _ = reply(&format!("ERROR {error}"));
    }
    result
}

pub(crate) enum StartStatus {
    Started,
    AlreadyRunning,
}

pub(crate) fn start(path: &Path, options: WatchOptions) -> Result<StartStatus> {
    let mut command = Command::new(path);
    command
        .arg("__background-watch")
        .arg("--timeout")
        .arg(options.timeout.as_secs().to_string())
        .args(["--mode", options.mode.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; this is a freshly forked child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }

    let mut child = command.spawn()?;
    let pipe = child.stdout.take().expect("stdout configured as piped");
    let (sender, receiver) = mpsc::channel();
    let result = (|| -> Result<StartStatus> {
        std::thread::Builder::new().spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(pipe).read_line(&mut line).map(|_| line);
            let _ = sender.send(read);
        })?;
        let line = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "未在时限内收到后台监听的启动结果")??;
        match line.trim_end() {
            "READY" => Ok(StartStatus::Started),
            "ALREADY" => {
                child.wait()?;
                Ok(StartStatus::AlreadyRunning)
            }
            response => Err(response
                .strip_prefix("ERROR ")
                .filter(|message| !message.is_empty())
                .unwrap_or("后台进程未确认监听就绪")
                .to_owned()
                .into()),
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
