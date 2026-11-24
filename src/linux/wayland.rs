use super::{Action, acknowledge, poll};
use crate::{Countdown, Mode, Result, random_text};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_device, ext_data_control_manager_v1 as ext_manager,
    ext_data_control_offer_v1 as ext_offer, ext_data_control_source_v1 as ext_source,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_device, zwlr_data_control_manager_v1 as wlr_manager,
    zwlr_data_control_offer_v1 as wlr_offer, zwlr_data_control_source_v1 as wlr_source,
};

enum Manager {
    Ext(ext_manager::ExtDataControlManagerV1),
    Wlr(wlr_manager::ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ext_device::ExtDataControlDeviceV1),
    Wlr(wlr_device::ZwlrDataControlDeviceV1),
}

#[derive(PartialEq, Eq)]
enum Selection {
    Empty,
    External,
    Own,
}

struct Seat {
    proxy: wl_seat::WlSeat,
    device: Option<Device>,
    countdown: Countdown,
    selection: Selection,
    awaiting_confirmation: bool,
    source: Option<wayland_client::backend::ObjectId>,
}

impl Seat {
    fn attach(&mut self, manager: &Manager, qh: &QueueHandle<State>, name: u32) {
        self.device = Some(match manager {
            Manager::Ext(manager) => Device::Ext(manager.get_data_device(&self.proxy, qh, name)),
            Manager::Wlr(manager) => Device::Wlr(manager.get_data_device(&self.proxy, qh, name)),
        });
    }

    fn detach(&mut self) {
        match self.device.take() {
            Some(Device::Ext(device)) => device.destroy(),
            Some(Device::Wlr(device)) => device.destroy(),
            None => (),
        }
        self.countdown.clear();
        self.source = None;
        self.selection = Selection::Empty;
        self.awaiting_confirmation = false;
    }
}

struct State {
    timeout: Duration,
    marker: String,
    ext: Option<u32>,
    wlr: Option<u32>,
    manager_name: Option<u32>,
    manager: Option<Manager>,
    seats: BTreeMap<u32, Seat>,
    manager_lost: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == ext_manager::ExtDataControlManagerV1::interface().name {
                    state.ext = Some(name);
                } else if interface == wlr_manager::ZwlrDataControlManagerV1::interface().name {
                    state.wlr = Some(name);
                } else if interface == wl_seat::WlSeat::interface().name {
                    let mut seat = Seat {
                        proxy: registry.bind(name, version.min(5), qh, ()),
                        device: None,
                        countdown: Countdown::new(state.timeout),
                        selection: Selection::Empty,
                        awaiting_confirmation: false,
                        source: None,
                    };
                    if let Some(manager) = &state.manager {
                        seat.attach(manager, qh, name);
                    }
                    state.seats.insert(name, seat);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(mut seat) = state.seats.remove(&name) {
                    seat.detach();
                    if seat.proxy.version() >= 5 {
                        seat.proxy.release();
                    }
                }
                if state.manager_name == Some(name) {
                    state.manager_lost = true;
                }
                if state.ext == Some(name) {
                    state.ext = None;
                }
                if state.wlr == Some(name) {
                    state.wlr = None;
                }
            }
            _ => (),
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ext_manager::ExtDataControlManagerV1);
delegate_noop!(State: ignore wlr_manager::ZwlrDataControlManagerV1);

struct SourceData {
    seat: u32,
    text: String,
}

fn fd_flags(fd: RawFd) -> io::Result<libc::c_int> {
    // SAFETY: fd remains owned by the caller for this call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags)
}

fn set_fd_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: fd remains owned by the caller and flags are valid F_SETFL values.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn send_text(fd: OwnedFd, text: &str) -> io::Result<()> {
    let mut output = File::from(fd);
    let fd = output.as_raw_fd();
    let flags = fd_flags(fd)?;
    if flags & libc::O_NONBLOCK != 0 {
        return output.write_all(text.as_bytes());
    }

    // A full paste pipe must not stall clipboard monitoring. Restore shared
    // flags before closing the descriptor, including after a failed write.
    set_fd_flags(fd, flags | libc::O_NONBLOCK)?;
    let sent = output.write_all(text.as_bytes());
    sent.and(set_fd_flags(fd, flags))
}

// Both protocol versions carry the same selection, MIME and transfer events.
macro_rules! clipboard_dispatch {
    ($device:ident, $d:ident, $offer:ident, $o:ident, $source:ident, $s:ident) => {
        impl Dispatch<$device::$d, u32> for State {
            fn event(
                state: &mut Self, _: &$device::$d, event: $device::Event,
                name: &u32, _: &Connection, _: &QueueHandle<Self>,
            ) {
                match event {
                    $device::Event::Selection { id } => {
                        if let Some(seat) = state.seats.get_mut(name) {
                            seat.selection = match id.as_ref() {
                                None => Selection::Empty,
                                Some(offer) if offer.data::<AtomicBool>().is_some_and(|own|
                                    own.load(Ordering::Relaxed)) => Selection::Own,
                                Some(_) => Selection::External,
                            };
                            seat.awaiting_confirmation = false;
                            if seat.selection == Selection::External {
                                seat.countdown.changed(Instant::now(), true);
                            } else {
                                seat.countdown.clear();
                            }
                        }
                        // We consume only offer metadata, never request clipboard data.
                        if let Some(offer) = id {
                            offer.destroy();
                        }
                    }
                    $device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                    $device::Event::Finished => {
                        if let Some(seat) = state.seats.get_mut(name) {
                            seat.detach();
                        }
                    }
                    _ => (),
                }
            }
            event_created_child!(State, $device::$d, [
                $device::EVT_DATA_OFFER_OPCODE => ($offer::$o, AtomicBool::new(false))
            ]);
        }

        impl Dispatch<$offer::$o, AtomicBool> for State {
            fn event(
                state: &mut Self, _: &$offer::$o, event: $offer::Event,
                own: &AtomicBool, _: &Connection, _: &QueueHandle<Self>,
            ) {
                if let $offer::Event::Offer { mime_type } = event
                    && !state.marker.is_empty() && mime_type == state.marker {
                    own.store(true, Ordering::Relaxed);
                }
            }
        }

        impl Dispatch<$source::$s, SourceData> for State {
            fn event(
                state: &mut Self, source: &$source::$s, event: $source::Event,
                data: &SourceData, _: &Connection, _: &QueueHandle<Self>,
            ) {
                match event {
                    $source::Event::Send { mime_type, fd } => {
                        if matches!(mime_type.as_str(), "text/plain" | "text/plain;charset=utf-8") {
                            // A receiver can stop its paste before the transfer.
                            let _ = send_text(fd, &data.text);
                        }
                    }
                    $source::Event::Cancelled => {
                        if let Some(seat) = state.seats.get_mut(&data.seat)
                            && seat.source.as_ref() == Some(&source.id()) {
                            seat.source = None;
                        }
                        source.destroy();
                    }
                    _ => (),
                }
            }
        }
    };
}
clipboard_dispatch!(
    ext_device,
    ExtDataControlDeviceV1,
    ext_offer,
    ExtDataControlOfferV1,
    ext_source,
    ExtDataControlSourceV1
);
clipboard_dispatch!(
    wlr_device,
    ZwlrDataControlDeviceV1,
    wlr_offer,
    ZwlrDataControlOfferV1,
    wlr_source,
    ZwlrDataControlSourceV1
);

impl State {
    fn check_manager(&self) -> Result<()> {
        if self.manager_lost {
            return Err("Wayland 会话已撤销剪贴板管理接口".into());
        }
        Ok(())
    }

    fn require_device(&self) -> Result<()> {
        self.check_manager()?;
        if !self.seats.values().any(|s| s.device.is_some()) {
            return Err("Wayland 会话没有可用的剪贴板设备".into());
        }
        Ok(())
    }

    fn remaining(&self) -> Option<Duration> {
        let now = Instant::now();
        self.seats
            .values()
            .filter_map(|s| s.countdown.remaining(now))
            .min()
    }

    fn write(&mut self, mode: Mode, due_only: bool, qh: &QueueHandle<Self>) -> Result<bool> {
        let now = due_only.then(Instant::now);
        let mut wrote = false;
        for (&name, seat) in &mut self.seats {
            let Some(device) = &seat.device else { continue };
            if now.is_some_and(|now| !seat.countdown.remaining(now).is_some_and(|r| r.is_zero())) {
                continue;
            }
            macro_rules! set_selection {
                ($manager:expr, $device:expr) => {{
                    if matches!(mode, Mode::Random) {
                        let source = $manager.create_data_source(
                            qh,
                            SourceData {
                                seat: name,
                                text: random_text()?,
                            },
                        );
                        for mime in ["text/plain", "text/plain;charset=utf-8", &self.marker] {
                            source.offer(mime.to_owned());
                        }
                        seat.source = Some(source.id());
                        $device.set_selection(Some(&source));
                    } else {
                        $device.set_selection(None);
                    }
                }};
            }
            match (&self.manager, device) {
                (Some(Manager::Ext(m)), Device::Ext(d)) => set_selection!(m, d),
                (Some(Manager::Wlr(m)), Device::Wlr(d)) => set_selection!(m, d),
                _ => return Err("Wayland 剪贴板管理器与设备协议不匹配".into()),
            }
            seat.countdown.clear();
            seat.awaiting_confirmation = true;
            wrote = true;
        }
        Ok(wrote)
    }
}

fn would_block(err: &WaylandError) -> bool {
    matches!(err, WaylandError::Io(err) if err.kind() == ErrorKind::WouldBlock)
}

fn wait(queue: &mut EventQueue<State>, timeout: Option<Duration>) -> Result<()> {
    let mut interest = libc::POLLIN;
    if let Err(err) = queue.flush() {
        if !would_block(&err) {
            return Err(err.into());
        }
        interest |= libc::POLLOUT;
    }
    if let Some(guard) = queue.prepare_read() {
        let events = poll(guard.connection_fd(), interest, timeout)?;
        if events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            && let Err(err) = guard.read()
            && !would_block(&err)
        {
            return Err(err.into());
        }
    }
    Ok(())
}

pub(super) fn run(action: Action) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut state = State {
        timeout: action.timeout(),
        marker: if action.may_write_random() {
            format!("application/x-clipwipe-{}", random_text()?)
        } else {
            String::new()
        },
        ext: None,
        wlr: None,
        manager_name: None,
        manager: None,
        seats: BTreeMap::new(),
        manager_lost: false,
    };
    queue.roundtrip(&mut state)?;
    let (name, manager) = if let Some(id) = state.ext {
        (id, Manager::Ext(registry.bind(id, 1, &qh, ())))
    } else if let Some(id) = state.wlr {
        (id, Manager::Wlr(registry.bind(id, 1, &qh, ())))
    } else {
        return Err(
            "当前 Wayland 桌面未提供 ext-data-control 或 wlr-data-control 剪贴板管理接口".into(),
        );
    };
    for (&name, seat) in &mut state.seats {
        seat.attach(&manager, &qh, name);
    }
    state.manager = Some(manager);
    state.manager_name = Some(name);
    match action {
        Action::Clear => {
            // After the registry round trip, binding and writing can be issued
            // together. The next sync delivers the initial and changed selection.
            state.require_device()?;
            state.write(Mode::Clear, false, &qh)?;
            queue.roundtrip(&mut state)?;
            state.require_device()?;
            if state
                .seats
                .values()
                .any(|s| s.selection != Selection::Empty)
            {
                return Err("Wayland 剪贴板在清空后仍有选区".into());
            }
            return Ok(());
        }
        Action::HoldRandom => {
            state.require_device()?;
            state.write(Mode::Random, false, &qh)?;
            queue.roundtrip(&mut state)?;
            state.require_device()?;
            if state.seats.values().any(|s| {
                s.device.is_some() && (s.selection != Selection::Own || s.source.is_none())
            }) {
                return Err("Wayland 未确认随机文本的剪贴板所有权".into());
            }
            acknowledge()?;
        }
        Action::Watch(_) => (),
    }
    loop {
        queue.dispatch_pending(&mut state)?;
        if matches!(action, Action::HoldRandom) && state.seats.values().all(|s| s.source.is_none())
        {
            return Ok(());
        }
        // A compositor can temporarily remove every seat. Watch keeps waiting
        // for the registry to announce a new one, with no deadline armed.
        state.check_manager()?;
        let mut remaining = None;
        if let Action::Watch(options) = action {
            remaining = state.remaining();
            if remaining.is_some_and(|d| d.is_zero()) {
                // Data-control has no conditional write. Synchronize all earlier
                // changes and recheck each seat as close to the write as possible.
                queue.roundtrip(&mut state)?;
                state.check_manager()?;
                if state.write(options.mode, true, &qh)? {
                    queue.roundtrip(&mut state)?;
                    state.check_manager()?;
                    if state.seats.values().any(|s| s.awaiting_confirmation) {
                        return Err("Wayland 未确认到期的剪贴板操作".into());
                    }
                }
                continue;
            }
        }
        wait(&mut queue, remaining)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn full_paste_pipe_does_not_block_or_leave_writer_nonblocking() {
        let (reader, mut writer) = io::pipe().unwrap();
        let fd = writer.as_raw_fd();
        let flags = fd_flags(fd).unwrap();
        set_fd_flags(fd, flags | libc::O_NONBLOCK).unwrap();
        let page = [0; 4096];
        while let Ok(count) = writer.write(&page) {
            assert!(count > 0);
        }
        set_fd_flags(fd, flags).unwrap();

        let duplicate = writer.try_clone().unwrap();
        let (sender, receiver) = mpsc::channel();
        let transfer = thread::spawn(move || {
            sender
                .send(send_text(duplicate.into(), "random clipboard text"))
                .unwrap();
        });
        let outcome = receiver.recv_timeout(Duration::from_secs(2));
        // Release a blocked worker before failing the test if nonblocking writes regress.
        drop(reader);
        transfer.join().unwrap();
        assert_eq!(outcome.unwrap().unwrap_err().kind(), ErrorKind::WouldBlock);
        assert_eq!(fd_flags(fd).unwrap(), flags);
    }
}
