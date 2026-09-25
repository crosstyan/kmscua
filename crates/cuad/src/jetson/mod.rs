//! Zero-copy recording on NVIDIA Jetson (L4T), behind the `jetson` feature.
//!
//! The scanout dma-buf goes to the hardware and never through the CPU:
//!
//!   KMS scanout (block-linear XR24) -> NvBufSurfaceImport -> VIC scale + convert
//!   -> NV12 surface -> NVENC (libnvv4l2, DMABUF) -> H.264 -> mp4.rs
//!
//! The CPU blends the cursor into the NV12 surface (a few thousand pixels),
//! copies the bitstream, and writes the MP4. The GStreamer path instead
//! detiles on the GPU through EGL, reads 33 MB back per 4K frame, downscales
//! and pipes RGBA to `nvvidconv`. The C side is in nv.c.

mod mp4;

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, CStr};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};

use crate::capture::{Capturer, Scanout};
use mp4::Mp4Writer;

/// The name `--record-codec` / `KMSCUA_RECORD_CODEC` selects this backend by,
/// and the codec it reports.
pub const NAME: &str = "jetson-zc";
const NVENC_NODE: &str = "/dev/v4l2-nvenc";
/// NV12 surfaces the encoder may hold at once. NVENC keeps about one frame
/// in flight at 30 fps; the rest is headroom for a slow tick.
const SLOTS: usize = 4;
/// The first frame allocates the NvMM pools.
const FIRST_FRAME_TIMEOUT_MS: c_int = 2000;
const FRAME_TIMEOUT_MS: c_int = 1000;
/// Distinct scanout buffers to keep imported (a compositor flips between 2-3).
const IMPORT_CACHE: usize = 4;
/// Drop the imports this often, bounding how long a framebuffer id that the
/// compositor freed and reused for another buffer could show stale pixels.
const REIMPORT_EVERY_S: u64 = 2;

const JZ_FMT_BGRA: c_int = 1;
const JZ_FMT_BGRX: c_int = 2;
const JZ_FMT_RGBA: c_int = 3;
const JZ_FMT_RGBX: c_int = 4;

#[repr(C)]
struct NvBufSurface {
    _private: [u8; 0],
}

#[repr(C)]
struct JzEnc {
    _private: [u8; 0],
}

extern "C" {
    fn jz_import(fd: c_int, w: u32, h: u32, fmt: c_int, pitch: u32, block_height_log2: c_int) -> *mut NvBufSurface;
    fn jz_alloc_nv12(w: u32, h: u32) -> *mut NvBufSurface;
    fn jz_destroy(s: *mut NvBufSurface);
    fn jz_convert(src: *mut NvBufSurface, dst: *mut NvBufSurface, src_w: u32, src_h: u32) -> c_int;
    fn jz_blend_cursor(s: *mut NvBufSurface, px: *const u32, cw: c_int, ch: c_int, cx: c_int, cy: c_int, factor: c_int) -> c_int;
    fn jz_enc_open(w: u32, h: u32, fps: u32, bitrate: u32, gop: u32, nout: c_int, err: *mut c_char, errlen: usize) -> *mut JzEnc;
    fn jz_enc_close(e: *mut JzEnc);
    fn jz_enc_queue(e: *mut JzEnc, index: c_int, s: *mut NvBufSurface, pts_us: i64) -> c_int;
    fn jz_enc_reclaim(e: *mut JzEnc, timeout_ms: c_int) -> c_int;
    fn jz_enc_dequeue(e: *mut JzEnc, timeout_ms: c_int, data: *mut *const u8, len: *mut u32, pts_us: *mut i64) -> c_int;
    fn jz_enc_release(e: *mut JzEnc, index: c_int) -> c_int;
}

/// Whether the backend should be tried: the user either named it or named
/// nothing, and the NVENC node exists.
pub fn wanted(preferred: Option<&str>) -> bool {
    matches!(preferred, None | Some(NAME)) && Path::new(NVENC_NODE).exists()
}

/// Import one scanout to check the VIC can read it (a vkms or compressed
/// scanout cannot be imported). Opening the encoder is left to record start.
pub fn probe(capture: &mut Capturer) -> Result<()> {
    let shot = capture.grab_scanout(false)?;
    Surface::import(&shot)?;
    Ok(())
}

/// An NvBufSurface, allocated here or imported. An imported surface owns
/// its dma-buf fd: NvBufSurfaceDestroy closes it.
struct Surface {
    ptr: *mut NvBufSurface,
}

// NvBufSurface handles are process-wide; the recorder uses them from the one
// capture worker thread that creates them.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    fn nv12(w: u32, h: u32) -> Result<Self> {
        let ptr = unsafe { jz_alloc_nv12(w, h) };
        if ptr.is_null() {
            bail!("NvBufSurfaceCreate NV12 {w}x{h} failed");
        }
        Ok(Self { ptr })
    }

    /// Import a single-plane 32-bit RGB scanout, linear or NVIDIA block-linear.
    fn import(shot: &Scanout) -> Result<Self> {
        const NVIDIA_VENDOR: u64 = 0x03;
        let f = &shot.frame;
        let fmt = match &f.format().to_le_bytes() {
            b"AR24" => JZ_FMT_BGRA,
            b"XR24" => JZ_FMT_BGRX,
            b"AB24" => JZ_FMT_RGBA,
            b"XB24" => JZ_FMT_RGBX,
            _ => bail!("unsupported scanout format {:#x}", f.format()),
        };
        let m = f.modifier();
        let block_height_log2 = if m == 0 {
            -1
        } else if m >> 56 == NVIDIA_VENDOR && m & 0x10 != 0 {
            // Compressed block-linear (bits 23..25) is not readable by the VIC.
            if (m >> 23) & 0x7 != 0 {
                bail!("compressed scanout modifier {m:#x}");
            }
            (m & 0xf) as c_int
        } else {
            bail!("unsupported scanout modifier {m:#x}");
        };
        let fd = dup(f.dma_buf_fd())?;
        let ptr = unsafe { jz_import(fd.as_raw_fd(), f.width(), f.height(), fmt, f.stride(), block_height_log2) };
        if ptr.is_null() {
            bail!("NvBufSurfaceImport failed ({}x{}, modifier {m:#x})", f.width(), f.height());
        }
        // Closing it here as well would close whatever reused the number
        // (seen: libnvbufsurface's own /dev/nvmap fd).
        let _ = fd.into_raw_fd();
        Ok(Self { ptr })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe { jz_destroy(self.ptr) };
    }
}

/// Duplicate a dma-buf fd for NvBufSurfaceImport, numbered from
/// `IMPORT_FD_BASE` up.
///
/// libnvbufsurface keeps a table keyed by fd *number*, and libnvv4l2 leaves
/// stale entries behind when it closes its own buffers. Importing an fd that
/// reuses such a number returns the stale surface (memType NVBUF_MEM_HANDLE,
/// pitch layout), which the VIC rejects and whose teardown corrupts the heap.
/// NvMM takes the lowest free numbers, so imports stay clear of them up here.
fn dup(fd: i32) -> Result<OwnedFd> {
    const IMPORT_FD_BASE: i32 = 512;
    let mut n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, IMPORT_FD_BASE) };
    if n < 0 {
        // RLIMIT_NOFILE below the base: take any number.
        n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    }
    if n < 0 {
        bail!("dup dma-buf: {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

/// What identifies a scanout buffer across grabs. Not the dma-buf: on this
/// driver every export is a new one (and inode numbers even move between
/// framebuffers). The KMS framebuffer id is stable while the compositor
/// flips between its buffers; a reused id is covered by the periodic
/// re-import (`REIMPORT_EVERY_S`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct BufKey {
    fb_id: u32,
    geometry: (u32, u32, u32, u32, u64),
}

impl BufKey {
    fn of(shot: &Scanout) -> Self {
        let f = &shot.frame;
        Self {
            fb_id: f.fb_id(),
            geometry: (f.width(), f.height(), f.stride(), f.format(), f.modifier()),
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub frames: usize,
    pub latency_ms_mean: f32,
    pub latency_ms_max: f32,
    pub convert_ms_mean: f32,
}

pub struct ZeroCopyEncoder {
    enc: *mut JzEnc,
    width: u32,
    height: u32,
    factor: u32,
    fps: u32,
    slots: Vec<Surface>,
    free: Vec<usize>,
    /// Grab time of each queued frame, in encode order.
    pending: VecDeque<Instant>,
    imports: VecDeque<(BufKey, Arc<Surface>)>,
    last: Option<(Arc<Surface>, Option<(kmscua_proto::CursorInfo, Vec<u32>)>)>,
    queued: u64,
    mux: Option<Mp4Writer>,
    first_out: bool,
    latency_sum: f64,
    latency_max: f32,
    convert_sum: f64,
}

// The V4L2 handle is only used by the capture worker that owns the recorder.
unsafe impl Send for ZeroCopyEncoder {}

impl ZeroCopyEncoder {
    /// Encode `width` x `height` (the scanout's top-left `width*factor` x
    /// `height*factor`, downscaled by the VIC) to an MP4 at `path`.
    pub fn create(path: &Path, width: u32, height: u32, factor: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        // The surfaces must exist before the encoder opens: libnvv4l2 does not
        // recognise dma-bufs of NvBufSurfaces created later (it then reads them
        // as raw buffers and fails with "BlockSide error 0x4").
        let slots = (0..SLOTS).map(|_| Surface::nv12(width, height)).collect::<Result<Vec<_>>>()?;
        let mut err = [0 as c_char; 256];
        let gop = (fps * 2).max(1);
        let enc = unsafe {
            jz_enc_open(width, height, fps, bitrate_kbps * 1000, gop, SLOTS as c_int, err.as_mut_ptr(), err.len())
        };
        if enc.is_null() {
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            bail!("open NVENC: {msg}");
        }
        let me = Self {
            enc,
            width,
            height,
            factor: factor.max(1),
            fps,
            slots,
            free: (0..SLOTS).rev().collect(),
            pending: VecDeque::new(),
            imports: VecDeque::new(),
            last: None,
            queued: 0,
            mux: None,
            first_out: true,
            latency_sum: 0.0,
            latency_max: 0.0,
            convert_sum: 0.0,
        };
        let mut me = me;
        me.mux = Some(Mp4Writer::create(path, width, height, fps)?);
        Ok(me)
    }

    /// Encode one frame. `None` repeats the previous scanout (a failed grab),
    /// so video time keeps pace with wall time; false when there is none yet.
    pub fn push(&mut self, shot: Option<&Scanout>) -> Result<bool> {
        let (src, cursor) = match shot {
            Some(s) => (self.import(s)?, s.cursor.clone()),
            None => match &self.last {
                Some((src, cursor)) => (src.clone(), cursor.clone()),
                None => return Ok(false),
            },
        };
        let grabbed = shot.map(|s| s.grabbed).unwrap_or_else(Instant::now);
        let slot = self.free_slot()?;
        let dst = self.slots[slot].ptr;
        let t0 = Instant::now();
        if unsafe { jz_convert(src.ptr, dst, self.width * self.factor, self.height * self.factor) } != 0 {
            self.free.push(slot);
            bail!("NvBufSurfTransform failed");
        }
        self.convert_sum += t0.elapsed().as_secs_f64() * 1000.0;
        if let Some((c, px)) = &cursor {
            let r = unsafe {
                jz_blend_cursor(dst, px.as_ptr(), c.width as c_int, c.height as c_int, c.x, c.y, self.factor as c_int)
            };
            if r != 0 {
                log::debug!("jetson: cursor blend failed");
            }
        }
        let pts_us = (self.queued * 1_000_000 / self.fps as u64) as i64;
        if unsafe { jz_enc_queue(self.enc, slot as c_int, dst, pts_us) } != 0 {
            self.free.push(slot);
            bail!("NVENC QBUF: {}", std::io::Error::last_os_error());
        }
        self.queued += 1;
        self.pending.push_back(grabbed);
        self.last = Some((src, cursor));
        self.drain(0)?;
        Ok(true)
    }

    /// Wait for every queued frame's bitstream and write the MP4 index.
    pub fn finish(mut self) -> Result<Stats> {
        while !self.pending.is_empty() {
            if !self.pull(FIRST_FRAME_TIMEOUT_MS)? {
                bail!("NVENC did not return {} frame(s)", self.pending.len());
            }
        }
        let mux = self.mux.take().ok_or_else(|| anyhow!("already finished"))?;
        let frames = mux.frames();
        mux.finish()?;
        let n = frames.max(1) as f64;
        Ok(Stats {
            frames,
            latency_ms_mean: (self.latency_sum / n) as f32,
            latency_ms_max: self.latency_max,
            convert_ms_mean: (self.convert_sum / self.queued.max(1) as f64) as f32,
        })
    }

    fn import(&mut self, shot: &Scanout) -> Result<Arc<Surface>> {
        if self.queued % (REIMPORT_EVERY_S * self.fps as u64) == 0 {
            self.imports.clear();
        }
        let key = BufKey::of(shot);
        if let Some(i) = self.imports.iter().position(|(k, _)| *k == key) {
            let hit = self.imports.remove(i).expect("index in range");
            let s = hit.1.clone();
            self.imports.push_front(hit);
            return Ok(s);
        }
        let s = Arc::new(Surface::import(shot)?);
        self.imports.push_front((key, s.clone()));
        self.imports.truncate(IMPORT_CACHE);
        Ok(s)
    }

    fn free_slot(&mut self) -> Result<usize> {
        if let Some(s) = self.free.pop() {
            return Ok(s);
        }
        let timeout = if self.first_out { FIRST_FRAME_TIMEOUT_MS } else { FRAME_TIMEOUT_MS };
        match unsafe { jz_enc_reclaim(self.enc, timeout) } {
            i if i >= 0 && (i as usize) < SLOTS => Ok(i as usize),
            -1 => bail!("NVENC did not release an input surface"),
            _ => bail!("NVENC output DQBUF: {}", std::io::Error::last_os_error()),
        }
    }

    /// Collect finished input slots and bitstream without blocking.
    fn drain(&mut self, timeout_ms: c_int) -> Result<()> {
        loop {
            match unsafe { jz_enc_reclaim(self.enc, 0) } {
                i if i >= 0 && (i as usize) < SLOTS => self.free.push(i as usize),
                -1 => break,
                _ => bail!("NVENC output DQBUF: {}", std::io::Error::last_os_error()),
            }
        }
        while !self.pending.is_empty() && self.pull(timeout_ms)? {}
        Ok(())
    }

    /// Move one access unit into the MP4. False on timeout.
    fn pull(&mut self, timeout_ms: c_int) -> Result<bool> {
        let (mut data, mut len, mut pts) = (std::ptr::null(), 0u32, 0i64);
        let index = unsafe { jz_enc_dequeue(self.enc, timeout_ms, &mut data, &mut len, &mut pts) };
        match index {
            -1 => return Ok(false),
            i if i < 0 => bail!("NVENC capture DQBUF: {}", std::io::Error::last_os_error()),
            _ => {}
        }
        // SAFETY: the capture buffer stays mapped and ours until jz_enc_release.
        let au = unsafe { std::slice::from_raw_parts(data, len as usize) };
        let written = self.mux.as_mut().map(|m| m.write_annexb(au)).unwrap_or(Ok(()));
        unsafe { jz_enc_release(self.enc, index) };
        written?;
        self.first_out = false;
        if let Some(t) = self.pending.pop_front() {
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            self.latency_sum += ms as f64;
            self.latency_max = self.latency_max.max(ms);
        }
        Ok(true)
    }
}

impl Drop for ZeroCopyEncoder {
    fn drop(&mut self) {
        // Stop streaming before the surfaces it may still read are destroyed.
        unsafe { jz_enc_close(self.enc) };
    }
}
