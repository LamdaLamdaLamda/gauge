//! End-to-end TUI tests: the real `gauge` binary runs inside a pseudo-
//! terminal, is driven with real key bytes, and its screen is read back
//! through a terminal emulator (`vt100`). This covers what the in-memory
//! render tests can't: raw mode, key decoding, the main loop, the network
//! engine behind it, and the files it writes.
//!
//! Every session runs with its own throw-away `HOME` (and XDG dirs), so it
//! never touches the developer's real config, history or log — and can
//! pre-seed a `config.toml` to pick ports and a short duration.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_gauge");
const COLS: u16 = 160;
const ROWS: u16 = 50;

const F5: &[u8] = b"\x1b[15~";
const TAB: &[u8] = b"\t";
const ENTER: &[u8] = b"\r";
const RIGHT: &[u8] = b"\x1b[C";

/// A scratch home directory, removed when dropped.
struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gauge-pty-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempHome(dir)
    }

    /// Where `dirs::config_dir()` points for this HOME on the current OS.
    fn config_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.0.join("Library/Application Support/gauge")
        } else {
            self.0.join(".config/gauge")
        }
    }

    fn data_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.0.join("Library/Application Support/gauge")
        } else {
            self.0.join(".local/share/gauge")
        }
    }

    fn write_config(&self, toml: &str) {
        std::fs::create_dir_all(self.config_dir()).unwrap();
        std::fs::write(self.config_dir().join("config.toml"), toml).unwrap();
    }

    fn apply_env(&self, cmd: &mut CommandBuilder) {
        cmd.env("HOME", &self.0);
        cmd.env("XDG_CONFIG_HOME", self.0.join(".config"));
        cmd.env("XDG_DATA_HOME", self.0.join(".local/share"));
        cmd.env("TERM", "xterm-256color");
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The TUI running in a PTY, with its screen mirrored into a `vt100` parser
/// by a background reader thread.
struct Tui {
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    home: TempHome,
}

impl Tui {
    fn start(home: TempHome) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new(BIN);
        home.apply_env(&mut cmd);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let screen = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let sink = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().unwrap();
        // The master must outlive the session or the child gets SIGHUP.
        std::mem::forget(pair.master);
        Tui { writer, screen, child, home }
    }

    fn text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Sends several keys with a short pause so the app handles each one.
    fn keys(&mut self, keys: &[&[u8]]) {
        for k in keys {
            self.send(k);
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    /// Types literal text (for text fields).
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.send(c.to_string().as_bytes());
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Polls the screen until `needle` shows up; panics with the screen
    /// contents if it doesn't within `timeout`.
    fn wait_for(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.text().contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {needle:?}; screen:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn assert_screen_lacks(&self, needle: &str) {
        assert!(
            !self.text().contains(needle),
            "did not expect {needle:?}; screen:\n{}",
            self.text()
        );
    }

    /// Waits for the process to exit by itself and returns whether it was a
    /// clean (status 0) exit.
    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// A `gauge server` CLI process, killed when dropped.
struct CliServer(Child);

impl CliServer {
    fn start(home: &Path, args: &[&str]) -> Self {
        let child = Command::new(BIN)
            .arg("server")
            .args(args)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(700)); // let it bind
        CliServer(child)
    }
}

impl Drop for CliServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const SHORT: Duration = Duration::from_secs(5);
const TEST_RUN: Duration = Duration::from_secs(25);

// ───────────────────────────── the tests ─────────────────────────────

#[test]
fn starts_on_the_live_screen_and_quits_cleanly_with_q() {
    let mut tui = Tui::start(TempHome::new());
    tui.wait_for("no test running", SHORT);
    tui.wait_for("Live Test", SHORT);

    tui.send(b"q");
    assert!(tui.wait_for_exit(SHORT), "q must end the program with a clean exit status");
}

#[test]
fn number_keys_switch_between_all_four_screens() {
    let mut tui = Tui::start(TempHome::new());
    tui.wait_for("no test running", SHORT);

    tui.send(b"2");
    tui.wait_for("Test Parameters", SHORT);
    tui.send(b"3");
    tui.wait_for("no runs yet", SHORT);
    tui.send(b"4");
    tui.wait_for("Bind Target", SHORT);
    tui.send(b"1");
    tui.wait_for("no test running", SHORT);
}

#[test]
fn config_screen_edits_a_text_field_and_cycles_a_dropdown() {
    let mut tui = Tui::start(TempHome::new());
    tui.wait_for("no test running", SHORT);
    tui.send(b"2");
    tui.wait_for("Test Parameters", SHORT);

    // Host field is focused first: Enter edits, typing replaces, Enter commits.
    // The edit buffer starts out holding the old value ("127.0.0.1"), so
    // clear it with more backspaces than it has characters.
    tui.send(ENTER);
    tui.send(&[0x7f; 16]);
    tui.type_text("10.9.8.7");
    tui.send(ENTER);
    tui.wait_for("--host 10.9.8.7", SHORT);

    // Tab to Port, Tab to Protocol, → cycles TCP -> UDP.
    tui.keys(&[TAB, TAB, RIGHT]);
    tui.wait_for("‹ UDP ›", SHORT);
    tui.wait_for("--udp", SHORT);
    tui.wait_for("Bandwidth (0=∞)", SHORT);
}

#[test]
fn a_full_client_test_runs_through_the_tui_and_lands_in_history() {
    let home = TempHome::new();
    let port = free_port();
    home.write_config(&format!("[client]\nport = {port}\nduration_secs = 5\n"));
    let _server = CliServer::start(&home.0, &["--port", &port.to_string()]);

    let mut tui = Tui::start(home);
    tui.wait_for("no test running", SHORT);
    tui.send(b"2");
    tui.wait_for(&format!("--port {port}"), SHORT);
    tui.send(F5);

    // The test connects, streams data and finishes on its own.
    tui.wait_for("connected", SHORT);
    tui.wait_for("test complete", TEST_RUN);
    tui.assert_screen_lacks("failed");

    // …and is recorded: visible in History, and on disk.
    tui.send(b"3");
    tui.wait_for("TCP bidir ×1", SHORT);
    tui.wait_for("Peak TX", SHORT);
    let history = std::fs::read_to_string(tui.home.data_dir().join("history.json"))
        .expect("history.json must be written");
    assert!(history.contains(&format!("127.0.0.1:{port}")), "{history}");

    // The Event Log is mirrored to the log file.
    let log = std::fs::read_to_string(tui.home.data_dir().join("gauge.log"))
        .expect("gauge.log must be written");
    assert!(log.contains("[live]") && log.contains("test complete"), "{log}");
}

#[test]
fn an_unreachable_server_shows_an_error_instead_of_hanging() {
    let home = TempHome::new();
    let port = free_port(); // nothing listens here
    home.write_config(&format!("[client]\nport = {port}\nduration_secs = 5\n"));

    let mut tui = Tui::start(home);
    tui.wait_for("no test running", SHORT);
    tui.send(b"2");
    tui.wait_for("Test Parameters", SHORT);
    tui.send(F5);
    tui.wait_for("failed", SHORT);
    tui.wait_for("error", SHORT);
}

#[test]
fn server_mode_listens_accepts_a_cli_client_and_stops() {
    let home = TempHome::new();
    let port = free_port();
    home.write_config(&format!("[server]\nport = {port}\n"));

    let mut tui = Tui::start(home);
    tui.wait_for("no test running", SHORT);
    tui.send(b"4");
    tui.wait_for(&format!("--port {port}"), SHORT);
    tui.send(F5);
    tui.wait_for("listening", SHORT);

    // A real client connects from outside the TUI.
    let status = Command::new(BIN)
        .args(["client", "--port", &port.to_string(), "-t", "2"])
        .env("HOME", &tui.home.0)
        .env("XDG_CONFIG_HOME", tui.home.0.join(".config"))
        .env("XDG_DATA_HOME", tui.home.0.join(".local/share"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    tui.wait_for("connected — stream 1/1", SHORT);

    tui.send(b"s");
    tui.wait_for("server stopped", SHORT);
}

#[test]
fn ctrl_s_on_the_config_screen_writes_a_real_config_file() {
    let home = TempHome::new();
    let config_path = home.config_dir().join("config.toml");
    let mut tui = Tui::start(home);
    tui.wait_for("no test running", SHORT);
    tui.send(b"2");
    tui.wait_for("Test Parameters", SHORT);

    tui.send(b"\x13"); // Ctrl+S
    tui.wait_for("config saved to", SHORT);
    let saved = std::fs::read_to_string(&config_path).expect("Ctrl+S must create config.toml");
    assert!(saved.contains("[client]") && saved.contains("host = \"127.0.0.1\""), "{saved}");
}
