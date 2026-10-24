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
    app_window(display, (100, 100, 200, 150), 0xff0000, None)
}

/// A mapped window at `(x, y, w, h)` filled with `color`, with
/// `WM_CLASS`/title `class` when given, reporting keys, buttons and focus.
fn app_window(display: &str, (x, y, w, h): (i16, i16, u16, u16), color: u32, class: Option<&str>) -> (RustConnection, u32) {
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, screen) = RustConnection::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().unwrap();
    let mask = EventMask::KEY_PRESS | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE;
    conn.create_window(
        0,
        win,
        root,
        x,
        y,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(color).event_mask(mask),
    )
    .unwrap();
    if let Some(c) = class {
        let wm_class = format!("{c}\0{}\0", c.to_uppercase());
        conn.change_property8(x11rb::protocol::xproto::PropMode::REPLACE, win, x11rb::protocol::xproto::AtomEnum::WM_CLASS, x11rb::protocol::xproto::AtomEnum::STRING, wm_class.as_bytes())
            .unwrap();
        conn.change_property8(x11rb::protocol::xproto::PropMode::REPLACE, win, x11rb::protocol::xproto::AtomEnum::WM_NAME, x11rb::protocol::xproto::AtomEnum::STRING, format!("{c} window").as_bytes())
            .unwrap();
    }
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    let ok = wait_events(&conn, 1, Duration::from_secs(10), |e| matches!(e, Event::MapNotify(_)));
    assert_eq!(ok.len(), 1, "window never mapped");
    (conn, win)
}

/// Events matching `want` until `n` have arrived or the deadline passes.
/// Deadlines are generous (a loaded machine delays delivery) and cost
/// nothing when the events come.
fn wait_events(conn: &RustConnection, n: usize, timeout: Duration, want: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    while Instant::now() < deadline {
        while let Some(e) = conn.poll_for_event().unwrap() {
            if want(&e) {
                got.push(e);
            }
        }
        if got.len() >= n {
            // A moment more, so an extra (wrong) event is seen too.
            std::thread::sleep(Duration::from_millis(100));
            while let Some(e) = conn.poll_for_event().unwrap() {
                if want(&e) {
                    got.push(e);
                }
            }
            return got;
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
    // Upstream's multi-display fields: one Xvfb is one display, placed 1:1.
    assert_eq!(s.displays[0].desktop, Some(Rect { x: 0, y: 0, width: 640, height: 480 }));
    assert_eq!(s.displays[0].scale, 1.0);
    assert!(!s.build_id.is_empty() && s.build.contains(&s.build_id), "{} / {}", s.build_id, s.build);
    let e = b.call(&Request::SetLayout { monitors: vec![] }).unwrap_err();
    assert!(format!("{e:#}").contains("not supported on X11"), "{e:#}");
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
    let presses = wait_events(&conn, 1, Duration::from_secs(5), |e| matches!(e, Event::ButtonPress(_)));
    assert_eq!(presses.len(), 1);

    // ASCII with a shifted character: keycodes as the server maps them.
    let map = xcua::input::Input::open(&x.display).unwrap().keymap().unwrap();
    b.call(&Request::Type { text: "hI".into(), delay_ms: None }).unwrap();
    let keys: Vec<u8> = wait_events(&conn, 3, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_)))
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
    let uni = wait_events(&conn, 2, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_)));
    assert_eq!(uni.len(), 2, "two key presses for two characters");
    assert_eq!(mapping(&conn), map_before, "spare keycodes were not unbound");

    // Chords and wheel leave nothing held.
    b.call(&Request::KeyCombo { keys: vec!["ctrl+shift+t".into()] }).unwrap();
    b.call(&Request::Scroll { x: 200, y: 175, dx: 0, dy: 2, modifiers: vec!["shift".into()] }).unwrap();
    let wheel: Vec<u8> = wait_events(&conn, 2, Duration::from_secs(5), |e| matches!(e, Event::ButtonPress(_)))
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
    /// Extra environment for every command except the cleanup `down`.
    env: Vec<(String, String)>,
}

impl Cli {
    fn new(state: std::path::PathBuf, display_env: Option<&str>) -> Self {
        Self { state, display_env: display_env.map(str::to_string), wrap: Vec::new(), env: Vec::new() }
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
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
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
        // With /proc and the usual PATH, so a test that failed under bwrap
        // or with a narrowed PATH still cleans up.
        self.wrap.clear();
        self.env.clear();
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
    // Upstream's switch_display works on the X screen as one placed display.
    assert_eq!(m.tool("switch_display", serde_json::json!({"display": "primary"})), "<image>");
    assert_eq!(m.tool("switch_display", serde_json::json!({"display": "all"})), "<image>");
    let none = m.tool("switch_display", serde_json::json!({"display": "HDMI-9"}));
    assert!(none.starts_with("ERR") && none.contains("available"), "{none}");
    let pid = cli.pid().unwrap();
    m.finish();
    assert!(wait_until(Duration::from_secs(15), || !alive(pid)), "mcp's Xvfb outlived it");
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
    std::thread::sleep(Duration::from_millis(500));
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
    assert!(wait_until(Duration::from_secs(15), || !alive(pid)));
    assert_eq!(c.tool("screenshot", serde_json::json!({})), "<image>", "no reconnect after the Xvfb died");
    let new_pid = cli.pid().unwrap();
    assert_ne!(new_pid, pid);
    let click = c.tool("left_click", serde_json::json!({"coordinate": [10, 10]}));
    assert!(click.contains("restarted"), "{click}");
    assert_eq!(c.tool("left_click", serde_json::json!({"coordinate": [10, 10]})), "OK");
    // Input first after a restart: refused, not replayed onto the new display.
    unsafe { libc::kill(new_pid, libc::SIGKILL) };
    assert!(wait_until(Duration::from_secs(15), || !alive(new_pid)));
    let typed = c.tool("type", serde_json::json!({"text": "rm -rf"}));
    assert!(typed.contains("restarted") && typed.contains("not sent"), "{typed}");
    let new_pid = cli.pid().unwrap();
    c.finish();
    assert!(wait_until(Duration::from_secs(15), || !alive(new_pid)), "last mcp did not stop the Xvfb");
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
    assert!(t0.elapsed() < Duration::from_secs(10), "shutdown waited {:?} (the hold was 30 s)", t0.elapsed());
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
    assert!(wait_until(Duration::from_secs(15), || !alive(pid)));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stopped (SIGSTOP) Xvfb cannot hang `up` or `down`: probes time out,
/// and once it stays silent for the whole patience, /proc shows it stopped
/// and it is replaced.
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
    assert!(t0.elapsed() < Duration::from_secs(25), "up took {:?}", t0.elapsed());
    assert!(t0.elapsed() >= Duration::from_secs(9), "replaced after only {:?}", t0.elapsed());
    assert!(up.contains("started Xvfb"), "{up}");
    assert!(wait_until(Duration::from_secs(15), || !alive(pid)), "hung Xvfb left behind");
    let pid = cli.pid().unwrap();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let t0 = Instant::now();
    let down = cli.ok(&["down"]);
    assert!(t0.elapsed() < Duration::from_secs(8), "down took {:?}", t0.elapsed());
    assert!(down.contains("stopped Xvfb"), "{down}");
    assert!(wait_until(Duration::from_secs(15), || !alive(pid)));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed non-input request does not wait behind a running hold_key.
#[test]
fn failed_read_does_not_wait_for_input() {
    let Some(x) = xvfb("busy") else { return };
    let b = std::sync::Arc::new(backend(&x, false));
    let b2 = b.clone();
    let hold = std::thread::spawn(move || b2.call(&Request::KeyHold { keys: vec!["shift".into()], seconds: 8.0 }));
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    assert!(b.call(&Request::RecordStop).is_err(), "record_stop without a recording");
    // Waiting behind the hold would take about 7.7 s.
    assert!(t0.elapsed() < Duration::from_secs(4), "waited {:?} behind the hold", t0.elapsed());
    hold.join().unwrap().unwrap();
}

/// A slow moment is not a dead server: `up` keeps probing, and an Xvfb that
/// answers again within the patience is reused, not killed.
#[test]
fn slow_xvfb_is_not_killed() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-slow-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    cli.ok(&["--size", "320x240", "up"]);
    let pid = cli.pid().unwrap();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let up = cli.cmd().arg("up").stdout(std::process::Stdio::piped()).spawn().unwrap();
    // Longer than one 2 s probe, well inside the 10 s patience.
    std::thread::sleep(Duration::from_secs(3));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let out = up.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && text.contains("already up"), "{text}");
    assert!(alive(pid), "a slow Xvfb was killed");
    assert_eq!(cli.pid(), Some(pid));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A display xcua attached to gets no _XCUA_NONCE; xcua's own Xvfb has one.
#[test]
fn attached_display_is_not_marked() {
    let Some(x) = xvfb("mark") else { return };
    let b = backend(&x, false);
    b.call(&Request::Screenshot { max_side: Some(64), format: ImageFormat::Png, quality: None, cursor: None, region: None }).unwrap();
    b.call(&Request::PointerMove { x: 5, y: 5 }).unwrap();
    let _ = b.doctor(None);
    let (conn, screen) = RustConnection::connect(Some(&x.display)).unwrap();
    let root = conn.setup().roots[screen].root;
    assert_eq!(xcua::x11::read_nonce(&conn, root), None, "wrote _XCUA_NONCE on an attached display");
    let props = conn.list_properties(root).unwrap().reply().unwrap().atoms;
    for a in props {
        let name = conn.get_atom_name(a).unwrap().reply().unwrap().name;
        assert!(!String::from_utf8_lossy(&name).starts_with("_XCUA"), "root property {}", String::from_utf8_lossy(&name));
    }

    let dir = std::env::temp_dir().join(format!("xcua-test-mark-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    cli.ok(&["--size", "320x240", "up"]);
    let own = cli.env_display().unwrap();
    let (conn, screen) = RustConnection::connect(Some(&own)).unwrap();
    let root = conn.setup().roots[screen].root;
    let nonce = std::fs::read_to_string(cli.state.join("nonce")).unwrap();
    assert_eq!(xcua::x11::read_nonce(&conn, root).as_deref(), Some(nonce.trim()));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CLI verbs map coordinates through the last screenshot's view, as the
/// MCP tools do: after `screenshot --max-side 320` of a 640x480 screen,
/// `click 100 88` lands on screen (200, 176), focuses the window there, and
/// `type` reaches it.
#[test]
fn cli_verbs_map_through_the_last_screenshot() {
    let Some(x) = xvfb("verbs") else { return };
    let (conn, win) = red_window(&x.display);
    let cli = Cli::new(x.dir.join("state"), None);
    let d = x.display.as_str();
    let shot = x.dir.join("half.png");
    std::fs::create_dir_all(&x.dir).unwrap();
    let out = cli.ok(&["--display", d, "screenshot", "-o", shot.to_str().unwrap(), "--max-side", "320"]);
    assert!(out.contains("320x240 of 640x480"), "{out}");
    let view: serde_json::Value = serde_json::from_slice(&std::fs::read(cli.state.join("view.json")).unwrap()).unwrap();
    assert_eq!(view["scale"], 2.0);

    assert_eq!(cli.ok(&["--display", d, "click", "100", "88"]).trim(), "OK");
    assert_eq!(conn.get_input_focus().unwrap().reply().unwrap().focus, win, "click did not focus the window");
    let press = wait_events(&conn, 1, Duration::from_secs(5), |e| matches!(e, Event::ButtonPress(_)));
    match press.first() {
        Some(Event::ButtonPress(p)) => assert_eq!((p.root_x, p.root_y), (200, 176), "click not mapped through the view"),
        other => panic!("no ButtonPress: {other:?}"),
    }
    let cursor: serde_json::Value = serde_json::from_str(&cli.ok(&["--display", d, "--json", "cursor"])).unwrap();
    assert_eq!((cursor["x"].as_i64(), cursor["y"].as_i64()), (Some(100), Some(88)));
    assert_eq!(cursor["screen"], serde_json::json!([200, 176]));

    let map = xcua::input::Input::open(d).unwrap().keymap().unwrap();
    cli.ok(&["--display", d, "type", "hI"]);
    let keys: Vec<u8> = wait_events(&conn, 3, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_)))
        .into_iter()
        .filter_map(|e| if let Event::KeyPress(k) = e { Some(k.detail) } else { None })
        .collect();
    let (h, i, shift) = (map.lookup(0x68).unwrap().0, map.lookup(0x69).unwrap().0, map.lookup(xcua::keysym::SHIFT_L).unwrap().0);
    assert_eq!(keys, vec![h, shift, i]);

    // batch: the MCP computer_batch shape, mapped the same way; a
    // screenshot inside it becomes the view; it stops at the first failure.
    let shots = x.dir.join("shots");
    let out = cli.ok(&[
        "--display", d, "batch", "--out-dir", shots.to_str().unwrap(),
        r#"{"actions": [{"action": "left_click", "coordinate": [100, 88]}, {"action": "type", "text": "ok"}, {"action": "screenshot"}]}"#,
    ]);
    assert!(out.contains("[0] left_click: OK") && out.contains("[2] screenshot: "), "{out}");
    assert_eq!(wait_events(&conn, 2, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_))).len(), 2);
    let view: serde_json::Value = serde_json::from_slice(&std::fs::read(cli.state.join("view.json")).unwrap()).unwrap();
    assert_eq!(view["max_side"], 320, "the batch screenshot kept the view's max_side");
    let (ok, text) = cli.run(&["--display", d, "batch", r#"[{"action": "left_click"}, {"action": "type", "text": "x"}]"#]);
    assert!(!ok && text.contains("needs `coordinate`") && text.contains("Not executed"), "{text}");

    // Errors exit non-zero with a message, as JSON with --json.
    let (ok, text) = cli.run(&["--display", d, "--json", "key", "ctrl+nosuchkey"]);
    let err: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert!(!ok && err["ok"] == false && err["error"].as_str().unwrap().contains("unknown key name: nosuchkey"), "{text}");

    // `type -` drops the one newline echo adds: no Return.
    let mut child = cli
        .cmd()
        .args(["--display", d, "type", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"ab\n").unwrap();
    assert!(child.wait().unwrap().success());
    let typed = wait_events(&conn, 2, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_)));
    assert_eq!(typed.len(), 2, "echo's newline was typed as Return");

    // Two shells typing at once take turns instead of interleaving.
    let a = cli.cmd().args(["--display", d, "type", "aaaaaaaaaaaaaaaaaaaa"]).spawn().unwrap();
    let b = cli.cmd().args(["--display", d, "type", "bbbbbbbbbbbbbbbbbbbb"]).spawn().unwrap();
    assert!(a.wait_with_output().unwrap().status.success() && b.wait_with_output().unwrap().status.success());
    let ka = map.lookup(0x61).unwrap().0;
    let seq: Vec<bool> = wait_events(&conn, 40, Duration::from_secs(10), |e| matches!(e, Event::KeyPress(_)))
        .into_iter()
        .filter_map(|e| if let Event::KeyPress(k) = e { Some(k.detail == ka) } else { None })
        .collect();
    assert_eq!(seq.len(), 40);
    let switches = seq.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(switches, 1, "verbs from two shells interleaved: {seq:?}");
    // A display that does not answer is an error naming it. (A view taken
    // of another display is set aside; view.rs tests that.)
    let (ok, text) = cli.run(&["--display", ":9999", "click", "1", "1"]);
    assert!(!ok && text.contains(":9999"), "{text}");
}

/// `record start` leaves a detached recorder that later invocations stamp,
/// mark and stop; a recorder that died is reported, not hung on.
#[test]
fn cli_record_across_processes() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-rec-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    let out = cli.ok(&["--size", "320x240", "record", "start", "--name", "r1", "--fps", "10"]);
    assert!(out.starts_with("recording ") && out.contains("r1.frames"), "{out}");
    let pid: i32 = std::fs::read_to_string(cli.state.join("record.pid")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    assert!(alive(pid));
    let (ok, again) = cli.run(&["record", "start"]);
    assert!(!ok && again.contains("already recording"), "{again}");
    cli.ok(&["click", "10", "10"]);
    cli.ok(&["record", "mark", "after the click"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(cli.ok(&["record", "status"]).starts_with("recording "));
    let stop = cli.ok(&["record", "stop"]);
    assert!(stop.contains("finalized=true") && stop.contains("left_click [10, 10]") && stop.contains("mark: after the click"), "{stop}");
    assert!(stop.contains("contact sheet "), "{stop}");
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)), "recorder still running after stop");
    assert!(!cli.state.join("record.pid").exists());
    let manifest = cli.state.join("recordings/r1.json");
    assert!(manifest.exists());
    let (ok, again) = cli.run(&["record", "stop"]);
    assert!(!ok && again.contains("not recording"), "{again}");
    let frames = dir.join("frames");
    cli.ok(&["record", "frames", "--count", "2", "-o", frames.to_str().unwrap()]);
    assert_eq!(std::fs::read_dir(&frames).unwrap().count(), 2);

    // A recorder killed outright: stop says so instead of waiting.
    cli.ok(&["record", "start", "--name", "r2"]);
    let pid: i32 = std::fs::read_to_string(cli.state.join("record.pid")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    let (ok, died) = cli.run(&["record", "stop"]);
    assert!(!ok && died.contains("died"), "{died}");
    // Frames of a cut-off recording: repeated times collapse, names differ.
    let cut = dir.join("cut");
    cli.ok(&["record", "frames", "--count", "3", "-o", cut.to_str().unwrap()]);
    let names: Vec<String> = std::fs::read_dir(&cut).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.is_empty() && names.iter().all(|n| n.starts_with("frame-0")), "{names:?}");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `doctor` never starts a display: with none it says what `up` would start.
#[test]
fn doctor_without_a_display_starts_nothing() {
    let dir = std::env::temp_dir().join(format!("xcua-test-doc-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    let out = cli.ok(&["--size", "800x600", "doctor"]);
    assert!(out.contains("would start") && out.contains("800x600") && out.contains("unknown until"), "{out}");
    let j: serde_json::Value = serde_json::from_str(&cli.ok(&["--json", "doctor"])).unwrap();
    assert!(j["display"].is_null(), "{j}");
    assert!(cli.env_display().is_none() && cli.pid().is_none(), "doctor started a display");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `up --size` refuses a running display of another size; `--restart`
/// replaces it.
#[test]
fn up_size_mismatch_refuses_then_restarts() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-size-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    cli.ok(&["up", "--size", "320x240"]);
    let old = cli.pid().unwrap();
    let (ok, text) = cli.run(&["up", "--size", "400x300"]);
    assert!(!ok && text.contains("running at 320x240") && text.contains("--restart"), "{text}");
    assert!(cli.ok(&["up"]).contains("already up"), "no --size: the running one is fine");
    let out = cli.ok(&["up", "--size", "400x300", "--restart"]);
    assert!(out.contains("started Xvfb") && out.contains("400x300"), "{out}");
    assert!(wait_until(Duration::from_secs(15), || !alive(old)), "old Xvfb left running");
    let shot = dir.join("s.png");
    cli.ok(&["screenshot", "-o", shot.to_str().unwrap()]);
    assert_eq!(image::image_dimensions(&shot).unwrap(), (400, 300));
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `up --wm <cmd>` starts a window manager on xcua's display, tracked like
/// the Xvfb: reported by status, its death reported, stopped by `down`.
#[test]
fn up_wm_is_started_reported_and_stopped() {
    if !have_xvfb() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-wm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let seen = dir.join("wm-display.txt");
    let fake = dir.join("fake-wm.sh");
    std::fs::write(&fake, format!("#!/bin/sh\necho \"$DISPLAY\" > {}\nexec sleep 300\n", seen.display())).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cli = Cli::new(dir.join("state"), None);
    let out = cli.ok(&["up", "--size", "320x240", "--wm", fake.to_str().unwrap()]);
    assert!(out.contains("started window manager"), "{out}");
    let wm: i32 = std::fs::read_to_string(cli.state.join("wm.pid")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    assert!(wait_until(Duration::from_secs(5), || seen.exists()));
    assert_eq!(std::fs::read_to_string(&seen).unwrap().trim(), cli.env_display().unwrap(), "WM not on xcua's display");
    assert!(cli.ok(&["status"]).contains("running; it owns focus"));
    // A WM that dies is reported, not restarted behind the agent's back.
    unsafe { libc::kill(wm, libc::SIGKILL) };
    assert!(wait_until(Duration::from_secs(5), || !alive(wm)));
    assert!(cli.ok(&["status"]).contains("DIED"));
    let up = cli.ok(&["up"]);
    assert!(up.contains("has died"), "{up}");
    // Started again on request; a restart brings it back; `down` stops
    // it with the Xvfb.
    cli.ok(&["up", "--wm", fake.to_str().unwrap()]);
    let before: i32 = std::fs::read_to_string(cli.state.join("wm.pid")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let out = cli.ok(&["up", "--size", "400x300", "--restart"]);
    assert!(out.contains("started window manager"), "restart dropped the WM: {out}");
    assert!(wait_until(Duration::from_secs(5), || !alive(before)), "old WM left running");
    let wm: i32 = std::fs::read_to_string(cli.state.join("wm.pid")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let xvfb_pid = cli.pid().unwrap();
    let down = cli.ok(&["down"]);
    assert!(down.contains("stopped window manager") && down.contains("stopped Xvfb"), "{down}");
    assert!(wait_until(Duration::from_secs(5), || !alive(wm) && !alive(xvfb_pid)));

    // --wm auto, when one of the known WMs is installed here.
    if let Some(name) = xcua::xvfb::find_wm() {
        let out = cli.ok(&["up", "--size", "320x240", "--wm", "auto"]);
        assert!(out.contains(&format!("started window manager {name}")), "{out}");
        let down = cli.ok(&["down"]);
        assert!(down.contains("stopped window manager"), "{down}");
    }
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run xcua with a deadline; None if it had to be killed (a hang).
fn run_within(cli: &Cli, args: &[&str], limit: Duration) -> Option<(bool, String)> {
    let mut child = cli
        .cmd()
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            return Some((out.status.success(), text));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Under real twm (xcua's generated rc), a new window is placed at once
/// and screenshots keep answering: twm's default interactive placement
/// grabs the server until someone clicks.
#[test]
fn twm_places_windows_without_grabbing() {
    if !have_xvfb() || xcua::xvfb::on_path("twm").is_none() {
        eprintln!("twm or Xvfb missing; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-twm-{}", std::process::id()));
    let cli = Cli::new(dir.join("state"), None);
    let out = cli.ok(&["up", "--size", "640x480", "--wm", "twm"]);
    assert!(out.contains("started window manager twm -f "), "{out}");
    let display = cli.env_display().unwrap();
    // red_window waits for MapNotify, which interactive placement withholds.
    let (_conn, _win) = red_window(&display);
    let shot = dir.join("s.png");
    let r = run_within(&cli, &["screenshot", "-o", shot.to_str().unwrap()], Duration::from_secs(20));
    let (ok, text) = r.expect("screenshot hung: the server is grabbed");
    assert!(ok, "{text}");
    assert!(cli.ok(&["status"]).contains("running; it owns focus"), "twm exited");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without /proc a recorded WM pid is never signalled: it may be anyone's.
#[test]
fn no_proc_never_signals_a_wm_pid() {
    let have_bwrap = xcua::xvfb::on_path("bwrap").is_some();
    if !have_xvfb() || !have_bwrap {
        eprintln!("bwrap or Xvfb missing; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("xcua-test-wmproc-{}", std::process::id()));
    let mut cli = Cli::new(dir.join("state"), None);
    cli.wrap = ["bwrap", "--dev-bind", "/", "/", "--tmpfs", "/proc"].map(String::from).to_vec();
    cli.ok(&["--size", "320x240", "up"]);
    // A stale wm.pid naming an unrelated live process.
    let mut bystander = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    std::fs::write(cli.state.join("wm.pid"), format!("{}\n", bystander.id())).unwrap();
    std::fs::write(cli.state.join("wm.cmd"), "jwm\n").unwrap();
    let down = cli.ok(&["down"]);
    assert!(down.contains("forgot window manager"), "{down}");
    assert!(bystander.try_wait().unwrap().is_none(), "xcua signalled an unrelated process");
    let _ = bystander.kill();
    let _ = bystander.wait();
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--wm auto` with no WM installed is a warning, not a failure; an
/// explicit WM that cannot start fails, saying the display is up.
#[test]
fn wm_failures_say_the_display_is_up() {
    let (Some(xvfb_bin), Some(xkbcomp)) = (xcua::xvfb::on_path("Xvfb"), xcua::xvfb::on_path("xkbcomp")) else { return };
    let dir = std::env::temp_dir().join(format!("xcua-test-nowm-{}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (src, name) in [(&xvfb_bin, "Xvfb"), (&xkbcomp, "xkbcomp")] {
        let _ = std::os::unix::fs::symlink(src, bin.join(name));
    }
    let mut cli = Cli::new(dir.join("state"), None);
    // A PATH with Xvfb and nothing else: no window manager to find.
    cli.env.push(("PATH".into(), bin.display().to_string()));
    let out = cli.ok(&["up", "--size", "320x240", "--wm", "auto"]);
    assert!(out.contains("no window manager installed") && out.contains("continuing without one"), "{out}");
    let display = cli.env_display().unwrap();
    assert!(xcua::xvfb::answers(&display));
    let (ok, text) = cli.run(&["up", "--wm", "/nonexistent/wm"]);
    assert!(!ok && text.contains("itself is up and usable"), "{text}");
    assert!(xcua::xvfb::answers(&display), "a failed WM took the display down");
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `wait-window` finds a window that maps late, by class, and with
/// --painted waits until it is drawn into.
#[test]
fn wait_window_mapped_then_painted() {
    let Some(x) = xvfb("waitwin") else { return };
    let cli = Cli::new(x.dir.join("state"), None);
    let d = x.display.clone();
    let late = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        app_window(&d, (50, 60, 300, 200), 0x336699, Some("xcuatest"))
    });
    let out = cli.ok(&["--display", &x.display, "wait-window", "--class", "XCUATEST", "--timeout", "10000"]);
    assert!(out.starts_with("mapped after") && out.contains("xcuatest window") && out.contains("300x200+50+60"), "{out}");
    let (conn, win) = late.join().unwrap();
    // One colour: not painted.
    let (ok, text) = cli.run(&["--display", &x.display, "wait-window", "--class", "xcuatest", "--painted", "--timeout", "1200"]);
    assert!(!ok && text.contains("still one colour"), "{text}");
    // Draw something: painted.
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, win, &x11rb::protocol::xproto::CreateGCAux::new().foreground(0xffffff)).unwrap();
    conn.poly_fill_rectangle(win, gc, &[x11rb::protocol::xproto::Rectangle { x: 20, y: 20, width: 120, height: 40 }]).unwrap();
    conn.flush().unwrap();
    let out = cli.ok(&["--display", &x.display, "wait-window", "--class", "xcuatest", "--painted", "--timeout", "5000"]);
    assert!(out.starts_with("painted after"), "{out}");
    let (ok, text) = cli.run(&["--display", &x.display, "wait-window", "--name", "nosuch", "--timeout", "500"]);
    assert!(!ok && text.contains("no matching window") && text.contains("xcuatest"), "{text}");
}

/// `type` waits for the focus to settle: a focus change that lands just
/// after it starts (a WM's click-to-focus, an app moving focus into a
/// field) does not split the text between two windows.
#[test]
fn type_waits_for_focus_to_settle() {
    let Some(x) = xvfb("settle") else { return };
    let b = backend(&x, false);
    let (ca, wa) = app_window(&x.display, (10, 10, 100, 100), 0xff0000, None);
    let (cb, wb) = app_window(&x.display, (200, 10, 100, 100), 0x00ff00, None);
    ca.set_input_focus(x11rb::protocol::xproto::InputFocus::POINTER_ROOT, wa, x11rb::CURRENT_TIME).unwrap();
    ca.flush().unwrap();
    ca.get_input_focus().unwrap().reply().unwrap();
    let mover = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        cb.set_input_focus(x11rb::protocol::xproto::InputFocus::POINTER_ROOT, wb, x11rb::CURRENT_TIME).unwrap();
        cb.flush().unwrap();
        cb
    });
    b.call(&Request::Type { text: "hello".into(), delay_ms: None }).unwrap();
    let cb = mover.join().unwrap();
    let on_a = wait_events(&ca, 5, Duration::from_millis(300), |e| matches!(e, Event::KeyPress(_)));
    let on_b = wait_events(&cb, 5, Duration::from_secs(5), |e| matches!(e, Event::KeyPress(_)));
    assert_eq!((on_a.len(), on_b.len()), (0, 5), "text split between the old and the new focus");
}

/// `maximize` fills the screen: by ConfigureWindow without a WM, and
/// under twm (no EWMH) by the same fallback.
#[test]
fn maximize_fills_the_screen() {
    let Some(x) = xvfb("max") else { return };
    let cli = Cli::new(x.dir.join("state"), None);
    let (_conn, _win) = app_window(&x.display, (100, 100, 200, 150), 0x336699, Some("xcuatest"));
    let out = cli.ok(&["--display", &x.display, "maximize", "--class", "xcuatest"]);
    assert!(out.contains("640x480+0+0"), "{out}");
    let (ok, text) = cli.run(&["--display", &x.display, "maximize"]);
    assert!(!ok && text.contains("no window has the keyboard"), "{text}");

    if xcua::xvfb::on_path("twm").is_some() {
        let dir = std::env::temp_dir().join(format!("xcua-test-maxtwm-{}", std::process::id()));
        let cli = Cli::new(dir.join("state"), None);
        cli.ok(&["up", "--size", "640x480", "--wm", "twm"]);
        let display = cli.env_display().unwrap();
        let (_conn, _win) = app_window(&display, (100, 100, 200, 150), 0x336699, Some("xcuatest"));
        let out = cli.ok(&["maximize", "--class", "xcuatest"]);
        assert!(out.contains("resized to the screen") || out.contains("maximized"), "{out}");
        drop(cli);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
