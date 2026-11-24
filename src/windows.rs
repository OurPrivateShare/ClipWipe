use crate::{Countdown, Mode, Result, WatchOptions, random_text};
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, GlobalFree, HGLOBAL, HWND,
    SetLastError, WAIT_FAILED,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, CountClipboardFormats, EmptyClipboard,
    GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard, RemoveClipboardFormatListener,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, INFINITE, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    WM_CLIPBOARDUPDATE, WM_QUIT, WS_CHILD,
};
use windows_sys::w;

struct Window {
    handle: HWND,
    listening: bool,
}

impl Window {
    fn new() -> io::Result<Self> {
        // STATIC is a predefined system class. A message-only window has no visible UI.
        let handle = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                null(),
                WS_CHILD,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                null_mut(),
                null(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            listening: false,
        })
    }

    fn listen(&mut self) -> io::Result<()> {
        if unsafe { AddClipboardFormatListener(self.handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        self.listening = true;
        Ok(())
    }

    fn drain_messages(&self) -> Option<bool> {
        let mut updated = false;
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                return None;
            }
            if message.hwnd == self.handle && message.message == WM_CLIPBOARDUPDATE {
                updated = true;
            } else {
                unsafe { DispatchMessageW(&message) };
            }
        }
        Some(updated)
    }

    fn update_pending(&self) -> bool {
        let mut message = MSG::default();
        // Leave the queued update for drain_messages so it is observed just once.
        unsafe {
            PeekMessageW(
                &mut message,
                self.handle,
                WM_CLIPBOARDUPDATE,
                WM_CLIPBOARDUPDATE,
                PM_NOREMOVE,
            ) != 0
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            if self.listening {
                RemoveClipboardFormatListener(self.handle);
            }
            DestroyWindow(self.handle);
        }
    }
}

struct GlobalText(HGLOBAL);

impl GlobalText {
    fn new(text: &str) -> io::Result<Self> {
        let utf16 = text.encode_utf16().chain(Some(0));
        let memory =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, utf16.clone().count() * size_of::<u16>()) };
        if memory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let allocation = Self(memory);
        let destination = unsafe { GlobalLock(memory) }.cast::<u16>();
        if destination.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            for (index, unit) in utf16.enumerate() {
                destination.add(index).write(unit);
            }
            SetLastError(0);
            if GlobalUnlock(memory) == 0 && GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(allocation)
    }

    fn publish(self) -> io::Result<()> {
        if unsafe { SetClipboardData(u32::from(CF_UNICODETEXT), self.0) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        // A successful SetClipboardData transfers the allocation to the system.
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for GlobalText {
    fn drop(&mut self) {
        unsafe { GlobalFree(self.0) };
    }
}

struct Clipboard;

impl Clipboard {
    fn open(window: HWND) -> io::Result<Self> {
        if unsafe { OpenClipboard(window) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }

    fn replace(&self, mode: Mode) -> Result<()> {
        // Prepare the payload before removing the old contents. Never read user data.
        let replacement = match mode {
            Mode::Clear => None,
            Mode::Random => Some(GlobalText::new(&random_text()?)?),
        };
        if unsafe { EmptyClipboard() } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        if let Some(text) = replacement {
            text.publish()
                .map_err(|error| format!("剪贴板已清空，写入随机文本失败：{error}"))?;
        }
        Ok(())
    }

    fn close(self) -> io::Result<()> {
        if unsafe { CloseClipboard() } == 0 {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ClipboardVersion {
    sequence: u32,
    owner: HWND,
}

impl ClipboardVersion {
    fn read() -> Self {
        unsafe {
            Self {
                sequence: GetClipboardSequenceNumber(),
                owner: GetClipboardOwner(),
            }
        }
    }

    fn schedule(self, window: HWND, clock: &mut Countdown) -> io::Result<()> {
        // Notification handling needs only metadata. Reserving the clipboard
        // here would contend with copy and paste for every update.
        let present = if self.owner == window {
            false
        } else {
            unsafe {
                SetLastError(0);
                let formats = CountClipboardFormats();
                let error = GetLastError();
                if formats == 0 && error != 0 {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                formats != 0
            }
        };
        clock.changed(Instant::now(), present);
        Ok(())
    }
}

struct WriteRetry {
    next: Instant,
    deadline: Instant,
    delay: Duration,
}

impl WriteRetry {
    fn after_failure(previous: Option<Self>, now: Instant) -> Option<Self> {
        let (deadline, delay) = previous.map_or(
            (now + Duration::from_secs(5), Duration::from_millis(20)),
            |retry| {
                (
                    retry.deadline,
                    (retry.delay * 2).min(Duration::from_secs(2)),
                )
            },
        );
        (now < deadline).then(|| Self {
            next: (now + delay).min(deadline),
            deadline,
            delay,
        })
    }
}

pub(crate) fn apply(mode: Mode) -> Result<()> {
    let window = Window::new()?;
    let start = Instant::now();
    let mut delay = Duration::from_millis(10);
    let clipboard = loop {
        match Clipboard::open(window.handle) {
            Ok(clipboard) => break clipboard,
            Err(error) if start.elapsed() >= Duration::from_secs(1) => {
                return Err(format!("无法打开剪贴板：{error}").into());
            }
            Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(100));
            }
        }
    };
    clipboard.replace(mode)?;
    clipboard.close()?;
    Ok(())
}

fn session_mutex_name() -> io::Result<Vec<u16>> {
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = 0;
    unsafe { GetUserObjectInformationW(station, UOI_NAME, null_mut(), 0, &mut bytes) };
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = vec![0u16; (bytes as usize).div_ceil(2)];
    if unsafe {
        GetUserObjectInformationW(
            station,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let station_name = name.strip_suffix(&[0]).unwrap_or(&name);
    let mut mutex = String::from("Local\\ClipWipe.Watch");
    if !station_name.iter().copied().eq("WinSta0".encode_utf16()) {
        // Window stations have separate clipboards. Keep kernel object names short
        // even when the window station name itself approaches MAX_PATH.
        let hash = station_name
            .iter()
            .fold(0xcbf29ce484222325u64, |hash, unit| {
                (hash ^ u64::from(*unit)).wrapping_mul(0x100000001b3)
            });
        mutex.push_str(&format!(".{hash:016x}"));
    }
    Ok(mutex.encode_utf16().chain(Some(0)).collect())
}

pub(crate) fn watcher_running() -> Result<bool> {
    let name = session_mutex_name()?;
    let raw = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            return Ok(false);
        }
        return Err(error.into());
    }
    drop(unsafe { OwnedHandle::from_raw_handle(raw) });
    Ok(true)
}

fn session_lock() -> Result<OwnedHandle> {
    let name = session_mutex_name()?;
    let raw = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    if already_exists {
        return Err("当前桌面会话已在运行 ClipWipe 监听。".into());
    }
    Ok(handle)
}

fn wait_for_input(duration: Option<Duration>) -> io::Result<()> {
    let millis = duration.map_or(INFINITE, |wait| {
        wait.as_nanos()
            .div_ceil(1_000_000)
            .min(u128::from(INFINITE - 1)) as u32
    });
    if unsafe { MsgWaitForMultipleObjectsEx(0, null(), millis, QS_ALLINPUT, MWMO_INPUTAVAILABLE) }
        == WAIT_FAILED
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn watch(options: WatchOptions) -> Result<()> {
    let _lock = session_lock()?;
    let mut window = Window::new()?;
    window.listen()?;
    let mut clock = Countdown::new(options.timeout);
    let mut version = ClipboardVersion::read();
    version.schedule(window.handle, &mut clock)?;
    let mut retry: Option<WriteRetry> = None;
    while let Some(updated) = window.drain_messages() {
        if updated {
            version = ClipboardVersion::read();
            version.schedule(window.handle, &mut clock)?;
            retry = None;
        }

        let now = Instant::now();
        let expired = clock.remaining(now) == Some(Duration::ZERO);
        if expired && retry.as_ref().is_none_or(|retry| now >= retry.next) {
            match Clipboard::open(window.handle) {
                Ok(clipboard) => {
                    retry = None;
                    let current = ClipboardVersion::read();
                    // A queued copy may have the same owner and sequence when
                    // its clipboard formats use delayed rendering.
                    if window.update_pending() {
                        clipboard.close()?;
                        continue;
                    }
                    if current != version {
                        current.schedule(window.handle, &mut clock)?;
                        version = current;
                    } else {
                        clock.clear();
                        // Report a failed write and keep listening to future copies.
                        if let Err(error) = clipboard.replace(options.mode) {
                            eprintln!("ClipWipe 清理失败：{error}");
                        }
                    }
                    clipboard.close()?;
                }
                Err(error) => {
                    retry = WriteRetry::after_failure(retry, now);
                    if retry.is_none() {
                        eprintln!("ClipWipe 仍无法打开剪贴板，等待下一次复制：{error}");
                        clock.clear();
                    }
                }
            }
        }
        let now = Instant::now();
        let wait = retry
            .as_ref()
            .map(|retry| retry.next.saturating_duration_since(now))
            .or_else(|| clock.remaining(now));
        wait_for_input(wait)?;
    }
    Ok(())
}
