use super::{Action, acknowledge, poll};
use crate::{Countdown, Mode, Result, random_text};
use std::os::fd::AsFd;
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

x11rb::atom_manager! {
    TransferAtoms: TransferAtomCookies {
        TARGETS, TIMESTAMP, MULTIPLE, ATOM_PAIR, UTF8_STRING, TEXT,
        plain: b"text/plain",
        plain_utf8: b"text/plain;charset=utf-8",
    }
}

struct Clipboard {
    conn: RustConnection,
    window: Window,
    selection: Atom,
    transfer_atoms: Option<TransferAtoms>,
    countdown: Countdown,
    owner: Window,
    acquired: Timestamp,
    text: Option<String>,
}

impl Clipboard {
    fn note_owner(&mut self, owner: Window, timestamp: Timestamp) {
        self.owner = owner;
        self.acquired = timestamp;
        self.countdown
            .changed(Instant::now(), owner != NONE && owner != self.window);
    }

    fn convert(
        &self,
        atoms: &TransferAtoms,
        window: Window,
        target: Atom,
        property: Atom,
        text: &[u8],
    ) -> Result<bool> {
        if property == NONE {
            return Ok(false);
        }
        let strings = [
            atoms.UTF8_STRING,
            AtomEnum::STRING.into(),
            atoms.plain,
            atoms.plain_utf8,
            atoms.TEXT,
        ];
        if target == atoms.TARGETS {
            let mut supported = [atoms.TARGETS; 8];
            supported[1] = atoms.TIMESTAMP;
            supported[2] = atoms.MULTIPLE;
            supported[3..].copy_from_slice(&strings);
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::ATOM,
                &supported,
            )?;
        } else if target == atoms.TIMESTAMP {
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::INTEGER,
                &[self.acquired],
            )?;
        } else if strings.contains(&target) {
            let encoding = if target == atoms.TEXT {
                atoms.UTF8_STRING
            } else {
                target
            };
            self.conn
                .change_property8(PropMode::REPLACE, window, property, encoding, text)?;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    fn convert_multiple(
        &self,
        atoms: &TransferAtoms,
        req: &SelectionRequestEvent,
        text: &[u8],
    ) -> Result<bool> {
        if req.property == NONE {
            return Ok(false);
        }
        let reply = self
            .conn
            .get_property(false, req.requestor, req.property, atoms.ATOM_PAIR, 0, 4096)?
            .reply();
        // A requester may disappear during the transfer.
        let Ok(reply) = reply else { return Ok(false) };
        let Some(words) = reply.value32() else {
            return Ok(false);
        };
        let mut pairs: Vec<_> = words.collect();
        if reply.type_ != atoms.ATOM_PAIR
            || reply.bytes_after != 0
            || !pairs.len().is_multiple_of(2)
        {
            return Ok(false);
        }
        for pair in pairs.as_chunks_mut::<2>().0 {
            if pair[1] == req.property
                || !self.convert(atoms, req.requestor, pair[0], pair[1], text)?
            {
                pair[1] = NONE;
            }
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            req.requestor,
            req.property,
            atoms.ATOM_PAIR,
            &pairs,
        )?;
        Ok(true)
    }

    fn serve(&self, req: SelectionRequestEvent) -> Result<()> {
        let property = if req.property == NONE {
            req.target
        } else {
            req.property
        };
        let mut success = false;
        if req.owner == self.window
            && req.selection == self.selection
            && let Some(atoms) = &self.transfer_atoms
            && let Some(text) = &self.text
        {
            success = if req.target == atoms.MULTIPLE {
                self.convert_multiple(atoms, &req, text.as_bytes())?
            } else {
                self.convert(atoms, req.requestor, req.target, property, text.as_bytes())?
            };
        }
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: if success { property } else { NONE },
        };
        self.conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, reply)?;
        Ok(())
    }

    fn track(&mut self, event: Event) -> Option<SelectionRequestEvent> {
        match event {
            Event::XfixesSelectionNotify(e) if e.selection == self.selection => {
                self.note_owner(e.owner, e.selection_timestamp);
                None
            }
            Event::SelectionRequest(req) => Some(req),
            _ => None,
        }
    }

    fn dispatch(&mut self) -> Result<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            if let Some(request) = self.track(event) {
                self.serve(request)?;
            }
        }
        // An earlier notification can precede the confirmation of our own write
        // in the same batch. Keep the new paste data until both are dispatched.
        if self.owner != self.window {
            self.text = None;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn acquire(&mut self, text: Option<String>) -> Result<()> {
        let new_owner = if text.is_some() { self.window } else { NONE };
        self.conn
            .set_selection_owner(new_owner, self.selection, CURRENT_TIME)?
            .check()?;
        self.owner = new_owner;
        self.text = text;
        self.countdown.clear();
        Ok(())
    }

    fn wipe_due(&mut self, mode: Mode) -> Result<()> {
        let text = match mode {
            Mode::Clear => None,
            Mode::Random => Some(random_text()?),
        };
        // The grab plus a server round trip orders pending same-window copies as well as
        // changes of owner before our check. No other client can copy between check and set.
        let mut requests = Vec::new();
        self.conn.grab_server()?;
        let result = (|| -> Result<()> {
            let owner = self
                .conn
                .get_selection_owner(self.selection)?
                .reply()?
                .owner;
            while let Some(event) = self.conn.poll_for_event()? {
                if let Some(req) = self.track(event) {
                    requests.push(req);
                }
            }
            if self.owner != owner {
                self.note_owner(owner, CURRENT_TIME);
            } else if self
                .countdown
                .remaining(Instant::now())
                .is_some_and(|d| d.is_zero())
            {
                self.acquire(text)?;
            }
            Ok(())
        })();
        self.conn.ungrab_server()?.check()?;
        // Transfers may make additional round trips and must run outside the grab.
        for request in requests {
            self.serve(request)?;
        }
        result
    }
}

pub(super) fn run(action: Action) -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let selection = conn.intern_atom(false, b"CLIPBOARD")?;
    if matches!(action, Action::Clear) {
        let selection = selection.reply()?.atom;
        conn.set_selection_owner(NONE, selection, CURRENT_TIME)?
            .check()?;
        if conn.get_selection_owner(selection)?.reply()?.owner != NONE {
            return Err("X11 剪贴板在清空后已被其他程序接管".into());
        }
        return Ok(());
    }
    let version = conn.xfixes_query_version(1, 0)?;
    // A clear-only watcher never publishes data, so it only needs CLIPBOARD.
    let transfer_atoms = action
        .may_write_random()
        .then(|| TransferAtoms::new(&conn))
        .transpose()?;
    let selection = selection.reply()?.atom;
    let version = version.reply()?;
    if version.major_version < 1 {
        return Err("此 X11 会话缺少 XFixes 选区变更通知".into());
    }
    let transfer_atoms = transfer_atoms.map(TransferAtomCookies::reply).transpose()?;
    let window = conn.generate_id()?;
    let root = conn.setup().roots[screen].root;
    let create = conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    let mask = SelectionEventMask::SET_SELECTION_OWNER
        | SelectionEventMask::SELECTION_WINDOW_DESTROY
        | SelectionEventMask::SELECTION_CLIENT_CLOSE;
    let subscribe = conn.xfixes_select_selection_input(window, selection, mask)?;
    // Wait on the owner query before checking the earlier requests. One reply
    // confirms the server has processed all three.
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    create.check()?;
    subscribe.check()?;
    let mut clipboard = Clipboard {
        conn,
        window,
        selection,
        transfer_atoms,
        countdown: Countdown::new(action.timeout()),
        owner,
        acquired: CURRENT_TIME,
        text: None,
    };
    clipboard.note_owner(owner, CURRENT_TIME);
    clipboard.dispatch()?;
    if matches!(action, Action::HoldRandom) {
        clipboard.acquire(Some(random_text()?))?;
        let owner = clipboard
            .conn
            .get_selection_owner(clipboard.selection)?
            .reply()?
            .owner;
        clipboard.dispatch()?;
        if owner != window || clipboard.owner != window {
            return Err("X11 随机文本的剪贴板所有权已丢失".into());
        }
        acknowledge()?;
    }
    loop {
        clipboard.dispatch()?;
        let remaining = match action {
            Action::Watch(options) => {
                let delay = clipboard.countdown.remaining(Instant::now());
                if delay.is_some_and(|d| d.is_zero()) {
                    clipboard.wipe_due(options.mode)?;
                    continue;
                }
                delay
            }
            Action::HoldRandom if clipboard.owner != window => return Ok(()),
            _ => None,
        };
        poll(clipboard.conn.stream().as_fd(), libc::POLLIN, remaining)?;
    }
}
