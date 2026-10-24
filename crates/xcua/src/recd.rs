//! Recording across CLI invocations: a detached recorder process.
//!
//! `xcua record start` spawns `xcua --display :N record start --foreground`
//! in its own session (so a closing shell does not take it down) and returns
//! once it records. The recorder owns an `XBackend` and its stills
//! `Recorder`, and keeps these in the state dir:
//!
//! - `record.pid`: its pid and start time; signals are sent only to that
//!   process (the same check as the Xvfb pidfile).
//! - `record.json`: its `RecordInfo`, wall-clock start and display,
//!   rewritten every second and once more, final, when it ends.
//! - `record.timeline.jsonl`: one `{"wall_ms", "what"}` line per action verb
//!   and per `record mark`, appended by those other processes. At the end
//!   the recorder turns it into the manifest's timeline, as the MCP layer
//!   does for its own tool calls.
//! - `record.log`: its stderr.
//!
//! `record stop` sends SIGTERM and waits for the final `record.json`. The
//! recorder also ends by itself at `max_seconds`, the byte budget or low
//! disk (see `record.rs`); `record stop` then just reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use kmscua_mcp::{video, Backend};
use kmscua_proto::{RecordInfo, Request, Status};
use serde::{Deserialize, Serialize};

use crate::xvfb::{Proc, State};
use crate::XBackend;

/// What `record.json` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecState {
    pub info: RecordInfo,
    pub display: String,
    /// Wall clock at the first still, unix milliseconds.
    pub started_ms: u64,
    /// Set once the recorder has ended.
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub timeline: Vec<(f32, String)>,
}

/// The `record start` options.
#[derive(Debug, Clone, Default)]
pub struct StartOpts {
    pub name: Option<String>,
    pub fps: Option<u32>,
    pub max_side: Option<u32>,
    pub max_seconds: Option<u32>,
}

fn pid_path(st: &State) -> PathBuf {
    st.dir.join("record.pid")
}
fn json_path(st: &State) -> PathBuf {
    st.dir.join("record.json")
}
fn timeline_path(st: &State) -> PathBuf {
    st.dir.join("record.timeline.jsonl")
}
fn log_path(st: &State) -> PathBuf {
    st.dir.join("record.log")
}

fn now_ms() -> u64 {
    video::unix_ms(SystemTime::now())
}

fn write_json(st: &State, s: &RecState) -> Result<()> {
    let p = json_path(st);
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(s)?).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))
}

/// The last recording's state, running or not.
pub fn read(st: &State) -> Option<RecState> {
    serde_json::from_slice(&fs::read(json_path(st)).ok()?).ok()
}

/// The recorder process, if one is alive.
pub fn running(st: &State) -> Option<Proc> {
    let p = Proc::parse(&fs::read_to_string(pid_path(st)).ok()?)?;
    p.is_same().then_some(p)
}

/// Stamp an action into the running recording's timeline (no-op when none
/// runs). Best effort: a failed stamp never fails the action.
pub fn stamp(st: &State, what: &str) {
    if running(st).is_none() {
        return;
    }
    use std::io::Write;
    let line = serde_json::json!({ "wall_ms": now_ms(), "what": what }).to_string();
    let r = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(timeline_path(st))
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = r {
        log::warn!("timeline stamp: {e}");
    }
}

/// The timeline file as (seconds since `started_ms`, what), in order.
fn read_timeline(st: &State, started_ms: u64) -> Vec<(f32, String)> {
    let Ok(text) = fs::read_to_string(timeline_path(st)) else { return Vec::new() };
    let mut v: Vec<(f32, String)> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|j| {
            let ms = j["wall_ms"].as_u64()?;
            let what = j["what"].as_str()?.to_string();
            Some((ms.saturating_sub(started_ms) as f32 / 1000.0, what))
        })
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v
}

/// Spawn the detached recorder and wait until it records.
pub fn start(st: &State, display: &str, opts: &StartOpts) -> Result<RecState> {
    let lock = st.lock()?;
    if let Some(p) = running(st) {
        let path = read(st).and_then(|s| s.info.path).unwrap_or_else(|| "?".into());
        bail!("already recording {path} (recorder pid {}); `xcua record stop` first", p.pid);
    }
    for p in [pid_path(st), json_path(st), timeline_path(st)] {
        let _ = fs::remove_file(p);
    }
    let exe = std::env::current_exe().context("find the xcua binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("--state-dir").arg(&st.dir).args(["--display", display, "record", "start", "--foreground"]);
    if let Some(n) = &opts.name {
        cmd.args(["--name", n]);
    }
    if let Some(f) = opts.fps {
        cmd.args(["--fps", &f.to_string()]);
    }
    if let Some(m) = opts.max_side {
        cmd.args(["--max-side", &m.to_string()]);
    }
    if let Some(m) = opts.max_seconds {
        cmd.args(["--max-seconds", &m.to_string()]);
    }
    let log = fs::File::create(log_path(st)).with_context(|| format!("create {}", log_path(st).display()))?;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log));
    // Its own session: no SIGHUP when the agent's shell goes, no signals
    // meant for this command's process group.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("start the recorder process")?;
    let me = Proc::now(child.id()).ok_or_else(|| anyhow!("recorder pid out of range"))?;
    fs::write(pid_path(st), me.line()).with_context(|| format!("write {}", pid_path(st).display()))?;
    drop(lock);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(s) = read(st).filter(|s| s.info.recording) {
            // Not waited for: it runs on after we exit, and init reaps it.
            drop(child);
            return Ok(s);
        }
        if let Some(status) = child.try_wait()? {
            let _ = fs::remove_file(pid_path(st));
            let log = fs::read_to_string(log_path(st)).unwrap_or_default();
            let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            bail!("the recorder exited ({status}) before recording: {last}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(pid_path(st));
            bail!("the recorder did not start within 15 s; see {}", log_path(st).display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The recorder itself: record until SIGTERM/SIGINT/SIGHUP or until the
/// recording ends on its own, then write the manifest and the final state.
pub fn foreground(st: &State, display: &str, opts: &StartOpts) -> Result<RecState> {
    // Block the stop signals before any thread exists, so the recorder
    // thread inherits the mask and sigtimedwait here is the one place they
    // arrive.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    if running(st).is_none_or(|p| p.pid != std::process::id()) {
        // Run by hand rather than by `record start`: claim the pidfile.
        if let Some(p) = running(st) {
            bail!("already recording (recorder pid {})", p.pid);
        }
        let me = Proc::now(std::process::id()).ok_or_else(|| anyhow!("own pid out of range"))?;
        fs::write(pid_path(st), me.line())?;
    }
    let result = record_until_stopped(st, display, opts, &set);
    let _ = fs::remove_file(pid_path(st));
    result
}

fn record_until_stopped(st: &State, display: &str, opts: &StartOpts, set: &libc::sigset_t) -> Result<RecState> {
    let backend = XBackend::open(display, false, &st.recordings())?;
    let started = SystemTime::now();
    let reply = backend.call(&Request::RecordStart {
        name: opts.name.clone(),
        fps: opts.fps,
        max_side: opts.max_side,
        bitrate_kbps: None,
        max_seconds: opts.max_seconds,
        cursor: Some(true),
    })?;
    let mut state = RecState {
        info: reply.data()?,
        display: display.to_string(),
        started_ms: video::unix_ms(started),
        manifest: None,
        timeline: Vec::new(),
    };
    write_json(st, &state)?;
    let tick = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    loop {
        let sig = unsafe { libc::sigtimedwait(set, std::ptr::null_mut(), &tick) };
        if sig > 0 {
            log::info!("recorder: signal {sig}, stopping");
            break;
        }
        state.info = backend.call(&Request::RecordStatus)?.data()?;
        let _ = write_json(st, &state);
        if !state.info.recording {
            break;
        }
    }
    state.info = backend.call(&Request::RecordStop)?.data()?;
    let path = PathBuf::from(state.info.path.clone().unwrap_or_default());
    state.timeline = read_timeline(st, state.started_ms);
    let source = backend
        .call(&Request::Status)
        .and_then(|r| r.data::<Status>())
        .map(|s| (s.desktop.width, s.desktop.height))
        .unwrap_or((0, 0));
    let manifest = video::manifest_from(&state.info, started, source, &state.timeline);
    video::write_manifest(&path, &manifest)?;
    state.manifest = Some(video::manifest_path(&path).display().to_string());
    write_json(st, &state)?;
    Ok(state)
}

/// Stop the running recorder and return its final state.
pub fn stop(st: &State) -> Result<RecState> {
    let Some(p) = running(st) else {
        match read(st) {
            Some(s) if s.info.recording => bail!(
                "the recorder died without finishing (see {}); its stills so far are in {}",
                log_path(st).display(),
                s.info.path.unwrap_or_default()
            ),
            _ => bail!("not recording"),
        }
    };
    unsafe { libc::kill(p.pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(30);
    while p.is_same() {
        if Instant::now() > deadline {
            bail!("the recorder (pid {}) did not stop within 30 s; see {}", p.pid, log_path(st).display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    match read(st) {
        Some(s) if s.manifest.is_some() => Ok(s),
        _ => bail!("the recorder stopped without writing its final state; see {}", log_path(st).display()),
    }
}

/// Add a note to the running recording's timeline.
pub fn mark(st: &State, note: &str) -> Result<()> {
    if running(st).is_none() {
        bail!("not recording");
    }
    stamp(st, &format!("mark: {note}"));
    Ok(())
}

/// The recording `path` names, or the last one.
pub fn recording_path(st: &State, path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p.to_path_buf());
    }
    read(st)
        .and_then(|s| s.info.path)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("no recording yet; pass --path or run `xcua record start` first"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_is_relative_and_sorted() {
        let dir = std::env::temp_dir().join(format!("xcua-recd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let st = State { dir: dir.clone() };
        fs::write(
            timeline_path(&st),
            "{\"wall_ms\": 2500, \"what\": \"click [1, 2]\"}\nnot json\n{\"wall_ms\": 1200, \"what\": \"mark: a\"}\n",
        )
        .unwrap();
        let t = read_timeline(&st, 1000);
        assert_eq!(t, vec![(0.2, "mark: a".to_string()), (1.5, "click [1, 2]".to_string())]);
        // No recorder: stamps are dropped, mark and stop refuse.
        stamp(&st, "ignored");
        assert_eq!(read_timeline(&st, 1000).len(), 2);
        assert!(mark(&st, "x").is_err());
        assert!(format!("{:#}", stop(&st).unwrap_err()).contains("not recording"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
