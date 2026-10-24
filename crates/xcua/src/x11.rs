//! The X server side: connecting, reading the root window, the cursor, the
//! monitors, and what a window is called.
//!
//! Everything goes through x11rb's pure-Rust `RustConnection`, so xcua links
//! no libxcb and builds static. Capture is a core `GetImage` of the root
//! window in ZPixmap, converted through the root visual's masks; the server
//! leaves the software cursor out of it, so the cursor is read through
//! XFIXES and composited the way cuad composites the DRM cursor plane.
//!
//! MIT-SHM is an opt-in fast path. The segment is a memfd passed over the
//! X socket (`ShmAttachFd`, SHM 1.2), not a SysV segment, so it still works
//! under `bwrap --unshare-ipc`; any failure to set it up or to use it drops
//! back to `GetImage` for the rest of the connection's life.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use image::RgbaImage;
use kmscua_proto::{CursorInfo, DisplayInfo, Rect};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;
use x11rb::wrapper::ConnectionExt as _;

use crate::pixels::{self, Shot};

/// x11rb's stream with a deadline on every wait: a server that is stopped
/// (SIGSTOP) or wedged accepts the socket and then never answers, and a
/// plain connection would wait for it forever.
pub struct DeadlineStream {
    inner: DefaultStream,
    deadline: Instant,
}

impl Stream for DeadlineStream {
    fn poll(&self, mode: PollMode) -> std::io::Result<()> {
        let mut events = 0;
        if mode.readable() {
            events |= libc::POLLIN;
        }
        if mode.writable() {
            events |= libc::POLLOUT;
        }
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "X server did not answer in time"));
            }
            let mut pfd = libc::pollfd { fd: self.inner.as_raw_fd(), events, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, i32::MAX as u128) as i32) };
            if r > 0 {
                return Ok(());
            }
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }

    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> std::io::Result<usize> {
        self.inner.read(buf, fds)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> std::io::Result<usize> {
        self.inner.write(buf, fds)
    }

    fn write_vectored(&self, bufs: &[std::io::IoSlice<'_>], fds: &mut Vec<RawFdContainer>) -> std::io::Result<usize> {
        self.inner.write_vectored(bufs, fds)
    }
}

/// A connection whose setup and every later reply must arrive within
/// `timeout` of now, for liveness and identity probes. Same address order
/// and authentication as `RustConnection::connect`.
pub fn connect_within(display: &str, timeout: Duration) -> Result<(RustConnection<DeadlineStream>, Window)> {
    use x11rb::reexports::x11rb_protocol::{parse_display::parse_display, xauth::get_auth};
    let deadline = Instant::now() + timeout;
    let parsed = parse_display(Some(display)).map_err(|e| anyhow!("bad display {display:?}: {e:?}"))?;
    let screen = parsed.screen as usize;
    let mut last = None;
    for addr in parsed.connect_instruction() {
        // connect(2) on a unix socket does not block on a stopped server
        // (the kernel's backlog accepts), so only what follows needs the
        // deadline.
        let (inner, (family, address)) = match DefaultStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        let (name, data) = get_auth(family, &address, parsed.display).ok().flatten().unwrap_or_default();
        let conn = RustConnection::connect_to_stream_with_auth_info(DeadlineStream { inner, deadline }, screen, name, data)
            .with_context(|| format!("X connection setup on {display}"))?;
        let root = conn.setup().roots.get(screen).ok_or_else(|| anyhow!("no screen {screen} on {display}"))?.root;
        return Ok((conn, root));
    }
    Err(anyhow!("no X server on {display}: {}", last.map(|e| e.to_string()).unwrap_or_else(|| "no address".into())))
}

/// The root-window property that identifies an X server instance: set by
/// xcua right after it starts an Xvfb, or by the backend on a display that
/// has none, and lost when the server restarts or resets.
pub const NONCE_PROPERTY: &str = "_XCUA_NONCE";

/// A fresh random identity.
pub fn new_nonce() -> String {
    let mut b = [0u8; 16];
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n != b.len() as isize {
        // No getrandom: time and pid are unique enough for an identity.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b[..8].copy_from_slice(&(t.as_nanos() as u64).to_le_bytes());
        b[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn read_nonce<C: Connection>(conn: &C, root: Window) -> Option<String> {
    let atom = conn.intern_atom(true, NONCE_PROPERTY.as_bytes()).ok()?.reply().ok()?.atom;
    if atom == x11rb::NONE {
        return None;
    }
    let r = conn.get_property(false, root, atom, AtomEnum::STRING, 0, 64).ok()?.reply().ok()?;
    (r.type_ != x11rb::NONE && !r.value.is_empty()).then(|| String::from_utf8_lossy(&r.value).into_owned())
}

pub fn write_nonce<C: Connection>(conn: &C, root: Window, nonce: &str) -> Result<()> {
    let atom = conn.intern_atom(false, NONCE_PROPERTY.as_bytes())?.reply()?.atom;
    conn.change_property8(xproto::PropMode::REPLACE, root, atom, AtomEnum::STRING, nonce.as_bytes())?
        .check()
        .context("set the _XCUA_NONCE root property")?;
    Ok(())
}

/// The server's identity, setting one when it has none.
pub fn ensure_nonce<C: Connection>(conn: &C, root: Window) -> Result<String> {
    if let Some(n) = read_nonce(conn, root) {
        return Ok(n);
    }
    let n = new_nonce();
    write_nonce(conn, root, &n)?;
    Ok(n)
}

/// Connect to `display` (":N"), returning the connection and its root.
pub fn connect(display: &str) -> Result<(RustConnection, Window)> {
    let (conn, screen) = RustConnection::connect(Some(display))
        .with_context(|| format!("connect to X display {display}"))?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// The root window's size, refusing an empty one: an X server with no
/// output (Xwayland before its first monitor, say) reports 0x0, and no
/// screenshot or pointer coordinate makes sense on it.
pub fn root_size(conn: &RustConnection, root: Window, display: &str) -> Result<(u32, u32)> {
    let g = conn.get_geometry(root).context("GetGeometry on the root window")?.reply()?;
    if g.width == 0 || g.height == 0 {
        bail!("the root window of {display} is {}x{}: the server has no usable screen", g.width, g.height);
    }
    Ok((g.width as u32, g.height as u32))
}

/// Which of the extensions xcua cares about the server has, in a fixed order.
pub fn extensions(conn: &RustConnection) -> Vec<(&'static str, bool)> {
    ["XTEST", "XFIXES", "MIT-SHM", "RANDR", "GLX", "RENDER", "XKEYBOARD"]
        .into_iter()
        .map(|name| (name, conn.extension_information(name).ok().flatten().is_some()))
        .collect()
}

/// How the root visual lays out a pixel in a ZPixmap.
#[derive(Clone, Copy, Debug)]
struct PixelFormat {
    bpp: u8,
    pad: u8,
    msb_first: bool,
    masks: [u32; 3],
}

impl PixelFormat {
    /// Whether xcua can read a root of this depth, pixel size and visual.
    fn check(display: &str, depth: u8, bpp: u8, red_mask: u32, class: xproto::VisualClass) -> Result<()> {
        if !matches!(bpp, 16 | 24 | 32) || red_mask == 0 {
            bail!(
                "unsupported root visual on {display}: depth {depth}, {bpp} bpp, class {class:?} (xcua needs TrueColor or DirectColor at 16, 24 or 32 bpp)"
            );
        }
        Ok(())
    }

    fn of(conn: &RustConnection, root: Window, display: &str) -> Result<Self> {
        let setup = conn.setup();
        let screen = setup
            .roots
            .iter()
            .find(|s| s.root == root)
            .ok_or_else(|| anyhow!("root window not in setup"))?;
        let depth = screen.root_depth;
        let fmt = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or_else(|| anyhow!("no pixmap format for depth {depth}"))?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.visual_id == screen.root_visual)
            .ok_or_else(|| anyhow!("root visual not in setup"))?;
        Self::check(display, depth, fmt.bits_per_pixel, visual.red_mask, visual.class)?;
        Ok(Self {
            bpp: fmt.bits_per_pixel,
            pad: fmt.scanline_pad,
            msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
            masks: [visual.red_mask, visual.green_mask, visual.blue_mask],
        })
    }

    fn stride(&self, width: u32) -> usize {
        let bits = width as usize * self.bpp as usize;
        let pad = self.pad.max(8) as usize;
        bits.div_ceil(pad) * pad / 8
    }

    /// ZPixmap bytes to tightly packed opaque RGBA.
    fn convert(&self, data: &[u8], w: u32, h: u32) -> Result<RgbaImage> {
        if w == 0 || h == 0 {
            bail!("cannot convert an empty {w}x{h} image");
        }
        let stride = self.stride(w);
        if data.len() < stride * h as usize {
            bail!("image data short: {} bytes for {w}x{h} at stride {stride}", data.len());
        }
        let mut img = RgbaImage::new(w, h);
        let out: &mut [u8] = img.as_mut();
        let row_out = w as usize * 4;
        // The common case, little-endian x8r8g8b8: B,G,R,X in memory.
        if self.bpp == 32 && !self.msb_first && self.masks == [0xff0000, 0xff00, 0xff] {
            for (src, dst) in data.chunks(stride).zip(out.chunks_exact_mut(row_out)) {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 0xff;
                }
            }
            return Ok(img);
        }
        let bytes = self.bpp as usize / 8;
        let chan = |v: u32, mask: u32| -> u8 {
            let shift = mask.trailing_zeros();
            let bits = (mask >> shift).count_ones();
            let c = (v & mask) >> shift;
            if bits >= 8 {
                (c >> (bits - 8)) as u8
            } else {
                ((c * 255) / ((1 << bits) - 1)) as u8
            }
        };
        for (src, dst) in data.chunks(stride).zip(out.chunks_exact_mut(row_out)) {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(bytes)) {
                let v = if self.msb_first {
                    s.iter().fold(0u32, |a, &b| (a << 8) | b as u32)
                } else {
                    s.iter().rev().fold(0u32, |a, &b| (a << 8) | b as u32)
                };
                d[0] = chan(v, self.masks[0]);
                d[1] = chan(v, self.masks[1]);
                d[2] = chan(v, self.masks[2]);
                d[3] = 0xff;
            }
        }
        Ok(img)
    }
}

/// A memfd mapped here and attached to the server.
struct ShmSeg {
    seg: u32,
    ptr: *mut u8,
    size: usize,
}

// The mapping is owned by this struct and only touched through `&mut Capture`.
unsafe impl Send for ShmSeg {}

impl ShmSeg {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        let fd = unsafe { libc::memfd_create(c"xcua-shm".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error()).context("ftruncate shm");
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap shm");
        }
        let seg = conn.generate_id()?;
        let attached = conn
            .shm_attach_fd(seg, fd, false)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from));
        if let Err(e) = attached {
            unsafe { libc::munmap(ptr, size) };
            return Err(e.context("ShmAttachFd"));
        }
        Ok(Self { seg, ptr: ptr as *mut u8, size })
    }

    fn release(self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg).map(|c| c.ignore_error());
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size) };
    }
}

/// Root-window capture on its own connection.
pub struct Capture {
    conn: RustConnection,
    root: Window,
    display: String,
    format: PixelFormat,
    xfixes: bool,
    /// Whether to try SHM at all (opt-in); cleared on the first failure.
    want_shm: bool,
    shm: Option<ShmSeg>,
}

impl Capture {
    pub fn open(display: &str, shm: bool) -> Result<Self> {
        let (conn, root) = connect(display)?;
        let format = PixelFormat::of(&conn, root, display)?;
        // XFIXES answers nothing before the client states its version.
        let xfixes = conn.extension_information("XFIXES")?.is_some()
            && conn.xfixes_query_version(5, 0)?.reply().is_ok();
        let want_shm = shm
            && conn.extension_information("MIT-SHM")?.is_some()
            && conn
                .shm_query_version()?
                .reply()
                .map(|v| (v.major_version, v.minor_version) >= (1, 2))
                .unwrap_or(false);
        if shm && !want_shm {
            log::info!("MIT-SHM 1.2 not available, using GetImage");
        }
        let cap = Self { conn, root, display: display.to_string(), format, xfixes, want_shm, shm: None };
        // Fail at open, with the reason, rather than on the first request.
        cap.size()?;
        Ok(cap)
    }

    /// Whether the connection still answers a round trip.
    pub fn alive(&self) -> bool {
        self.conn.get_input_focus().is_ok_and(|c| c.reply().is_ok())
    }

    /// This server's identity (`_XCUA_NONCE`), set if it has none.
    pub fn nonce(&self) -> Result<String> {
        ensure_nonce(&self.conn, self.root)
    }

    pub fn cursor_supported(&self) -> bool {
        self.xfixes
    }

    /// Whether the last grab went through shared memory.
    pub fn shm_active(&self) -> bool {
        self.shm.is_some()
    }

    /// Root window size now (RandR may have changed it since connect).
    pub fn size(&self) -> Result<(u32, u32)> {
        root_size(&self.conn, self.root, &self.display)
    }

    pub fn desktop(&self) -> Result<Rect> {
        let (w, h) = self.size()?;
        Ok(Rect { x: 0, y: 0, width: w, height: h })
    }

    /// One frame of `region` (whole screen when None), cursor composited
    /// when asked and XFIXES is there.
    pub fn grab(&mut self, with_cursor: bool, region: Option<Rect>) -> Result<Shot> {
        let t0 = Instant::now();
        let (fw, fh) = self.size()?;
        let region = match region {
            Some(r) => pixels::clamp_rect(r, fw, fh).ok_or_else(|| anyhow!("region outside the screen"))?,
            None => Rect { x: 0, y: 0, width: fw, height: fh },
        };
        let mut rgba = match self.grab_shm(&region) {
            Some(img) => img,
            None => self.grab_core(&region)?,
        };
        let mut cursor = None;
        if with_cursor && self.xfixes {
            match self.cursor_image() {
                Ok((info, px)) => {
                    if info.visible {
                        pixels::composite_cursor(&mut rgba, &region, &info, &px);
                    }
                    cursor = Some(info);
                }
                Err(e) => log::debug!("cursor unavailable this frame: {e:#}"),
            }
        }
        Ok(Shot {
            rgba,
            origin: (region.x, region.y),
            cursor,
            grab_ms: t0.elapsed().as_secs_f32() * 1000.0,
        })
    }

    fn grab_core(&self, r: &Rect) -> Result<RgbaImage> {
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, r.x as i16, r.y as i16, r.width as u16, r.height as u16, !0)?
            .reply()
            .context("GetImage on the root window")?;
        self.format.convert(&reply.data, r.width, r.height)
    }

    /// The SHM path, or None to fall back. A failure turns SHM off for good.
    fn grab_shm(&mut self, r: &Rect) -> Option<RgbaImage> {
        if !self.want_shm {
            return None;
        }
        match self.try_shm(r) {
            Ok(img) => Some(img),
            Err(e) => {
                log::warn!("MIT-SHM capture failed, falling back to GetImage: {e:#}");
                self.want_shm = false;
                if let Some(s) = self.shm.take() {
                    s.release(&self.conn);
                }
                None
            }
        }
    }

    fn try_shm(&mut self, r: &Rect) -> Result<RgbaImage> {
        let need = self.format.stride(r.width) * r.height as usize;
        if self.shm.as_ref().is_none_or(|s| s.size < need) {
            if let Some(old) = self.shm.take() {
                old.release(&self.conn);
            }
            // Room for the whole screen, so regions never reallocate.
            let (fw, fh) = self.size()?;
            let size = need.max(self.format.stride(fw) * fh as usize);
            self.shm = Some(ShmSeg::new(&self.conn, size)?);
        }
        let seg = self.shm.as_ref().unwrap();
        self.conn
            .shm_get_image(
                self.root,
                r.x as i16,
                r.y as i16,
                r.width as u16,
                r.height as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                seg.seg,
                0,
            )?
            .reply()
            .context("ShmGetImage")?;
        let data = unsafe { std::slice::from_raw_parts(seg.ptr, need) };
        self.format.convert(data, r.width, r.height)
    }

    /// Cursor image and position: `x, y` the image's top-left on screen.
    fn cursor_image(&self) -> Result<(CursorInfo, Vec<u32>)> {
        let c = self.conn.xfixes_get_cursor_image()?.reply()?;
        let info = CursorInfo {
            // XFIXES hides nothing: a cursor that is "none" is a 1x1 or
            // fully transparent image, which composites to nothing.
            visible: c.width > 0 && c.height > 0,
            x: c.x as i32 - c.xhot as i32,
            y: c.y as i32 - c.yhot as i32,
            hot_x: c.xhot as i32,
            hot_y: c.yhot as i32,
            width: c.width as u32,
            height: c.height as u32,
        };
        Ok((info, c.cursor_image))
    }

    /// Where the pointer is, cuad's shape: top-left plus hotspot is the
    /// pointer. Without XFIXES the hotspot is 0 and the size unknown.
    pub fn cursor(&self) -> Result<CursorInfo> {
        if self.xfixes {
            return Ok(self.cursor_image()?.0);
        }
        let p = self.conn.query_pointer(self.root)?.reply()?;
        Ok(CursorInfo {
            visible: true,
            x: p.root_x as i32,
            y: p.root_y as i32,
            hot_x: 0,
            hot_y: 0,
            width: 0,
            height: 0,
        })
    }

    /// RandR 1.5 monitors, else the one screen.
    pub fn displays(&self) -> Result<Vec<DisplayInfo>> {
        let (w, h) = self.size()?;
        let screen = || DisplayInfo {
            name: "screen".into(),
            crtc_id: 0,
            connector_id: 0,
            x: 0,
            y: 0,
            width: w,
            height: h,
            refresh_hz: 0,
            active: true,
        };
        if self.conn.extension_information("RANDR")?.is_none() {
            return Ok(vec![screen()]);
        }
        let monitors = self
            .conn
            .randr_query_version(1, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .filter(|v| (v.major_version, v.minor_version) >= (1, 5))
            .and_then(|_| self.conn.randr_get_monitors(self.root, true).ok())
            .and_then(|c| c.reply().ok());
        let Some(m) = monitors else { return Ok(vec![screen()]) };
        let list: Vec<DisplayInfo> = m
            .monitors
            .iter()
            .map(|mon| DisplayInfo {
                name: atom_name(&self.conn, mon.name).unwrap_or_else(|| "monitor".into()),
                crtc_id: 0,
                connector_id: mon.outputs.first().copied().unwrap_or(0),
                x: mon.x.max(0) as u32,
                y: mon.y.max(0) as u32,
                width: mon.width as u32,
                height: mon.height as u32,
                refresh_hz: 0,
                active: true,
            })
            .collect();
        Ok(if list.is_empty() { vec![screen()] } else { list })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(s) = self.shm.take() {
            s.release(&self.conn);
        }
    }
}

fn atom_name(conn: &RustConnection, atom: u32) -> Option<String> {
    let r = conn.get_atom_name(atom).ok()?.reply().ok()?;
    Some(String::from_utf8_lossy(&r.name).into_owned())
}

pub fn intern(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn property(conn: &RustConnection, w: Window, prop: u32, ty: u32) -> Option<Vec<u8>> {
    let r = conn.get_property(false, w, prop, ty, 0, 4096).ok()?.reply().ok()?;
    if r.type_ == x11rb::NONE {
        None
    } else {
        Some(r.value)
    }
}

fn property32(conn: &RustConnection, w: Window, prop: u32, ty: u32) -> Option<u32> {
    let r = conn.get_property(false, w, prop, ty, 0, 1).ok()?.reply().ok()?;
    let v = r.value32()?.next();
    v
}

/// Whether a window manager runs: it sets `_NET_SUPPORTING_WM_CHECK` on
/// the root, or holds SubstructureRedirect on it.
pub fn has_wm(conn: &RustConnection, root: Window) -> bool {
    if let Ok(atom) = intern(conn, "_NET_SUPPORTING_WM_CHECK") {
        if property32(conn, root, atom, AtomEnum::WINDOW.into()).is_some_and(|w| w != 0) {
            return true;
        }
    }
    conn.get_window_attributes(root)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|a| u32::from(a.all_event_masks) & u32::from(xproto::EventMask::SUBSTRUCTURE_REDIRECT) != 0)
}

/// The child of root that holds `w` (w itself when it is one).
pub fn toplevel(conn: &RustConnection, root: Window, mut w: Window) -> Result<Window> {
    for _ in 0..64 {
        let t = conn.query_tree(w)?.reply()?;
        if t.parent == root || t.parent == x11rb::NONE {
            return Ok(w);
        }
        w = t.parent;
    }
    bail!("window tree deeper than 64 under {w:#x}")
}

/// The window carrying the client properties: the top-level itself, or
/// the child with `WM_STATE` that a reparenting WM put inside its frame.
fn client_of(conn: &RustConnection, top: Window) -> Window {
    let Ok(wm_state) = intern(conn, "WM_STATE") else { return top };
    if property(conn, top, wm_state, AtomEnum::ANY.into()).is_some() {
        return top;
    }
    let mut queue = vec![(top, 0)];
    while let Some((w, depth)) = queue.pop() {
        if depth > 0 && property(conn, w, wm_state, AtomEnum::ANY.into()).is_some() {
            return w;
        }
        if depth < 3 {
            if let Some(t) = conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                queue.extend(t.children.into_iter().map(|c| (c, depth + 1)));
            }
        }
    }
    top
}

/// Title, class, pid and geometry of `w`, one per line.
pub fn describe(conn: &RustConnection, root: Window, w: Window) -> String {
    let mut out = String::new();
    let client = toplevel(conn, root, w).map(|t| client_of(conn, t)).unwrap_or(w);
    let utf8 = intern(conn, "UTF8_STRING").unwrap_or(0);
    let net_name = intern(conn, "_NET_WM_NAME").unwrap_or(0);
    let title = property(conn, client, net_name, utf8)
        .or_else(|| property(conn, client, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into()))
        .map(|v| String::from_utf8_lossy(&v).into_owned());
    out.push_str(&format!("window: {client:#x}\n"));
    if client != w {
        out.push_str(&format!("focused subwindow: {w:#x}\n"));
    }
    out.push_str(&format!("title: {}\n", title.as_deref().unwrap_or("(none)")));
    if let Some(class) = property(conn, client, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into()) {
        let parts: Vec<String> = class
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        out.push_str(&format!("class: {}\n", parts.join(" / ")));
    }
    if let Some(pid) = intern(conn, "_NET_WM_PID")
        .ok()
        .and_then(|a| property32(conn, client, a, AtomEnum::CARDINAL.into()))
    {
        out.push_str(&format!("pid: {pid}\n"));
    }
    if let Some(g) = conn.get_geometry(client).ok().and_then(|c| c.reply().ok()) {
        let at = conn
            .translate_coordinates(client, root, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|t| (t.dst_x, t.dst_y))
            .unwrap_or((g.x, g.y));
        out.push_str(&format!("geometry: {}x{}+{}+{}\n", g.width, g.height, at.0, at.1));
    }
    out
}

/// What has keyboard focus, as plain text: `_NET_ACTIVE_WINDOW` when a WM
/// publishes it, else the core input focus.
pub fn focused_text(conn: &RustConnection, root: Window) -> Result<String> {
    let active = intern(conn, "_NET_ACTIVE_WINDOW")
        .ok()
        .and_then(|a| property32(conn, root, a, AtomEnum::WINDOW.into()))
        .filter(|&w| w != 0);
    if let Some(w) = active {
        return Ok(format!("source: _NET_ACTIVE_WINDOW\n{}", describe(conn, root, w)));
    }
    let focus = conn.get_input_focus()?.reply()?.focus;
    Ok(match focus {
        0 => "source: GetInputFocus\nfocus: None (keys go nowhere; click a window to focus it)\n".to_string(),
        1 => {
            let under = conn.query_pointer(root)?.reply()?.child;
            let mut s = "source: GetInputFocus\nfocus: PointerRoot (keys go to the window under the pointer)\n".to_string();
            if under != x11rb::NONE {
                s.push_str(&describe(conn, root, under));
            }
            s
        }
        w if w == root => "source: GetInputFocus\nfocus: root window\n".to_string(),
        w => format!("source: GetInputFocus\n{}", describe(conn, root, w)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_visual_is_an_error_naming_the_display() {
        let e = PixelFormat::check(":9", 8, 8, 0, xproto::VisualClass::PSEUDO_COLOR).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains(":9") && msg.contains("8 bpp"), "{msg}");
        assert!(PixelFormat::check(":9", 24, 32, 0xff0000, xproto::VisualClass::TRUE_COLOR).is_ok());
        assert!(PixelFormat::check(":9", 16, 16, 0xf800, xproto::VisualClass::TRUE_COLOR).is_ok());
    }

    #[test]
    fn convert_refuses_empty_and_short_images() {
        let f = PixelFormat { bpp: 32, pad: 32, msb_first: false, masks: [0xff0000, 0xff00, 0xff] };
        assert!(f.convert(&[], 0, 0).is_err());
        assert!(f.convert(&[0; 4], 2, 2).is_err());
        let img = f.convert(&[1, 2, 3, 0], 1, 1).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1, 255]);
    }

    #[test]
    fn convert_handles_other_layouts() {
        // 16 bpp RGB565, little-endian: pure red.
        let f = PixelFormat { bpp: 16, pad: 32, msb_first: false, masks: [0xf800, 0x07e0, 0x001f] };
        let img = f.convert(&[0x00, 0xf8, 0, 0], 1, 1).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        // 32 bpp big-endian x8r8g8b8: pure blue.
        let f = PixelFormat { bpp: 32, pad: 32, msb_first: true, masks: [0xff0000, 0xff00, 0xff] };
        let img = f.convert(&[0, 0, 0, 0xff], 1, 1).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255]);
    }
}
