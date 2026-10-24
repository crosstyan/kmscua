//! Against a private Xvfb per test (`-displayfd`, never the user's display):
//! capture, XTEST motion, focus on click, typing into a real X client (a
//! window this test creates and reads events from), Unicode typing through
//! spare keycodes. Skipped when Xvfb is not installed.

use std::process::Child;
use std::time::{Duration, Instant};

use kmscua_mcp::Backend;
use kmscua_proto::{Button, CursorInfo, ImageFormat, Rect, Request, ScreenshotInfo, Status};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use xcua::XBackend;

struct Xvfb {
    child: Child,
    display: String,
    dir: std::path::PathBuf,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn xvfb(tag: &str) -> Option<Xvfb> {
    let have = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("Xvfb").is_file()))
        .unwrap_or(false);
    if !have {
        eprintln!("Xvfb not installed; skipping");
        return None;
    }
    let (child, display) = xcua::xvfb::spawn((640, 480), None).expect("start Xvfb");
    let dir = std::env::temp_dir().join(format!("xcua-test-{tag}-{}", std::process::id()));
    Some(Xvfb { child, display, dir })
}

fn backend(x: &Xvfb, shm: bool) -> XBackend {
    XBackend::open(&x.display, shm, &x.dir.join("recordings")).expect("open backend")
}

fn shot(b: &XBackend, max_side: Option<u32>, region: Option<Rect>) -> (ScreenshotInfo, image::RgbaImage) {
    let r = b
        .call(&Request::Screenshot { max_side, format: ImageFormat::Png, quality: None, cursor: Some(false), region })
        .unwrap();
    let info: ScreenshotInfo = r.data().unwrap();
    (info, image::load_from_memory(&r.payload).unwrap().into_rgba8())
}

/// A mapped 200x150 red window at (100, 100) that reports keys, buttons
/// and focus, on its own client connection.
fn red_window(display: &str) -> (RustConnection, u32) {
    let (conn, screen) = RustConnection::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().unwrap();
    let mask = EventMask::KEY_PRESS | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE;
    conn.create_window(
        0,
        win,
        root,
        100,
        100,
        200,
        150,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(0xff0000).event_mask(mask),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    let ok = wait_events(&conn, Duration::from_secs(5), |e| matches!(e, Event::MapNotify(_)));
    assert_eq!(ok.len(), 1, "window never mapped");
    (conn, win)
}

/// Events matching `want` until the deadline (or the first one, when
/// `want` is for MapNotify).
fn wait_events(conn: &RustConnection, timeout: Duration, want: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    while Instant::now() < deadline {
        while let Some(e) = conn.poll_for_event().unwrap() {
            if want(&e) {
                let map = matches!(e, Event::MapNotify(_));
                got.push(e);
                if map {
                    return got;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    got
}

#[test]
fn screenshot_dims_and_status() {
    let Some(x) = xvfb("dims") else { return };
    let b = backend(&x, false);
    let (info, img) = shot(&b, None, None);
    assert_eq!((info.width, info.height, info.scale), (640, 480, 1.0));
    assert_eq!(img.dimensions(), (640, 480));
    let (info, img) = shot(&b, Some(320), None);
    assert_eq!((info.width, info.height), (320, 240));
    assert_eq!((info.source_width, info.source_height, info.scale), (640, 480, 2.0));
    assert_eq!(img.dimensions(), (320, 240));
    let (info, _) = shot(&b, None, Some(Rect { x: 600, y: 470, width: 100, height: 100 }));
    assert_eq!((info.width, info.height, info.origin_x, info.origin_y), (40, 10, 600, 470));

    let s: Status = b.call(&Request::Status).unwrap().data().unwrap();
    assert_eq!((s.desktop.width, s.desktop.height), (640, 480));
    assert!(s.gpu_driver.is_none());
    assert_eq!(s.record_codec.as_deref(), Some("jpeg-stills"));
    assert!(!s.displays.is_empty());
    b.call(&Request::Ping).unwrap();
    b.call(&Request::Wake).unwrap();
    // Recording is the recorder's job; whatever it answers must be Err or a
    // real RecordInfo, and RecordStatus always answers.
    b.call(&Request::RecordStatus).unwrap();
}

#[test]
fn pixels_match_with_and_without_shm() {
    let Some(x) = xvfb("pixels") else { return };
    let (_conn, _win) = red_window(&x.display);
    for shm in [false, true] {
        let b = backend(&x, shm);
        let (_, img) = shot(&b, None, None);
        assert_eq!(img.get_pixel(200, 175).0, [255, 0, 0, 255], "shm={shm}");
        assert_eq!(img.get_pixel(20, 20).0, [0, 0, 0, 255], "shm={shm}");
        let (_, img) = shot(&b, None, Some(Rect { x: 150, y: 150, width: 10, height: 10 }));
        assert_eq!(img.get_pixel(5, 5).0, [255, 0, 0, 255], "shm={shm} region");
        assert_eq!(b.capture().lock().unwrap().shm_active(), shm, "SHM path taken");
    }
}

#[test]
fn motion_lands_on_the_pixel() {
    let Some(x) = xvfb("motion") else { return };
    let b = backend(&x, false);
    let (conn, screen) = RustConnection::connect(Some(&x.display)).unwrap();
    let root = conn.setup().roots[screen].root;
    for (px, py) in [(300, 200), (0, 0), (639, 479)] {
        b.call(&Request::PointerMove { x: px, y: py }).unwrap();
        let p = conn.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!((p.root_x as i32, p.root_y as i32), (px, py));
        let c: CursorInfo = b.call(&Request::Cursor).unwrap().data().unwrap();
        assert_eq!((c.x + c.hot_x, c.y + c.hot_y), (px, py));
    }
    // Clamped to the screen like cuad's ABS range.
    b.call(&Request::PointerMove { x: 5000, y: -3 }).unwrap();
    let p = conn.query_pointer(root).unwrap().reply().unwrap();
    assert_eq!((p.root_x, p.root_y), (639, 0));
}

#[test]
fn keysym_map_comes_from_the_server() {
    let Some(x) = xvfb("keymap") else { return };
    let i = xcua::input::Input::open(&x.display).unwrap();
    let map = i.keymap().unwrap();
    let (a, shift) = map.lookup(0x61).expect("a on the map");
    assert!(!shift);
    assert_eq!(map.lookup(0x41), Some((a, true)), "A is shift+a");
    assert!(map.lookup(xcua::keysym::SHIFT_L).is_some());
    assert!(map.lookup(0x0100_94c3).is_none(), "铃 is not on a US map");
    assert!(!map.spare().is_empty(), "Xvfb leaves keycodes free");
}

#[test]
fn click_focuses_and_typing_reaches_the_window() {
    let Some(x) = xvfb("type") else { return };
    let b = backend(&x, false);
    let (conn, win) = red_window(&x.display);
    b.call(&Request::Click { x: 200, y: 175, button: Button::Left, count: 1, modifiers: vec![] }).unwrap();
    let focus = conn.get_input_focus().unwrap().reply().unwrap().focus;
    assert_eq!(focus, win, "click did not focus the window under the pointer");
    let presses = wait_events(&conn, Duration::from_millis(500), |e| matches!(e, Event::ButtonPress(_)));
    assert_eq!(presses.len(), 1);

    // ASCII with a shifted character: keycodes as the server maps them.
    let map = xcua::input::Input::open(&x.display).unwrap().keymap().unwrap();
    b.call(&Request::Type { text: "hI".into(), delay_ms: None }).unwrap();
    let keys: Vec<u8> = wait_events(&conn, Duration::from_millis(500), |e| matches!(e, Event::KeyPress(_)))
        .into_iter()
        .filter_map(|e| if let Event::KeyPress(k) = e { Some(k.detail) } else { None })
        .collect();
    let h = map.lookup(0x68).unwrap().0;
    let i = map.lookup(0x69).unwrap().0;
    let shift = map.lookup(xcua::keysym::SHIFT_L).unwrap().0;
    assert_eq!(keys, vec![h, shift, i]);

    // Unicode through the lib seam: one Type, spare keycodes, map restored.
    let mapping = |c: &RustConnection| {
        let s = c.setup();
        c.get_keyboard_mapping(s.min_keycode, s.max_keycode - s.min_keycode + 1).unwrap().reply().unwrap().keysyms
    };
    let map_before = mapping(&conn);
    kmscua_mcp::type_text(&b, "é铃").unwrap();
    let uni = wait_events(&conn, Duration::from_millis(500), |e| matches!(e, Event::KeyPress(_)));
    assert_eq!(uni.len(), 2, "two key presses for two characters");
    assert_eq!(mapping(&conn), map_before, "spare keycodes were not unbound");

    // Chords and wheel leave nothing held.
    b.call(&Request::KeyCombo { keys: vec!["ctrl+shift+t".into()] }).unwrap();
    b.call(&Request::Scroll { x: 200, y: 175, dx: 0, dy: 2, modifiers: vec!["shift".into()] }).unwrap();
    let wheel: Vec<u8> = wait_events(&conn, Duration::from_millis(500), |e| matches!(e, Event::ButtonPress(_)))
        .into_iter()
        .filter_map(|e| if let Event::ButtonPress(p) = e { Some(p.detail) } else { None })
        .collect();
    assert_eq!(wheel, vec![5, 5]);
    let held = conn.query_keymap().unwrap().reply().unwrap().keys;
    assert!(held.iter().all(|&b| b == 0), "keys left down: {held:?}");

    let focused = b.focused().unwrap().unwrap();
    assert!(focused.contains(&format!("{win:#x}")), "{focused}");
}

#[test]
fn unknown_key_is_an_error_not_ok() {
    let Some(x) = xvfb("err") else { return };
    let b = backend(&x, false);
    assert!(b.call(&Request::KeyCombo { keys: vec!["ctrl+nosuchkey".into()] }).is_err());
    assert!(b.call(&Request::Type { text: "a\u{7}".into(), delay_ms: None }).is_err());
    assert!(b
        .call(&Request::Screenshot { max_side: None, format: ImageFormat::Png, quality: None, cursor: None, region: Some(Rect { x: 9000, y: 0, width: 5, height: 5 }) })
        .is_err());
}

/// The xcua binary against a state dir; `xcua down` on drop, so a failed
/// assertion does not leak the Xvfb it started.
struct Cli {
    state: std::path::PathBuf,
    display_env: Option<String>,
    /// Run every command under this (bwrap …), except the cleanup `down`.
    wrap: Vec<String>,
}

impl Cli {
    fn new(state: std::path::PathBuf, display_env: Option<&str>) -> Self {
        Self { state, display_env: display_env.map(str::to_string), wrap: Vec::new() }
    }

    fn cmd(&self) -> std::process::Command {
        let mut c = match self.wrap.split_first() {
            Some((w, rest)) => {
                let mut c = std::process::Command::new(w);
                c.args(rest).arg(env!("CARGO_BIN_EXE_xcua"));
                c
            }
            None => std::process::Command::new(env!("CARGO_BIN_EXE_xcua")),
        };
        c.arg("--state-dir").arg(&self.state);
        match &self.display_env {
            Some(d) => c.env("DISPLAY", d),
            None => c.env_remove("DISPLAY"),
        };
        c
    }

    /// Run to completion; (success, stdout + stderr).
    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = self.cmd().args(args).output().expect("run xcua");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args);
        assert!(ok, "xcua {args:?}: {text}");
        text
    }

    fn env_display(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.state.join("env")).ok()?;
        text.trim().strip_prefix("export DISPLAY=").map(str::to_string)
    }

    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.state.join("xvfb.pid")).ok()?.split_whitespace().next()?.parse().ok()
    }

    fn mcp(&self, args: &[&str]) -> Mcp {
        let mut child = self
            .cmd()
            .args(args)
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start xcua mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp { child, stdin: Some(stdin), stdout, id: 0 };
        m.rpc("initialize", serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
        m.notify("notifications/initialized");
        m
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // With /proc, so a test that failed under bwrap still cleans up.
        self.wrap.clear();
        if let Some(pid) = self.pid() {
            // A test may have left it stopped.
            unsafe { libc::kill(pid, libc::SIGCONT) };
        }
        let _ = self.cmd().arg("down").output();
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// Alive and not a zombie (an Xvfb the mcp started is its child, and stays
/// a zombie until the mcp reaps it).
fn alive(pid: i32) -> bool {
    let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rfind(')').map(|i| s[i + 1..].trim_start().starts_with('Z')))
        .unwrap_or(false);
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists && !zombie
}

fn wait_until(timeout: Duration, f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// A JSON-RPC session with `xcua mcp`; killed on drop.
struct Mcp {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    id: u64,
}

impl Mcp {
    fn notify(&mut self, method: &str) {
        use std::io::Write;
        let m = serde_json::json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin.as_mut().unwrap(), "{m}").unwrap();
    }

    fn rpc(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::{BufRead, Write};
        self.id += 1;
        let m = serde_json::json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.stdin.as_mut().unwrap(), "{m}").unwrap();
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "xcua mcp hung up");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["id"] == self.id {
                return v;
            }
        }
    }

    /// A tool call's text, or "ERR …".
    fn tool(&mut self, name: &str, args: serde_json::Value) -> String {
        let r = self.rpc("tools/call", serde_json::json!({"name": name, "arguments": args}));
        if let Some(e) = r.get("error") {
            return format!("ERR {e}");
        }
        r["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().map(str::to_string).unwrap_or_else(|| format!("<{}>", c["type"].as_str().unwrap_or("?"))))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Close stdin and wait for a clean exit.
    fn finish(mut self) {
        drop(self.stdin.take());
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let Some(st) = self.child.try_wait().unwrap() {
                assert!(st.success(), "xcua mcp exited with {st}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("xcua mcp did not exit after stdin closed");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn have_xvfb() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("Xvfb").is_file()))
        .unwrap_or(false)
}

/// `$DISPLAY` in the environment is never used unless `--display env`
/// asks for it, `--display` on one-shot commands leaves the state alone,
/// and `up --display` will not hide xcua's own Xvfb.
#[test]
fn display_env_is_never_implicit() {
    let Some(decoy) = xvfb("policy") else { return };
    let cli = Cli::new(decoy.dir.join("state"), Some(&decoy.display));
    let up = cli.ok(&["--size", "320x240", "up"]);
    assert!(up.contains("started Xvfb"), "{up}");
    let own = cli.env_display().unwrap();
    assert_ne!(own, decoy.display, "attached to $DISPLAY");
    let shot = decoy.dir.join("own.png");
    cli.ok(&["screenshot", "-o", shot.to_str().unwrap()]);
    assert_eq!(image::image_dimensions(&shot).unwrap(), (320, 240), "screenshot came from the decoy");

    // One-shot --display: used for the run, state untouched.
    cli.ok(&["--display", "env", "screenshot", "-o", shot.to_str().unwrap()]);
    assert_eq!(image::image_dimensions(&shot).unwrap(), (640, 480));
    cli.ok(&["--display", "env", "doctor"]);
    assert_eq!(cli.env_display().as_deref(), Some(own.as_str()), "a one-shot --display rewrote the env file");
    assert!(cli.pid().is_some_and(alive), "a one-shot --display orphaned xcua's Xvfb");
    // up --display refuses while xcua's own Xvfb runs.
    let (ok, text) = cli.run(&["--display", &decoy.display, "up"]);
    assert!(!ok && text.contains("xcua down"), "{text}");

    let down = cli.ok(&["down"]);
    assert!(down.contains("stopped Xvfb"), "{down}");
    let up = cli.ok(&["--display", &decoy.display, "up"]);
    assert!(up.contains("attached to"), "{up}");
    let down = cli.ok(&["down"]);
    assert!(down.contains("left running"), "{down}");
    assert!(xcua::xvfb::answers(&decoy.display));
}

/// The entry point that once panicked: `xcua mcp` with `$DISPLAY` set
/// starts its own Xvfb, serves clicks and screenshots on it, stops it on
/// exit; with `--display` it attaches and leaves the state dir alone.
#[test]
fn mcp_with_display_set_starts_its_own() {
    let Some(decoy) = xvfb("mcpenv") else { return };
    let cli = Cli::new(decoy.dir.join("state"), Some(&decoy.display));
    let mut m = cli.mcp(&["--size", "320x240"]);
    let own = cli.env_display().unwrap();
    assert_ne!(own, decoy.display);
    assert_eq!(m.tool("left_click", serde_json::json!({"coordinate": [100, 100]})), "OK");
    assert_eq!(m.tool("screenshot", serde_json::json!({})), "<image>");
    let doctor = m.tool("doctor", serde_json::json!({}));
    assert!(doctor.contains("320x240") && doctor.contains(&format!("\"{own}\"")), "{doctor}");
    let pid = cli.pid().unwrap();
    m.finish();
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)), "mcp's Xvfb outlived it");
    assert!(cli.env_display().is_none());

    let mut m = cli.mcp(&["--display", &decoy.display]);
    let doctor = m.tool("doctor", serde_json::json!({}));
    assert!(doctor.contains("640x480"), "{doctor}");
    assert!(cli.env_display().is_none(), "mcp --display wrote the state dir");
    m.finish();
    assert!(xcua::xvfb::answers(&decoy.display));
}

/// Four `xcua up`s at once start one Xvfb between them.
#[test]
fn concurrent_up_starts_one_xvfb() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-race-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    let kids: Vec<_> = (0..4)
        .map(|_| cli.cmd().args(["--size", "320x240", "up"]).stdout(std::process::Stdio::piped()).spawn().unwrap())
        .collect();
    let outs: Vec<String> = kids
        .into_iter()
        .map(|k| String::from_utf8_lossy(&k.wait_with_output().unwrap().stdout).into_owned())
        .collect();
    let started = outs.iter().filter(|o| o.contains("started Xvfb")).count();
    assert_eq!(started, 1, "{outs:#?}");
    let display = cli.env_display().unwrap();
    assert!(outs.iter().all(|o| o.contains(&format!("DISPLAY={display} "))), "{outs:#?}");
    let leftovers: Vec<_> = std::fs::read_dir(&cli.state)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two `xcua mcp`s share xcua's Xvfb: the first leaving does not stop it
/// under the second, the last one leaving does. A SIGKILLed starter's Xvfb
/// is adopted by the next mcp; a vanished Xvfb is started again.
#[test]
fn mcp_users_share_adopt_and_reconnect() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-users-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    let mut a = cli.mcp(&["--size", "320x240"]);
    let pid = cli.pid().unwrap();
    let mut b = cli.mcp(&[]);
    assert_eq!(cli.pid(), Some(pid), "the second mcp started another Xvfb");
    assert!(cli.ok(&["status"]).contains("mcp users: ["), "{}", cli.ok(&["status"]));
    assert_eq!(a.tool("screenshot", serde_json::json!({})), "<image>");
    a.finish();
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "first mcp stopped the Xvfb its peer uses");
    assert_eq!(b.tool("screenshot", serde_json::json!({})), "<image>");

    // B's starter (A) is gone; SIGKILL B as well: the Xvfb is orphaned.
    drop(b);
    assert!(alive(pid));
    // C adopts it, and stops it when leaving.
    let mut c = cli.mcp(&[]);
    assert_eq!(cli.pid(), Some(pid));
    let status = cli.ok(&["status"]);
    assert!(status.contains("owner: mcp "), "{status}");

    // Kill the Xvfb under C: a read reconnects to a new one and answers,
    // the next input request is told the display restarted, then input
    // works on the new display.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    assert_eq!(c.tool("screenshot", serde_json::json!({})), "<image>", "no reconnect after the Xvfb died");
    let new_pid = cli.pid().unwrap();
    assert_ne!(new_pid, pid);
    let click = c.tool("left_click", serde_json::json!({"coordinate": [10, 10]}));
    assert!(click.contains("restarted"), "{click}");
    assert_eq!(c.tool("left_click", serde_json::json!({"coordinate": [10, 10]})), "OK");
    // Input first after a restart: refused, not replayed onto the new display.
    unsafe { libc::kill(new_pid, libc::SIGKILL) };
    assert!(wait_until(Duration::from_secs(5), || !alive(new_pid)));
    let typed = c.tool("type", serde_json::json!({"text": "rm -rf"}));
    assert!(typed.contains("restarted") && typed.contains("not sent"), "{typed}");
    let new_pid = cli.pid().unwrap();
    c.finish();
    assert!(wait_until(Duration::from_secs(5), || !alive(new_pid)), "last mcp did not stop the Xvfb");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed request releases what it pressed, not a button an earlier
/// `left_mouse_down` holds.
#[test]
fn failed_request_keeps_an_earlier_held_button() {
    let Some(x) = xvfb("held") else { return };
    let b = backend(&x, false);
    let (conn, screen) = RustConnection::connect(Some(&x.display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let button1 = |c: &RustConnection| u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100 != 0;
    b.call(&Request::PointerMove { x: 50, y: 50 }).unwrap();
    b.call(&Request::PointerButton { button: Button::Left, down: true }).unwrap();
    assert!(button1(&conn));
    assert!(b.call(&Request::KeyCombo { keys: vec!["ctrl+nosuchkey".into()] }).is_err());
    assert!(b.call(&Request::Type { text: "ab\u{7}".into(), delay_ms: None }).is_err());
    assert!(button1(&conn), "a failed request released the held button");
    b.call(&Request::PointerButton { button: Button::Left, down: false }).unwrap();
    assert!(!button1(&conn));

    // The mechanism: what the request pressed goes, the rest stays.
    let mut i = xcua::input::Input::open(&x.display).unwrap();
    i.button(Button::Left, true).unwrap();
    let before = i.held();
    i.key(30, true).unwrap(); // evdev KEY_A
    i.release_since(&before);
    let keys = conn.query_keymap().unwrap().reply().unwrap().keys;
    assert!(keys.iter().all(|&k| k == 0), "key left down: {keys:?}");
    assert!(button1(&conn));
    i.button(Button::Left, false).unwrap();
}

/// Shutdown cuts a long hold_key short and leaves nothing down.
#[test]
fn shutdown_cancels_a_hold() {
    let Some(x) = xvfb("hold") else { return };
    let b = std::sync::Arc::new(backend(&x, false));
    let b2 = b.clone();
    let t0 = Instant::now();
    let hold = std::thread::spawn(move || b2.call(&Request::KeyHold { keys: vec!["shift".into()], seconds: 30.0 }));
    std::thread::sleep(Duration::from_millis(300));
    b.shutdown(Duration::from_secs(2));
    let r = hold.join().unwrap();
    assert!(r.is_err(), "hold was not cancelled");
    assert!(t0.elapsed() < Duration::from_secs(3), "shutdown waited {:?}", t0.elapsed());
    let (conn, _) = RustConnection::connect(Some(&x.display)).unwrap();
    let keys = conn.query_keymap().unwrap().reply().unwrap().keys;
    assert!(keys.iter().all(|&k| k == 0), "keys left down: {keys:?}");
}

/// Clicking a menu-like override-redirect window does not take the focus.
#[test]
fn click_does_not_focus_override_redirect() {
    let Some(x) = xvfb("override") else { return };
    let b = backend(&x, false);
    let (conn, win) = red_window(&x.display);
    b.call(&Request::Click { x: 200, y: 175, button: Button::Left, count: 1, modifiers: vec![] }).unwrap();
    assert_eq!(conn.get_input_focus().unwrap().reply().unwrap().focus, win);
    let root = conn.setup().roots[0].root;
    let menu = conn.generate_id().unwrap();
    conn.create_window(
        0,
        menu,
        root,
        400,
        300,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(0x00ff00).override_redirect(1),
    )
    .unwrap();
    conn.map_window(menu).unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
    b.call(&Request::Click { x: 450, y: 350, button: Button::Left, count: 1, modifiers: vec![] }).unwrap();
    assert_eq!(conn.get_input_focus().unwrap().reply().unwrap().focus, win, "focus went to the override-redirect window");
}

/// Without /proc (bwrap --tmpfs /proc) xcua still knows its own Xvfb by the
/// nonce on its root window: three `up`s start one, `down` stops it.
#[test]
fn no_proc_recognises_its_xvfb_by_nonce() {
    let have_bwrap = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bwrap").is_file()))
        .unwrap_or(false);
    if !have_xvfb() || !have_bwrap {
        eprintln!("bwrap or Xvfb missing; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-noproc-{}", std::process::id()));
    let mut cli = Cli::new(dir.join("state"), None);
    cli.wrap = ["bwrap", "--dev-bind", "/", "/", "--tmpfs", "/proc"].map(String::from).to_vec();
    let (ok, probe) = cli.run(&["status"]);
    assert!(ok, "xcua under bwrap: {probe}");
    let first = cli.ok(&["--size", "320x240", "up"]);
    assert!(first.contains("started Xvfb"), "{first}");
    let pid = cli.pid().unwrap();
    for _ in 0..2 {
        let again = cli.ok(&["up"]);
        assert!(again.contains("already up"), "{again}");
    }
    assert_eq!(cli.pid(), Some(pid));
    let shot = dir.join("s.png");
    cli.ok(&["screenshot", "-o", shot.to_str().unwrap()]);
    let display = cli.env_display().unwrap();
    let down = cli.ok(&["down"]);
    assert!(down.contains("stopped Xvfb"), "{down}");
    assert!(!xcua::xvfb::answers(&display));
    assert!(wait_until(Duration::from_secs(3), || !alive(pid)));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stopped (SIGSTOP) Xvfb cannot hang `up` or `down`: probes time out,
/// and with /proc the hung Xvfb is killed.
#[test]
fn stopped_xvfb_does_not_hang_up_or_down() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-stop-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    cli.ok(&["--size", "320x240", "up"]);
    let pid = cli.pid().unwrap();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let t0 = Instant::now();
    let up = cli.ok(&["up"]);
    assert!(t0.elapsed() < Duration::from_secs(10), "up took {:?}", t0.elapsed());
    assert!(up.contains("started Xvfb"), "{up}");
    assert!(wait_until(Duration::from_secs(3), || !alive(pid)), "hung Xvfb left behind");
    let pid = cli.pid().unwrap();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let t0 = Instant::now();
    let down = cli.ok(&["down"]);
    assert!(t0.elapsed() < Duration::from_secs(8), "down took {:?}", t0.elapsed());
    assert!(down.contains("stopped Xvfb"), "{down}");
    assert!(wait_until(Duration::from_secs(3), || !alive(pid)));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed non-input request does not wait behind a running hold_key.
#[test]
fn failed_read_does_not_wait_for_input() {
    let Some(x) = xvfb("busy") else { return };
    let b = std::sync::Arc::new(backend(&x, false));
    let b2 = b.clone();
    let hold = std::thread::spawn(move || b2.call(&Request::KeyHold { keys: vec!["shift".into()], seconds: 3.0 }));
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    assert!(b.call(&Request::RecordStop).is_err(), "record_stop without a recording");
    assert!(t0.elapsed() < Duration::from_millis(1000), "waited {:?} behind the hold", t0.elapsed());
    hold.join().unwrap().unwrap();
}
