use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

mod cli;
mod install;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as clipboard;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as clipboard;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("ClipWipe supports Windows and Linux graphical sessions");

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Clear,
    Random,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Random => "random",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WatchOptions {
    timeout: Duration,
    mode: Mode,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            mode: Mode::Clear,
        }
    }
}

struct Countdown {
    timeout: Duration,
    changed_at: Option<Instant>,
}

impl Countdown {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            changed_at: None,
        }
    }

    fn changed(&mut self, now: Instant, present: bool) {
        self.changed_at = present.then_some(now);
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.changed_at.map(|changed_at| {
            self.timeout
                .saturating_sub(now.saturating_duration_since(changed_at))
        })
    }

    fn clear(&mut self) {
        self.changed_at = None;
    }
}

fn random_text() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
    Ok(text)
}

fn run(command: cli::Command) -> Result<()> {
    use cli::Command;

    match command {
        Command::Help(sections) => print_information(sections),
        Command::Version => print_information(&[]),
        Command::Clear(mode) => clipboard::apply(mode),
        Command::Watch(options) => clipboard::watch(options),
        Command::Install => install::install(),
        Command::Uninstall => install::uninstall(),
        Command::Enable(options) => install::enable(options),
        Command::Disable => install::disable(),
        #[cfg(target_os = "linux")]
        Command::HoldRandom => clipboard::hold_random(),
    }
}

fn version() -> &'static str {
    concat!("ClipWipe ", env!("CARGO_PKG_VERSION"))
}

fn print_information(sections: &[&str]) -> Result<()> {
    let mut output = io::stdout().lock();
    let result: io::Result<()> = (|| {
        writeln!(output, "{}", version())?;
        if !sections.is_empty() {
            writeln!(output)?;
            for section in sections {
                output.write_all(section.as_bytes())?;
            }
        }
        output.flush()
    })();
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

fn main() -> ExitCode {
    let arguments = std::env::args_os()
        .skip(1)
        .map(|argument| argument.into_string().map_err(|_| "参数必须是有效文本"))
        .collect::<std::result::Result<Vec<_>, _>>();
    let command = match arguments
        .map_err(String::from)
        .and_then(|args| cli::parse(&args))
    {
        Ok(command) => command,
        Err(error) => {
            eprintln!("clipwipe: {error}\n运行 clipwipe --help 查看用法。");
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("clipwipe: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_copy_gets_the_full_delay() {
        let start = Instant::now();
        let mut countdown = Countdown::new(Duration::from_secs(30));
        assert_eq!(countdown.remaining(start), None);
        countdown.changed(start, true);
        countdown.changed(start + Duration::from_secs(29), true);
        assert_eq!(
            countdown.remaining(start + Duration::from_secs(30)),
            Some(Duration::from_secs(29))
        );
        assert_eq!(
            countdown.remaining(start + Duration::from_secs(59)),
            Some(Duration::ZERO)
        );
        countdown.clear();
        assert_eq!(countdown.remaining(start + Duration::from_secs(59)), None);
    }

    #[test]
    fn empty_clipboard_disarms_the_timer() {
        let start = Instant::now();
        let mut countdown = Countdown::new(Duration::from_secs(u64::MAX));
        countdown.changed(start, true);
        assert_eq!(
            countdown.remaining(start),
            Some(Duration::from_secs(u64::MAX))
        );
        countdown.changed(start + Duration::from_secs(1), false);
        assert_eq!(countdown.remaining(start + Duration::from_secs(30)), None);
    }

    #[test]
    fn random_override_is_ascii_hex() {
        let value = random_text().unwrap();
        assert_eq!(value.len(), 32);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
