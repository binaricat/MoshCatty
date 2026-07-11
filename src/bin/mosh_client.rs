//! Drop-in `mosh-client` CLI for Netcatty (and standalone use).
//!
//! ```text
//! MOSH_KEY=<key> mosh-client <host> <port>
//! ```
//!
//! Cross-platform I/O:
//! - Unix: raw + non-blocking stdin (node-pty compatible; cfmakeraw disables ISIG
//!   so Ctrl+C is a byte, not SIGINT that would kill the client)
//! - Windows: dedicated stdin thread so UDP poll/keepalive never block
//!   (fixes ConPTY/node-pty stall — multi-agent audit CRITICAL);
//!   also installs a console ctrl handler so ConPTY Ctrl+C is not
//!   STATUS_CONTROL_C_EXIT (Netcatty / node-pty path)

use std::env;
use std::io::{self, Read, Write};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use moshcatty::{Client, DisplayPreference, LocalPredictor};

fn main() {
    if let Err(e) = run() {
        eprintln!("mosh-client: {e}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(all(windows, debug_assertions, feature = "conpty-test-probe"))]
    if env::var_os("MOSHCATTY_CONPTY_TEST").is_some() {
        return run_conpty_input_probe();
    }

    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-#" || args[i] == "--help" {
            print_usage();
            process::exit(0);
        }
        if args[i].starts_with('-') {
            if args[i] == "-p" || args[i] == "-s" || args[i] == "-c" {
                args.remove(i);
                if i < args.len() {
                    args.remove(i);
                }
                continue;
            }
            args.remove(i);
            continue;
        }
        i += 1;
    }

    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    let host = args[0].clone();
    let port: u16 = args[1]
        .parse()
        .map_err(|_| format!("invalid port: {}", args[1]))?;
    let key = env::var("MOSH_KEY").map_err(|_| "MOSH_KEY environment variable is required")?;

    let cols = env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(term_cols)
        .unwrap_or(80u16);
    let rows = env::var("LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(term_rows)
        .unwrap_or(24u16);

    let mut client = Client::dial(&host, port, &key)?;
    client.resize(cols, rows);

    let running = Arc::new(AtomicBool::new(true));
    install_signal_flag(running.clone());

    let _raw_guard = enter_raw_mode_if_tty()?;

    // Always use a stdin thread so the UDP loop never blocks (Unix+Windows).
    let stdin_rx = spawn_stdin_reader();

    let mut stdout = io::stdout();
    // Match stock mosh-client Display::open/close: run the session on the local
    // alternate screen, restore primary buffer (and cursor/mouse modes) on exit.
    // Set MOSH_NO_TERM_INIT=1 to skip (same env as upstream mosh).
    let _display = DisplaySession::enter(&mut stdout)?;
    // Speculative local echo (underline until HostBytes confirm). Required for
    // high-latency "feels like local" typing; see Netcatty #2121 / stock mosh.
    let mut predictor = LocalPredictor::new(DisplayPreference::from_env());
    let mut last_resize_check = Instant::now();
    let mut cur_cols = cols;
    let mut cur_rows = rows;

    while running.load(Ordering::SeqCst) {
        if client.is_dead() {
            if let Some(r) = client.dead_reason() {
                eprintln!("mosh-client: {r}");
            }
            break;
        }

        // Keep adaptive prediction in sync with measured SRTT.
        predictor.set_srtt(client.srtt());

        let paint = client.poll()?;
        if !paint.is_empty() {
            // Server frame is authoritative: drop outstanding guesses so the
            // next keystroke predicts against the confirmed screen.
            predictor.on_host_paint();
            stdout.write_all(&paint)?;
            stdout.flush()?;
        }

        match stdin_rx.try_recv() {
            Ok(Some(buf)) if !buf.is_empty() => {
                let local = predictor.predict(&buf);
                if !local.is_empty() {
                    stdout.write_all(&local)?;
                    stdout.flush()?;
                }
                client.send_keys(&buf);
            }
            Ok(None) => {
                // EOF: drain remaining paint briefly then exit (PTY closed).
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && !client.is_dead() {
                    let paint = client.poll()?;
                    if paint.is_empty() {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    predictor.on_host_paint();
                    stdout.write_all(&paint)?;
                    stdout.flush()?;
                }
                break;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
            Ok(Some(_)) => {}
        }

        if last_resize_check.elapsed() > Duration::from_millis(250) {
            if let (Some(c), Some(r)) = (term_cols(), term_rows()) {
                if c != cur_cols || r != cur_rows {
                    cur_cols = c;
                    cur_rows = r;
                    client.resize(c, r);
                }
            }
            last_resize_check = Instant::now();
        }

        thread::sleep(Duration::from_millis(2));
    }

    Ok(())
}

#[cfg(all(windows, debug_assertions, feature = "conpty-test-probe"))]
fn run_conpty_input_probe() -> Result<(), Box<dyn std::error::Error>> {
    const MAX_PROBE_BYTES: usize = 4096;
    let expected_bytes: usize = env::var("MOSHCATTY_CONPTY_TEST_BYTES")
        .map_err(|_| "MOSHCATTY_CONPTY_TEST_BYTES is required")?
        .parse()
        .map_err(|_| "MOSHCATTY_CONPTY_TEST_BYTES must be an integer")?;
    if expected_bytes > MAX_PROBE_BYTES {
        return Err("MOSHCATTY_CONPTY_TEST_BYTES exceeds the probe limit".into());
    }
    let running = Arc::new(AtomicBool::new(true));
    install_signal_flag(running);
    let _raw_guard = enter_raw_mode_if_tty()?.ok_or("ConPTY test probe requires a console")?;
    println!("MOSHCATTY_CONPTY_READY");
    io::stdout().flush()?;
    let mut input = vec![0u8; expected_bytes];
    io::stdin().read_exact(&mut input)?;
    let hex = input
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("MOSHCATTY_INPUT_HEX={hex}");
    Ok(())
}

/// Stock mosh `Display::open()`: enter alternate screen + application cursor keys.
/// Uses xterm-compatible CSI (no terminfo). Equivalent to smcup + `\e[?1h`.
const DISPLAY_OPEN: &[u8] = b"\x1b[?1049h\x1b[?1h";

/// Stock mosh `Display::close()`: leave app-cursor / reset SGR / show cursor /
/// disable common mouse modes / leave alternate screen (rmcup).
const DISPLAY_CLOSE: &[u8] = b"\x1b[?1l\x1b[0m\x1b[?25h\
\x1b[?1003l\x1b[?1002l\x1b[?1001l\x1b[?1000l\
\x1b[?1015l\x1b[?1006l\x1b[?1005l\
\x1b[?1049l";

/// RAII guard so rmcup always runs on session end (clean exit, EOF, panic drop).
struct DisplaySession {
    active: bool,
}

impl DisplaySession {
    fn enter(stdout: &mut impl Write) -> io::Result<Self> {
        if env::var_os("MOSH_NO_TERM_INIT").is_some() {
            return Ok(Self { active: false });
        }
        stdout.write_all(DISPLAY_OPEN)?;
        stdout.flush()?;
        Ok(Self { active: true })
    }
}

impl Drop for DisplaySession {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Best-effort restore; do not panic in Drop.
        let mut out = io::stdout();
        let _ = out.write_all(DISPLAY_CLOSE);
        let _ = out.flush();
        self.active = false;
    }
}

/// Background stdin reader. Sends `Some(bytes)` on data, `None` on EOF.
fn spawn_stdin_reader() -> Receiver<Option<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(None);
                    break;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });
    rx
}

fn print_usage() {
    eprintln!("Usage: MOSH_KEY=<key> mosh-client <host> <port>");
    eprintln!("Pure Rust Mosh client (Netcatty). No Cygwin / terminfo required.");
}

#[cfg(unix)]
fn term_cols() -> Option<u16> {
    winsize()
        .map(|w| w.ws_col)
        .or_else(|| env::var("COLUMNS").ok().and_then(|s| s.parse().ok()))
}

#[cfg(unix)]
fn term_rows() -> Option<u16> {
    winsize()
        .map(|w| w.ws_row)
        .or_else(|| env::var("LINES").ok().and_then(|s| s.parse().ok()))
}

#[cfg(not(unix))]
fn term_cols() -> Option<u16> {
    winsize_windows()
        .map(|(c, _)| c)
        .or_else(|| env::var("COLUMNS").ok().and_then(|s| s.parse().ok()))
}

#[cfg(not(unix))]
fn term_rows() -> Option<u16> {
    winsize_windows()
        .map(|(_, r)| r)
        .or_else(|| env::var("LINES").ok().and_then(|s| s.parse().ok()))
}

/// Live console size on Windows (ConPTY / node-pty). Prefer this over env so
/// Netcatty resizeSession updates reach mosh-server as UserInstruction::resize.
#[cfg(windows)]
fn winsize_windows() -> Option<(u16, u16)> {
    use std::mem::MaybeUninit;
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        size: Coord,
        cursor_position: Coord,
        attributes: u16,
        window: SmallRect,
        maximum_window_size: Coord,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn GetConsoleScreenBufferInfo(
            console_output: *mut std::ffi::c_void,
            info: *mut ConsoleScreenBufferInfo,
        ) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = 0xFFFFFFF5; // (u32)-11
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == (-1isize as *mut _) {
            return None;
        }
        let mut info = MaybeUninit::<ConsoleScreenBufferInfo>::uninit();
        if GetConsoleScreenBufferInfo(handle, info.as_mut_ptr()) == 0 {
            return None;
        }
        let info = info.assume_init();
        let cols = (info.window.right - info.window.left + 1) as u16;
        let rows = (info.window.bottom - info.window.top + 1) as u16;
        if cols > 0 && rows > 0 {
            Some((cols, rows))
        } else {
            None
        }
    }
}

#[cfg(all(not(unix), not(windows)))]
fn winsize_windows() -> Option<(u16, u16)> {
    None
}

#[cfg(unix)]
fn winsize() -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(ws)
    } else {
        None
    }
}

#[cfg(unix)]
struct RawMode {
    fd: i32,
    original: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    fn enter() -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn enter_raw_mode_if_tty() -> io::Result<Option<RawMode>> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::isatty(io::stdin().as_raw_fd()) } == 1 {
        Ok(RawMode::enter().ok())
    } else {
        Ok(None)
    }
}

#[cfg(any(windows, test))]
const WINDOWS_COOKED_INPUT_FLAGS: u32 = 0x0001 | 0x0002 | 0x0004;
#[cfg(any(windows, test))]
const WINDOWS_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

#[cfg(any(windows, test))]
fn windows_legacy_raw_input_mode(original: u32) -> u32 {
    original & !WINDOWS_COOKED_INPUT_FLAGS
}

#[cfg(any(windows, test))]
fn windows_vt_raw_input_mode(original: u32) -> u32 {
    (original | WINDOWS_VIRTUAL_TERMINAL_INPUT) & !WINDOWS_COOKED_INPUT_FLAGS
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum WindowsRawInputMode {
    VirtualTerminal,
    Legacy,
}

#[cfg(any(windows, test))]
fn set_windows_raw_input_mode(
    original: u32,
    mut set_mode: impl FnMut(u32) -> bool,
) -> Result<WindowsRawInputMode, ()> {
    if set_mode(windows_vt_raw_input_mode(original)) {
        return Ok(WindowsRawInputMode::VirtualTerminal);
    }
    if set_mode(windows_legacy_raw_input_mode(original)) {
        return Ok(WindowsRawInputMode::Legacy);
    }
    Err(())
}

/// Windows console "raw-ish" mode: clear ENABLE_PROCESSED_INPUT so Ctrl+C is
/// not treated solely as a process-control event, and enable VT input so
/// ConPTY preserves escape sequences for arrows and modifier shortcuts.
/// ConPTY/node-pty still often synthesizes CTRL_C_EVENT;
/// `install_signal_flag` ignores that. Restore on drop.
#[cfg(windows)]
struct WindowsConsoleMode {
    handle: *mut std::ffi::c_void,
    original: u32,
}

#[cfg(windows)]
impl WindowsConsoleMode {
    fn enter() -> io::Result<Option<Self>> {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
            fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
            fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
        }
        const STD_INPUT_HANDLE: u32 = 0xFFFFFFF6; // (u32)-10
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() || handle == (-1isize as *mut _) {
                return Ok(None);
            }
            let mut original = 0u32;
            if GetConsoleMode(handle, &mut original) == 0 {
                return Ok(None);
            }
            // Drop cooked-console bits analogous to cfmakeraw / ISIG off.
            // VT input is required under ConPTY so ReadFile receives escape
            // sequences for arrows, Alt combinations, and modified keys. If an
            // older console rejects VT input, preserve the previous raw-mode
            // behavior so Ctrl+C still reaches the remote shell.
            if set_windows_raw_input_mode(original, |mode| SetConsoleMode(handle, mode) != 0)
                .is_err()
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(Self { handle, original }))
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsConsoleMode {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
        }
        unsafe {
            let _ = SetConsoleMode(self.handle, self.original);
        }
    }
}

#[cfg(windows)]
fn enter_raw_mode_if_tty() -> io::Result<Option<WindowsConsoleMode>> {
    WindowsConsoleMode::enter()
}

#[cfg(all(not(unix), not(windows)))]
fn enter_raw_mode_if_tty() -> io::Result<Option<()>> {
    Ok(None)
}

#[cfg(unix)]
fn install_signal_flag(running: Arc<AtomicBool>) {
    unsafe {
        static mut FLAG: *const AtomicBool = std::ptr::null();
        FLAG = Arc::into_raw(running);
        extern "C" fn handler(_: i32) {
            unsafe {
                if !FLAG.is_null() {
                    (*FLAG).store(false, Ordering::SeqCst);
                }
            }
        }
        libc::signal(libc::SIGINT, handler as *const () as usize);
        libc::signal(libc::SIGTERM, handler as *const () as usize);
    }
}

/// Ignore CTRL+C / CTRL+BREAK as process-kill under ConPTY (Netcatty/node-pty).
/// The `\x03` byte is still delivered on stdin and forwarded to mosh-server so
/// the *remote* foreground process is interrupted — matching Unix mosh-client
/// after `cfmakeraw` (ISIG off).
///
/// Close / logoff / shutdown still request a clean client exit via `running`.
#[cfg(windows)]
fn install_signal_flag(running: Arc<AtomicBool>) {
    use std::sync::OnceLock;
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(running);

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        const CTRL_C_EVENT: u32 = 0;
        const CTRL_BREAK_EVENT: u32 = 1;
        const CTRL_CLOSE_EVENT: u32 = 2;
        const CTRL_LOGOFF_EVENT: u32 = 5;
        const CTRL_SHUTDOWN_EVENT: u32 = 6;
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                // Handled: do not terminate. Stdin reader still sees \x03.
                1
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                if let Some(flag) = FLAG.get() {
                    flag.store(false, Ordering::SeqCst);
                }
                1
            }
            _ => 0,
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler_routine: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    // Idempotent enough for a single-process CLI; re-register is harmless.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(all(not(unix), not(windows)))]
fn install_signal_flag(running: Arc<AtomicBool>) {
    std::mem::forget(running);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_vt_raw_mode_preserves_shortcut_escape_sequences() {
        let original = WINDOWS_COOKED_INPUT_FLAGS | 0x0010 | 0x0080;
        let raw = windows_vt_raw_input_mode(original);

        assert_eq!(raw & WINDOWS_COOKED_INPUT_FLAGS, 0);
        assert_ne!(raw & WINDOWS_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(raw & 0x0010, 0);
        assert_ne!(raw & 0x0080, 0);
    }

    #[test]
    fn windows_raw_mode_stops_after_vt_input_succeeds() {
        let original = WINDOWS_COOKED_INPUT_FLAGS | 0x0010;
        let mut attempted = Vec::new();

        let applied = set_windows_raw_input_mode(original, |mode| {
            attempted.push(mode);
            true
        });

        assert_eq!(applied, Ok(WindowsRawInputMode::VirtualTerminal));
        assert_eq!(attempted, vec![windows_vt_raw_input_mode(original)]);
    }

    #[test]
    fn windows_raw_mode_falls_back_when_vt_input_is_rejected() {
        let original = WINDOWS_COOKED_INPUT_FLAGS | 0x0010 | 0x0080;
        let mut attempted = Vec::new();

        let applied = set_windows_raw_input_mode(original, |mode| {
            attempted.push(mode);
            attempted.len() == 2
        });

        assert_eq!(applied, Ok(WindowsRawInputMode::Legacy));
        assert_eq!(
            attempted,
            vec![
                windows_vt_raw_input_mode(original),
                windows_legacy_raw_input_mode(original),
            ]
        );
    }

    #[test]
    fn windows_raw_mode_reports_when_both_attempts_fail() {
        let original = WINDOWS_COOKED_INPUT_FLAGS;
        let mut attempts = 0;

        let applied = set_windows_raw_input_mode(original, |_| {
            attempts += 1;
            false
        });

        assert_eq!(applied, Err(()));
        assert_eq!(attempts, 2);
    }
}
