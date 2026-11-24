mod wayland;
mod x11;

use crate::{Mode, Result, WatchOptions};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs};

#[derive(Clone, Copy)]
enum Action {
    Clear,
    HoldRandom,
    Watch(WatchOptions),
}

impl Action {
    fn timeout(self) -> Duration {
        match self {
            Self::Watch(options) => options.timeout,
            _ => Duration::ZERO,
        }
    }

    fn may_write_random(self) -> bool {
        matches!(
            self,
            Self::HoldRandom
                | Self::Watch(WatchOptions {
                    mode: Mode::Random,
                    ..
                })
        )
    }
}

fn using_wayland() -> bool {
    env::var("XDG_SESSION_TYPE").is_ok_and(|s| s.eq_ignore_ascii_case("wayland"))
        || env::var_os("WAYLAND_DISPLAY").is_some_and(|s| !s.is_empty())
        || env::var_os("WAYLAND_SOCKET").is_some_and(|s| !s.is_empty())
}

fn run(action: Action) -> Result<()> {
    if using_wayland() {
        wayland::run(action)
    } else if env::var_os("DISPLAY").is_some_and(|s| !s.is_empty()) {
        x11::run(action)
    } else {
        Err("当前没有可用的 Linux 图形会话（缺少 Wayland/X11 显示）".into())
    }
}

fn wayland_display_path(display: &OsStr, runtime: Option<&OsStr>) -> Option<PathBuf> {
    let display = Path::new(display);
    let socket = if display.is_absolute() {
        display.to_owned()
    } else {
        let runtime = Path::new(runtime?);
        if !runtime.is_absolute() {
            return None;
        }
        runtime.join(display)
    };
    Some(fs::canonicalize(&socket).unwrap_or(socket))
}

fn x11_server(display: &str) -> Option<(&str, u16)> {
    let (host, screen) = display.rsplit_once(':')?;
    let number = screen.split('.').next()?.parse().ok()?;
    let host = if host.starts_with("unix/") {
        ""
    } else {
        host.strip_prefix("tcp/").unwrap_or(host)
    };
    Some((host, number))
}

pub(crate) fn watch(options: WatchOptions) -> Result<()> {
    // Abstract sockets vanish when the last descriptor is closed, even on a crash.
    let mut session = DefaultHasher::new();
    let wayland = using_wayland();
    wayland.hash(&mut session);
    if wayland {
        let display = env::var_os("WAYLAND_DISPLAY");
        let runtime = env::var_os("XDG_RUNTIME_DIR");
        if let Some(path) = display
            .as_deref()
            .and_then(|display| wayland_display_path(display, runtime.as_deref()))
        {
            path.hash(&mut session);
        } else {
            (display, runtime).hash(&mut session);
        }
    } else {
        // Screen numbers on the same X server share the CLIPBOARD selection.
        let display = env::var("DISPLAY").unwrap_or_default();
        if let Some(server) = x11_server(&display) {
            server.hash(&mut session);
        } else {
            display.hash(&mut session);
        }
    }
    // SAFETY: geteuid has no arguments or preconditions.
    let name = format!(
        "clipwipe-{}-{:016x}",
        unsafe { libc::geteuid() },
        session.finish()
    );
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    let _instance = UnixListener::bind_addr(&address).map_err(|err| {
        if err.kind() == io::ErrorKind::AddrInUse {
            io::Error::new(err.kind(), "当前图形会话已有 ClipWipe 正在监听")
        } else {
            err
        }
    })?;
    run(Action::Watch(options))
}

pub(crate) fn apply(mode: Mode) -> Result<()> {
    if matches!(mode, Mode::Clear) {
        return run(Action::Clear);
    }
    // X11 and Wayland fetch data from its owner when a paste is requested.
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("__hold-random")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and the child has not become a group leader.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    };
    let mut child = command.spawn()?;
    let mut output = BufReader::new(child.stdout.take().ok_or("随机覆盖握手管道不可用")?);
    let status = (|| -> Result<()> {
        let state = poll(
            output.get_ref().as_fd(),
            libc::POLLIN,
            Some(Duration::from_secs(10)),
        )?;
        if state == 0 {
            return Err("等待 Linux 剪贴板所有权确认超时".into());
        }
        let mut line = String::new();
        output.read_line(&mut line)?;
        if line.trim_end() == "READY" {
            return Ok(());
        }
        Err(line
            .trim()
            .strip_prefix("ERROR ")
            .filter(|s| !s.is_empty())
            .unwrap_or("Linux 剪贴板持有进程启动失败")
            .to_string()
            .into())
    })();
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    status
}

pub(crate) fn hold_random() -> Result<()> {
    let result = run(Action::HoldRandom);
    if let Err(err) = &result {
        let _ = writeln!(io::stdout().lock(), "ERROR {err}");
    }
    result
}

fn acknowledge() -> Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "READY")?;
    output.flush()?;
    Ok(())
}

fn poll(fd: BorrowedFd<'_>, events: i16, remaining: Option<Duration>) -> io::Result<i16> {
    let mut interest = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    let timeout = remaining.map(|time| (Instant::now(), time));
    loop {
        let millis = timeout.map_or(-1, |(start, time)| {
            time.saturating_sub(start.elapsed())
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32
        });
        // SAFETY: this is one initialized descriptor, live for the duration of poll.
        if unsafe { libc::poll(&mut interest, 1, millis) } >= 0 {
            if interest.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            return Ok(interest.revents);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_display_identity_uses_the_resolved_socket_path() {
        let relative =
            wayland_display_path(OsStr::new("wayland-0"), Some(OsStr::new("/run/user/12345")));
        let absolute = wayland_display_path(
            OsStr::new("/run/user/12345/wayland-0"),
            Some(OsStr::new("/run/user/54321")),
        );
        assert!(relative.is_some());
        assert_eq!(relative, absolute);
        assert_ne!(
            relative,
            wayland_display_path(OsStr::new("/run/user/12345/wayland-1"), None)
        );
    }

    #[test]
    fn x11_display_identity_ignores_local_transport_alias_and_screen() {
        assert_eq!(x11_server(":0"), x11_server("unix/:00.1"));
        assert_eq!(x11_server("localhost:0"), x11_server("tcp/localhost:0.1"));
        assert_ne!(x11_server(":0"), x11_server(":1"));
        assert_ne!(x11_server(":0"), x11_server("localhost:0"));
    }
}
