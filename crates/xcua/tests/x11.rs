//! Against a private Xvfb per test (`-displayfd`, never the user's display):
//! capture, XTEST motion, focus on click, typing into a real X client (a
//! window this test creates and reads events from), Unicode typing through
//! spare keycodes. Skipped when Xvfb is not installed.

use std::process::Child;
use std::time::{Duration, Instant};

use kmscua_mcp::Backend;
use kmscua_proto::{Button, CursorInfo, ImageFormat, Rect, Request, ScreenshotInfo, Status};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, EventMask, WindowClass,
};
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
