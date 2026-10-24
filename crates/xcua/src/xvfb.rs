//! The X server's lifecycle: find one that answers, or start an Xvfb, and
//! leave a note where the agent's other processes can read it.
//!
//! State lives in one directory (`--state-dir`, else `$XCUA_STATE_DIR`,
//! else `$XDG_STATE_HOME/xcua`, else `~/.local/state/xcua`):
//!
//! - `env`: `DISPLAY=:N`, shell-sourceable (`. ~/.local/state/xcua/env`).
//!   Written whenever xcua is up, attached or started.
//! - `xvfb.pid`: the Xvfb's pid, only when xcua started it.
//! - `owner`: `up`, or `mcp <pid>` for an Xvfb an `xcua mcp` started.
//! - `xvfb.log`: the Xvfb's stderr.
//! - `recordings/`: stills recordings.
//!
//! Which display `up` picks, first that answers wins: `--display`, the
//! display in the state dir when its Xvfb is alive, `$DISPLAY`, else a new
//! Xvfb. A new Xvfb picks its own number (`-displayfd`), listens on no TCP
//! port and runs in its own session, so it outlives the process that
//! started it.
//!
//! The ownership rule: an Xvfb started by `xcua up` stays until `xcua
//! down`. One that `xcua mcp` had to start itself is stopped when that
//! `mcp` exits (stdin closed, SIGTERM, SIGINT, SIGHUP), unless `xcua up`
//! adopted it in the meantime. Run `xcua up` first when the apps the agent
//! starts must outlive the MCP server. A display xcua attached to and did
//! not start is never stopped.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// Where xcua keeps its notes.
#[derive(Clone, Debug)]
pub struct State {
    pub dir: PathBuf,
}

/// Who asked for the Xvfb, recorded in `owner`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Up,
    Mcp(u32),
}

impl Owner {
    fn as_line(&self) -> String {
        match self {
            Owner::Up => "up".into(),
            Owner::Mcp(pid) => format!("mcp {pid}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "up" {
            return Some(Owner::Up);
        }
        s.strip_prefix("mcp ").and_then(|p| p.trim().parse().ok()).map(Owner::Mcp)
    }
}

/// What `up` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Up {
    /// A display xcua did not start answered.
    Attached { display: String },
    /// xcua's own Xvfb from earlier is still alive.
    Reused { display: String, pid: u32 },
    Started { display: String, pid: u32 },
}

impl Up {
    pub fn display(&self) -> &str {
        match self {
            Up::Attached { display } | Up::Reused { display, .. } | Up::Started { display, .. } => display,
        }
    }
}

impl State {
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir.or_else(|| std::env::var_os("XCUA_STATE_DIR").map(PathBuf::from)) {
            Some(d) => d,
            None => match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x).join("xcua"),
                None => {
                    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("neither HOME nor --state-dir set"))?;
                    PathBuf::from(home).join(".local/state/xcua")
                }
            },
        };
        Ok(Self { dir })
    }

    pub fn env_path(&self) -> PathBuf {
        self.dir.join("env")
    }
    pub fn pid_path(&self) -> PathBuf {
        self.dir.join("xvfb.pid")
    }
    fn owner_path(&self) -> PathBuf {
        self.dir.join("owner")
    }
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("xvfb.log")
    }
    pub fn recordings(&self) -> PathBuf {
        self.dir.join("recordings")
    }

    /// The display in `env`, if any.
    pub fn display(&self) -> Option<String> {
        let text = fs::read_to_string(self.env_path()).ok()?;
        text.lines()
            .find_map(|l| l.trim().strip_prefix("DISPLAY="))
            .map(|d| d.trim().trim_matches('"').to_string())
            .filter(|d| !d.is_empty())
    }

    pub fn pid(&self) -> Option<u32> {
        fs::read_to_string(self.pid_path()).ok()?.trim().parse().ok()
    }

    pub fn owner(&self) -> Option<Owner> {
        Owner::parse(&fs::read_to_string(self.owner_path()).ok()?)
    }

    fn write(&self, display: &str, pid: Option<u32>, owner: Option<Owner>) -> Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        write_atomic(&self.env_path(), &format!("DISPLAY={display}\n"))?;
        match pid {
            Some(p) => write_atomic(&self.pid_path(), &format!("{p}\n"))?,
            None => remove(&self.pid_path()),
        }
        match owner {
            Some(o) => write_atomic(&self.owner_path(), &format!("{}\n", o.as_line()))?,
            None => remove(&self.owner_path()),
        }
        Ok(())
    }

    fn clear(&self) {
        remove(&self.env_path());
        remove(&self.pid_path());
        remove(&self.owner_path());
    }

    /// xcua's own Xvfb from the state dir, if it is alive and answers.
    pub fn live(&self) -> Option<(String, u32)> {
        let (display, pid) = (self.display()?, self.pid()?);
        (is_xvfb(pid) && answers(&display)).then_some((display, pid))
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Whether an X server answers on `display` (a full connection setup).
pub fn answers(display: &str) -> bool {
    !display.is_empty() && x11rb::rust_connection::RustConnection::connect(Some(display)).is_ok()
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// `pid` is alive and is an Xvfb, not a recycled pid.
fn is_xvfb(pid: u32) -> bool {
    alive(pid)
        && fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| c.trim() == "Xvfb")
            .unwrap_or(true)
}

/// Start an Xvfb of `size` and wait until it answers. It picks its own
/// display number, writes stderr to `log` (or nowhere) and runs in its own
/// session. The caller owns the `Child`; dropping it does not stop Xvfb.
pub fn spawn(size: (u32, u32), log: Option<&Path>) -> Result<(Child, String)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe for -displayfd");
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let wr_raw = fds[1];
    let stderr = match log {
        Some(p) => {
            if let Some(d) = p.parent() {
                let _ = fs::create_dir_all(d);
            }
            Stdio::from(fs::File::create(p).with_context(|| format!("create {}", p.display()))?)
        }
        None => Stdio::null(),
    };
    let mut cmd = Command::new("Xvfb");
    cmd.args(["-displayfd", "3", "-screen", "0"])
        .arg(format!("{}x{}x24", size.0, size.1))
        // -s 0: no screen saver, which would blank the root after 10 minutes.
        .args(["-nolisten", "tcp", "-noreset", "+extension", "GLX", "+render", "-s", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    unsafe {
        cmd.pre_exec(move || {
            // Own session: no SIGHUP from our terminal, no group signals.
            libc::setsid();
            // dup2 onto itself keeps O_CLOEXEC, so clear it by hand then.
            let r = if wr_raw == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(wr_raw, 3) };
            if r < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("start Xvfb (is it installed? apt install xvfb)")?;
    drop(wr);
    let mut line = String::new();
    let read = {
        let mut r = BufReader::new(fs::File::from(rd));
        r.read_line(&mut line)
    };
    let number = line.trim();
    if read.is_err() || number.is_empty() || number.parse::<u32>().is_err() {
        let status = child.try_wait().ok().flatten();
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "Xvfb exited before reporting a display ({}){}",
            status.map(|s| s.to_string()).unwrap_or_else(|| "no display number".into()),
            log.map(|p| format!("; see {}", p.display())).unwrap_or_default()
        );
    }
    let display = format!(":{number}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !answers(&display) {
        if Instant::now() > deadline || child.try_wait().ok().flatten().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Xvfb on {display} did not answer within 10 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((child, display))
}

/// Make sure a display answers, starting an Xvfb if none does (see the
/// module doc for the order and the ownership rule).
pub fn up(state: &State, display: Option<&str>, size: (u32, u32), owner: Owner) -> Result<Up> {
    if let Some(d) = display {
        if !answers(d) {
            bail!("no X server answers on {d}");
        }
        if state.live().is_some_and(|(sd, _)| sd == d) {
            return reuse(state, owner);
        }
        state.write(d, None, None)?;
        return Ok(Up::Attached { display: d.to_string() });
    }
    if state.live().is_some() {
        return reuse(state, owner);
    }
    // Whatever the state dir said is stale now.
    state.clear();
    if let Some(d) = std::env::var("DISPLAY").ok().filter(|d| answers(d)) {
        state.write(&d, None, None)?;
        return Ok(Up::Attached { display: d });
    }
    let (child, display) = spawn(size, Some(&state.log_path()))?;
    let pid = child.id();
    // Not waited for: it outlives us, and init reaps it.
    drop(child);
    state.write(&display, Some(pid), Some(owner))?;
    Ok(Up::Started { display, pid })
}

/// `up` adopts an mcp-owned Xvfb so it stays; `mcp` leaves ownership alone.
fn reuse(state: &State, owner: Owner) -> Result<Up> {
    let (display, pid) = state.live().ok_or_else(|| anyhow!("xcua's Xvfb went away"))?;
    if owner == Owner::Up && state.owner() != Some(Owner::Up) {
        state.write(&display, Some(pid), Some(Owner::Up))?;
    }
    Ok(Up::Reused { display, pid })
}

/// Stop xcua's Xvfb (if it started one) and forget the display.
pub fn down(state: &State) -> Result<String> {
    let msg = match (state.pid(), state.display()) {
        (Some(pid), d) if is_xvfb(pid) => {
            stop(pid);
            format!("stopped Xvfb {} (pid {pid})", d.unwrap_or_default())
        }
        (_, Some(d)) => format!("forgot {d} (not started by xcua, left running)"),
        (_, None) => "nothing to stop".to_string(),
    };
    state.clear();
    Ok(msg)
}

/// What `xcua mcp` calls on exit: stop the Xvfb only if this process
/// started it and nobody adopted it since.
pub fn release(state: &State, me: Owner) {
    if state.owner() != Some(me) {
        return;
    }
    if let Some(pid) = state.pid() {
        if is_xvfb(pid) {
            stop(pid);
        }
    }
    state.clear();
}

/// SIGTERM, then SIGKILL after 3 s.
fn stop(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while is_xvfb(pid) && Instant::now() < deadline {
        // Reap it if it is our child; harmless otherwise.
        unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        std::thread::sleep(Duration::from_millis(50));
    }
    if is_xvfb(pid) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
    }
}

/// The display for a command that needs one but must not start one:
/// `--display`, xcua's live Xvfb, then `$DISPLAY`.
pub fn resolve(state: &State, display: Option<&str>) -> Result<String> {
    if let Some(d) = display {
        return if answers(d) { Ok(d.to_string()) } else { bail!("no X server answers on {d}") };
    }
    if let Some((d, _)) = state.live() {
        return Ok(d);
    }
    if let Some(d) = state.display().filter(|d| answers(d)) {
        return Ok(d);
    }
    if let Some(d) = std::env::var("DISPLAY").ok().filter(|d| answers(d)) {
        return Ok(d);
    }
    bail!("no X display: run `xcua up` (or pass --display)")
}

/// One paragraph for `xcua status`.
pub fn status(state: &State) -> String {
    let mut s = format!("state dir: {}\n", state.dir.display());
    match state.display() {
        Some(d) => {
            s.push_str(&format!("display: {d} ({})\n", if answers(&d) { "answering" } else { "not answering" }));
            s.push_str(&format!("env file: {}\n", state.env_path().display()));
        }
        None => s.push_str("display: none (run `xcua up`)\n"),
    }
    match state.pid() {
        Some(pid) => s.push_str(&format!(
            "xvfb pid: {pid} ({}), owner: {}\n",
            if is_xvfb(pid) { "alive" } else { "gone" },
            state.owner().map(|o| o.as_line()).unwrap_or_else(|| "?".into())
        )),
        None if state.display().is_some() => s.push_str("xvfb: not started by xcua (attached)\n"),
        None => {}
    }
    s
}

/// "1920x1080" to (1920, 1080).
pub fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| anyhow!("size must be WxH, not {s}"))?;
    let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
    if !(64..=16384).contains(&w) || !(64..=16384).contains(&h) {
        bail!("size {w}x{h} out of range");
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_round_trip() {
        for o in [Owner::Up, Owner::Mcp(42)] {
            assert_eq!(Owner::parse(&o.as_line()), Some(o));
        }
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert!(parse_size("12x7").is_err());
    }
}
