//! The X server's lifecycle: find xcua's Xvfb or start one, and leave a
//! note where the agent's other processes can read it.
//!
//! State lives in one directory (`--state-dir`, else `$XCUA_STATE_DIR`,
//! else `$XDG_STATE_HOME/xcua`, else `~/.local/state/xcua`):
//!
//! - `env`: `export DISPLAY=:N`, for `. ~/.local/state/xcua/env`.
//! - `xvfb.pid`: the Xvfb's pid and its start time (field 22 of
//!   `/proc/<pid>/stat`), only when xcua started it.
//! - `nonce`: a random identity xcua sets as the `_XCUA_NONCE` property on
//!   the root window of the Xvfb it starts. A display whose root carries it
//!   is xcua's Xvfb, with or without `/proc` (bwrap `--tmpfs /proc`).
//! - `owner`: `up`, or `mcp <pid>` for an Xvfb an `xcua mcp` started.
//! - `users/<pid>`: one file per `xcua mcp` using xcua's Xvfb, holding that
//!   process's start time.
//! - `lock`: `flock`ed from every liveness check through the writes that
//!   follow, so concurrent `xcua up`s start one Xvfb between them. Taken
//!   with a 10 s deadline. Where the filesystem has no `flock`, `lock.excl`
//!   (created O_EXCL, holding pid and start time) stands in, and is broken
//!   when its holder is dead.
//!
//! Signals: with `/proc`, only when the pid is in 2..=i32::MAX, its `comm`
//! reads `Xvfb` and its start time matches the pidfile. Without `/proc`,
//! only when the pid is alive (`kill(pid, 0)`) and the display answers
//! with xcua's nonce; SIGTERM is then not escalated to SIGKILL, and success
//! means the display stopped answering. The residual risk without `/proc`:
//! the nonce proves xcua's Xvfb is alive, not that the pidfile's pid is
//! that Xvfb. They differ only if the state dir is shared between pid
//! namespaces that share the X socket (two sandboxes over one home and one
//! network namespace), where the pid may name another process.
//!
//! Every probe of a display has a 2 s deadline, so a stopped (SIGSTOP) or
//! wedged Xvfb cannot hang `up`, `down` or an exiting `mcp`. Deciding that
//! xcua's Xvfb is unresponsive takes repeated probes over 10 s, since a busy
//! sandbox (a software-rendered browser, 4K grabs) can miss one. If it
//! never answers, `up` (and an `mcp`'s re-up) kills it and starts a new one
//! only when `/proc` shows it stopped (state T); a running but silent Xvfb
//! is left alone and `up` fails with a message, so a slow moment never
//! costs the agent its session.
//! - `xvfb.log`: the Xvfb's stderr. `recordings/`: stills recordings.
//!
//! Which display a command uses: `--display :N` when given (`--display
//! env` spells out the shell's `$DISPLAY`), else xcua's live Xvfb, else a
//! display recorded by `xcua up --display`, else a new Xvfb. `$DISPLAY` is
//! never used implicitly. `--display` on any command but `up` uses that
//! display for the one run and leaves the state dir alone; `up --display`
//! records the attach, and refuses while xcua's own Xvfb is running.
//!
//! Size: `xcua up --size WxH` refuses when xcua's display already runs at
//! another size, unless `--restart` (which closes the apps on it). Commands
//! that start a display on their own (`mcp`, the verbs) start it at
//! `--size` (default 1920x1080) and otherwise use whatever runs. `doctor`
//! never starts one.
//!
//! Window manager: none by default (xcua gives the clicked window the
//! keyboard itself). `up --wm auto` starts the first installed of
//! [`WM_CANDIDATES`] on xcua's display, `--wm <cmd>` that command, `--wm
//! none` stops it. It is tracked like the Xvfb: `wm.pid` (pid and start
//! time), `wm.cmd`, its output in `wm.log`; `down` stops it, `status` and
//! `doctor` report it, and one that died is reported, never restarted
//! behind the agent's back. With a WM running, focus is the WM's (twm,
//! for one, gives it to the window under the pointer). twm is started with
//! a generated `twmrc` in the state dir: its defaults place a new window
//! interactively with the server grabbed, which stalls every screenshot
//! until someone clicks. Without `/proc` a WM is never signalled (its pid
//! cannot be told from a recycled one); it is forgotten, and ends with its
//! X server.
//!
//! The ownership rule: an Xvfb started (or adopted) by `xcua up` stays
//! until `xcua down`. One that an `xcua mcp` started is shared by every
//! `xcua mcp` that finds it, and stopped when the last live user leaves.
//! A user that died without leaving (SIGKILL) is pruned by the next one; an
//! `mcp` that finds the starter dead adopts the Xvfb. A display xcua
//! attached to and did not start is never stopped.

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::x11::Probe;

/// How long an Xvfb gets to report its display and answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a display gets to answer a probe.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a command waits for another xcua to let go of the state dir.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long xcua's own Xvfb may stay silent before `up` acts on it.
const PATIENCE: Duration = Duration::from_secs(10);
/// Screen size when nothing says otherwise.
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
/// What `--wm auto` looks for, in this order.
pub const WM_CANDIDATES: &[&str] = &["jwm", "openbox", "fluxbox", "icewm", "twm"];

/// What `xcua up` asks for beyond "a display that answers".
#[derive(Clone, Debug, Default)]
pub struct UpOpts {
    /// `--size` given explicitly: a running display of another size is
    /// refused unless `restart`.
    pub size: Option<(u32, u32)>,
    /// Stop xcua's running Xvfb (and WM) and start a new one.
    pub restart: bool,
    /// `--wm`: "auto", "none" or a command. None leaves the WM as it is.
    pub wm: Option<String>,
}

/// What the state dir's Xvfb is doing.
#[derive(Debug, PartialEq, Eq)]
pub enum Liveness {
    /// Answers, with xcua's nonce.
    Live(String, Proc),
    /// Accepts connections but never answered within the patience.
    Silent(String, Proc),
    /// Not there (or another server sits on its number).
    Gone,
}

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
    /// A display xcua did not start: named by `--display`, or recorded by
    /// an earlier `up --display`.
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

    /// Whether the display is xcua's own Xvfb (so an `mcp` is its user).
    pub fn owned(&self) -> bool {
        !matches!(self, Up::Attached { .. })
    }
}

/// A process as the pidfile names it: pid plus start time, so a recycled
/// pid is never mistaken for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub start: Option<u64>,
}

impl Proc {
    /// `pid` as it is now, if it is a pid that may be signalled at all.
    pub fn now(pid: u32) -> Option<Self> {
        valid_pid(pid as u64)?;
        Some(Self { pid, start: start_time(pid) })
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split_whitespace();
        let pid = it.next()?.parse::<u64>().ok()?;
        let pid = valid_pid(pid)? as u32;
        Some(Self { pid, start: it.next().and_then(|t| t.parse().ok()) })
    }

    pub fn line(&self) -> String {
        match self.start {
            Some(t) => format!("{} {t}\n", self.pid),
            None => format!("{}\n", self.pid),
        }
    }

    /// Alive, an Xvfb, and the same process the pidfile recorded.
    pub fn is_xvfb(&self) -> bool {
        valid_pid(self.pid as u64).is_some()
            && alive(self.pid)
            && fs::read_to_string(format!("/proc/{}/comm", self.pid)).is_ok_and(|c| c.trim() == "Xvfb")
            && self.start.is_none_or(|t| start_time(self.pid) == Some(t))
    }

    /// Alive and the same process (any program): for `users/` entries.
    pub fn is_same(&self) -> bool {
        valid_pid(self.pid as u64).is_some() && alive(self.pid) && self.start.is_none_or(|t| start_time(self.pid) == Some(t))
    }
}

/// A pid that may be passed to kill(2): not 0 (our process group), not 1
/// (init), not one that turns negative (-1 is every process we may signal).
fn valid_pid(pid: u64) -> Option<i32> {
    (2..=i32::MAX as u64).contains(&pid).then_some(pid as i32)
}

/// Field 22 of `/proc/<pid>/stat`: start time in clock ticks since boot.
fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may hold spaces and parens; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Alive, zombies excluded. A dead child of ours (an Xvfb this `mcp`
/// started through a re-up) is reaped here, so it does not linger as a
/// zombie that `kill(pid, 0)` still finds.
fn alive(pid: u32) -> bool {
    let Some(p) = valid_pid(pid as u64) else { return false };
    unsafe { libc::waitpid(p, std::ptr::null_mut(), libc::WNOHANG) };
    let exists = unsafe { libc::kill(p, 0) == 0 };
    exists && !zombie(pid)
}

/// State `Z` in `/proc/<pid>/stat` (false without /proc).
fn zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rfind(')').map(|i| s[i + 1..].trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Whether this process can see `/proc` (bwrap `--tmpfs /proc` hides it).
pub fn have_proc() -> bool {
    Path::new("/proc/self/stat").exists()
}

/// The state dir's lock, released on drop: a `flock`ed file, or an O_EXCL
/// file removed on drop where the filesystem has no `flock`.
pub enum StateLock {
    Flock(#[allow(dead_code)] fs::File),
    Excl(PathBuf),
}

impl Drop for StateLock {
    fn drop(&mut self) {
        if let StateLock::Excl(p) = self {
            remove(p);
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
    fn nonce_path(&self) -> PathBuf {
        self.dir.join("nonce")
    }
    fn wm_pid_path(&self) -> PathBuf {
        self.dir.join("wm.pid")
    }
    fn wm_cmd_path(&self) -> PathBuf {
        self.dir.join("wm.cmd")
    }
    pub fn wm_log_path(&self) -> PathBuf {
        self.dir.join("wm.log")
    }

    /// The window manager xcua started: its command, process, and whether
    /// it is still running.
    pub fn wm(&self) -> Option<(String, Proc, bool)> {
        let cmd = fs::read_to_string(self.wm_cmd_path()).ok()?.trim().to_string();
        let p = Proc::parse(&fs::read_to_string(self.wm_pid_path()).ok()?)?;
        let alive = p.is_same();
        Some((cmd, p, alive))
    }

    /// The identity of xcua's Xvfb, as set on its root window.
    pub fn nonce(&self) -> Option<String> {
        fs::read_to_string(self.nonce_path()).ok().map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
    }
    fn users_dir(&self) -> PathBuf {
        self.dir.join("users")
    }
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("xvfb.log")
    }
    pub fn recordings(&self) -> PathBuf {
        self.dir.join("recordings")
    }

    /// Take the state dir's lock, waiting at most 10 s for another xcua.
    pub fn lock(&self) -> Result<StateLock> {
        self.lock_within(LOCK_TIMEOUT)
    }

    pub fn lock_within(&self, timeout: Duration) -> Result<StateLock> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        let deadline = Instant::now() + timeout;
        let path = self.dir.join("lock");
        // std opens with O_CLOEXEC: an Xvfb started under the lock does not
        // inherit it and hold it forever.
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(StateLock::Flock(f));
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "the state dir {} is locked by another xcua (busy for {} s); is one stuck? `xcua status` shows it",
                            self.dir.display(),
                            timeout.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Some(libc::ENOLCK | libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) => {
                    log::info!("no flock on {} ({e}); using an O_EXCL lock file", path.display());
                    return self.lock_excl(deadline);
                }
                _ => return Err(e).with_context(|| format!("flock {}", path.display())),
            }
        }
    }

    /// The fallback lock: `lock.excl` created O_EXCL with our pid and start
    /// time, waited on until `deadline`, broken when its holder is dead.
    pub fn lock_excl(&self, deadline: Instant) -> Result<StateLock> {
        use std::io::Write;
        let path = self.dir.join("lock.excl");
        let me = Proc::now(std::process::id()).ok_or_else(|| anyhow!("own pid out of range"))?;
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    f.write_all(me.line().as_bytes()).with_context(|| format!("write {}", path.display()))?;
                    return Ok(StateLock::Excl(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).ok().and_then(|t| Proc::parse(&t));
                    if holder.is_some_and(|h| !h.is_same()) {
                        log::warn!("breaking the stale lock {} of dead pid {}", path.display(), holder.unwrap().pid);
                        remove(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "the state dir {} is locked by pid {} ({})",
                            self.dir.display(),
                            holder.map(|h| h.pid.to_string()).unwrap_or_else(|| "?".into()),
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
            }
        }
    }

    /// The display in `env` (`export DISPLAY=:N` or `DISPLAY=:N`).
    pub fn display(&self) -> Option<String> {
        parse_env(&fs::read_to_string(self.env_path()).ok()?)
    }

    /// The pidfile's Xvfb, if the pidfile names a pid that may be signalled.
    pub fn pid(&self) -> Option<Proc> {
        Proc::parse(&fs::read_to_string(self.pid_path()).ok()?)
    }

    pub fn owner(&self) -> Option<Owner> {
        Owner::parse(&fs::read_to_string(self.owner_path()).ok()?)
    }

    fn write(&self, display: &str, pid: Option<(Proc, &str)>, owner: Option<Owner>) -> Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        write_atomic(&self.env_path(), &format!("export DISPLAY={display}\n"))?;
        match pid {
            Some((p, nonce)) => {
                write_atomic(&self.pid_path(), &p.line())?;
                write_atomic(&self.nonce_path(), &format!("{nonce}\n"))?;
            }
            None => {
                remove(&self.pid_path());
                remove(&self.nonce_path());
            }
        }
        self.set_owner(owner)
    }

    fn set_owner(&self, owner: Option<Owner>) -> Result<()> {
        match owner {
            Some(o) => write_atomic(&self.owner_path(), &format!("{}\n", o.as_line())),
            None => {
                remove(&self.owner_path());
                Ok(())
            }
        }
    }

    fn clear(&self) {
        remove(&self.env_path());
        remove(&self.pid_path());
        remove(&self.wm_pid_path());
        remove(&self.wm_cmd_path());
        remove(&self.nonce_path());
        remove(&self.owner_path());
        let _ = fs::remove_dir_all(self.users_dir());
    }

    /// xcua's own Xvfb, if its display answers with xcua's nonce, probing
    /// for up to 10 s before calling it silent.
    pub fn live(&self) -> Option<(String, Proc)> {
        match self.check(PATIENCE) {
            Liveness::Live(d, p) => Some((d, p)),
            _ => None,
        }
    }

    /// Probe xcua's Xvfb until it answers, is gone, or `patience` runs out.
    /// Without a recorded nonce (state from an older xcua), the pid and
    /// start time stand in for it.
    pub fn check(&self, patience: Duration) -> Liveness {
        let (Some(display), Some(pid)) = (self.display(), self.pid()) else { return Liveness::Gone };
        let nonce = self.nonce();
        if nonce.is_none() && !pid.is_xvfb() {
            return Liveness::Gone;
        }
        match probe_patiently(&display, patience) {
            Probe::Answered(n) if nonce.is_none() || n == nonce => Liveness::Live(display, pid),
            Probe::Answered(_) | Probe::Absent => Liveness::Gone,
            Probe::Silent => Liveness::Silent(display, pid),
        }
    }

    /// The display a command would use without starting one: xcua's live
    /// Xvfb, or one recorded by `up --display`.
    pub fn current(&self) -> Option<String> {
        self.live().map(|(d, _)| d).or_else(|| self.attached())
    }

    /// A display recorded by `up --display` (no pidfile) that answers.
    fn attached(&self) -> Option<String> {
        if self.pid_path().exists() {
            return None;
        }
        self.display().filter(|d| answers(d))
    }

    fn add_user(&self, pid: u32) -> Result<()> {
        let dir = self.users_dir();
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let me = Proc::now(pid).ok_or_else(|| anyhow!("pid {pid} out of range"))?;
        write_atomic(&dir.join(pid.to_string()), &me.line())
    }

    fn remove_user(&self, pid: u32) {
        remove(&self.users_dir().join(pid.to_string()));
    }

    /// The `xcua mcp`s still using xcua's Xvfb; entries of dead ones are removed.
    pub fn live_users(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(self.users_dir()) else { return Vec::new() };
        let mut live = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "tmp") {
                continue;
            }
            let user = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<u32>().ok())
                .and_then(|pid| {
                    let start = fs::read_to_string(&path).ok().and_then(|t| Proc::parse(&t)).and_then(|p| p.start);
                    Some(Proc { pid: valid_pid(pid as u64)? as u32, start })
                });
            match user {
                Some(u) if u.is_same() => live.push(u.pid),
                _ => remove(&path),
            }
        }
        live.sort_unstable();
        live
    }
}

fn parse_env(text: &str) -> Option<String> {
    text.lines()
        .map(|l| l.trim())
        .find_map(|l| l.strip_prefix("export ").unwrap_or(l).trim_start().strip_prefix("DISPLAY="))
        .map(|d| d.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|d| !d.is_empty())
}

/// Write beside `path` under a name no other process uses, then rename.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display())).inspect_err(|_| remove(&tmp))
}

fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Whether an X server answers on `display`: a full connection setup
/// within 2 s, so a stopped server counts as not answering.
pub fn answers(display: &str) -> bool {
    !display.is_empty() && crate::x11::connect_within(display, ANSWER_TIMEOUT).is_ok()
}

/// Probe `display` until it answers or is gone, for at most `patience`.
fn probe_patiently(display: &str, patience: Duration) -> Probe {
    let end = Instant::now() + patience;
    loop {
        let p = crate::x11::probe(display, ANSWER_TIMEOUT);
        if p != Probe::Silent || Instant::now() >= end {
            return p;
        }
        log::info!("{display} did not answer within {} s; probing again", ANSWER_TIMEOUT.as_secs());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether `/proc` shows `pid` stopped (state T or t).
fn stopped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rfind(')').map(|i| s[i + 1..].trim_start().starts_with(['T', 't'])))
        .unwrap_or(false)
}

/// The `_XCUA_NONCE` on `display`'s root, within 2 s.
pub fn nonce_of(display: &str) -> Option<String> {
    let (conn, root) = crate::x11::connect_within(display, ANSWER_TIMEOUT).ok()?;
    crate::x11::read_nonce(&conn, root)
}

/// Start an Xvfb of `size` and wait until it answers, at most
/// [`START_TIMEOUT`] in all. It picks its own display number, writes
/// stderr to `log` (or nowhere) and runs in its own session. The caller
/// owns the `Child`; dropping it does not stop Xvfb.
pub fn spawn(size: (u32, u32), log: Option<&Path>) -> Result<(Child, String)> {
    let deadline = Instant::now() + START_TIMEOUT;
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
    let mut child = cmd
        .spawn()
        .context("start Xvfb (not installed? apt install xvfb, or see the README for an unprivileged unpack)")?;
    drop(wr);
    let fail = |child: &mut Child, why: String| -> anyhow::Error {
        let _ = child.kill();
        let _ = child.wait();
        anyhow!("{why}{}", log.map(|p| format!("; see {}", p.display())).unwrap_or_default())
    };
    let number = match read_line_until(&rd, deadline) {
        Ok(Some(line)) if line.trim().parse::<u32>().is_ok() => line.trim().to_string(),
        Ok(Some(line)) => return Err(fail(&mut child, format!("Xvfb reported {line:?} instead of a display number"))),
        Ok(None) => {
            let status = child.try_wait().ok().flatten();
            let why = match status {
                Some(s) => format!("Xvfb exited before reporting a display ({s})"),
                None => format!("Xvfb did not report a display within {} s", START_TIMEOUT.as_secs()),
            };
            return Err(fail(&mut child, why));
        }
        Err(e) => return Err(fail(&mut child, format!("read Xvfb's display number: {e}"))),
    };
    let display = format!(":{number}");
    while !answers(&display) {
        if Instant::now() > deadline || child.try_wait().ok().flatten().is_some() {
            return Err(fail(&mut child, format!("Xvfb on {display} did not answer within {} s", START_TIMEOUT.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((child, display))
}

/// One line from `fd`, waiting at most until `deadline`. None on EOF or
/// timeout without a full line.
fn read_line_until(fd: &OwnedFd, deadline: Instant) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            return Ok(Some(String::from_utf8_lossy(&buf[..i]).into_owned()));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, i32::MAX as u128) as i32) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            continue;
        }
        let mut chunk = [0u8; 64];
        let n = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        match n {
            0 => return Ok(None),
            n if n < 0 => {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            n => buf.extend_from_slice(&chunk[..n as usize]),
        }
    }
}

/// The `--display` value as a display name: `env` means the shell's
/// `$DISPLAY`, the one spelled-out way to reach it.
pub fn explicit(display: Option<&str>) -> Result<Option<String>> {
    match display {
        None => Ok(None),
        Some("env") => std::env::var("DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .map(Some)
            .ok_or_else(|| anyhow!("--display env, but $DISPLAY is not set")),
        // x11rb 0.13 maps a socket-path display ("launchd mode") to
        // /tmp/.X11-unix/X0, which would be the user's own display.
        Some(d) if d.starts_with('/') || d.starts_with("unix:") => {
            bail!("--display {d}: socket paths are not supported; use :N")
        }
        Some(d) => Ok(Some(d.to_string())),
    }
}

/// `xcua up`, or the auto-up of `mcp`/`screenshot`/`doctor` (with
/// `display` None): make sure a display answers, under the state lock (see
/// the module doc for the order and the ownership rule). An `mcp` caller
/// (`Owner::Mcp`) is registered as a user of xcua's Xvfb.
pub fn up(state: &State, display: Option<&str>, size: (u32, u32), owner: Owner) -> Result<Up> {
    up_with(state, display, owner, size, &UpOpts::default()).map(|(up, _)| up)
}

/// `up` with the size check, `--restart` and `--wm` (see the module doc).
/// `start_size` is used when a display has to be started and `opts.size`
/// does not say. Answers what happened and notes for the user (the WM).
pub fn up_with(state: &State, display: Option<&str>, owner: Owner, start_size: (u32, u32), opts: &UpOpts) -> Result<(Up, Vec<String>)> {
    let _lock = state.lock()?;
    let size = opts.size.unwrap_or(start_size);
    // A restart brings back the WM that ran, unless --wm says otherwise.
    let mut wm_want = opts.wm.clone();
    if let Some(d) = display {
        if opts.restart {
            bail!("--restart restarts xcua's own Xvfb; {d} is not xcua's to restart");
        }
        if opts.wm.as_deref().is_some_and(|w| w != "none") {
            bail!("--wm starts a window manager on xcua's own Xvfb only, not on {d}");
        }
        match state.check(PATIENCE) {
            Liveness::Live(own, p) if own == d => return reuse(state, owner, own, p).map(|u| (u, Vec::new())),
            Liveness::Live(own, p) | Liveness::Silent(own, p) => bail!(
                "xcua's own Xvfb {own} (pid {}) is running for {}; run `xcua down` first, or use --display without `up`",
                p.pid,
                state.dir.display()
            ),
            Liveness::Gone => {}
        }
        if !answers(d) {
            bail!("no X server answers on {d}");
        }
        state.clear();
        state.write(d, None, None)?;
        return Ok((Up::Attached { display: d.to_string() }, Vec::new()));
    }
    match state.check(PATIENCE) {
        Liveness::Live(d, p) => {
            let running = display_size(&d);
            let mismatch = opts.size.zip(running).filter(|(want, have)| want != have);
            if let (Some(((ww, wh), (hw, hh))), false) = (mismatch, opts.restart) {
                bail!(
                    "xcua's display {d} is running at {hw}x{hh}; `xcua up --size {ww}x{wh} --restart` restarts it at {ww}x{wh} (apps on it will close)"
                );
            }
            if !opts.restart {
                let up = reuse(state, owner, d.clone(), p)?;
                let notes = apply_wm(state, &d, opts.wm.as_deref())?;
                return Ok((up, notes));
            }
            let nonce = state.nonce();
            if wm_want.is_none() {
                wm_want = state.wm().filter(|(_, _, alive)| *alive).map(|(cmd, _, _)| cmd);
            }
            stop_wm(state);
            if stop(&p, Some(&d), nonce.as_deref()) == Stopped::StillAnswering {
                bail!("xcua's Xvfb on {d} (pid {}) did not stop; not restarting", p.pid);
            }
            state.clear();
        }
        Liveness::Silent(d, p) => {
            // Never on one missed probe, never on a running process: only a
            // stopped Xvfb that /proc confirms is xcua's is replaced.
            if have_proc() && p.is_xvfb() && stopped(p.pid) {
                log::warn!("xcua's Xvfb on {d} (pid {}) is stopped and has not answered for {} s; killing it", p.pid, PATIENCE.as_secs());
                stop(&p, None, None);
            } else {
                bail!(
                    "xcua's Xvfb on {d} (pid {}) has not answered for {} s but is not stopped (busy?); not starting a second one. Retry, or `xcua down` to replace it",
                    p.pid,
                    PATIENCE.as_secs()
                );
            }
        }
        Liveness::Gone => {}
    }
    if let Some(d) = state.attached() {
        if opts.restart || opts.wm.as_deref().is_some_and(|w| w != "none") {
            bail!("{d} was attached with `up --display`, not started by xcua; `xcua down` first");
        }
        return Ok((Up::Attached { display: d }, Vec::new()));
    }
    // Whatever the state dir said is stale now. A WM that outlived its
    // display goes with it.
    stop_wm(state);
    state.clear();
    let (mut child, display) = spawn(size, Some(&state.log_path()))?;
    let proc = Proc::now(child.id()).ok_or_else(|| anyhow!("Xvfb pid {} out of range", child.id()))?;
    let nonce = crate::x11::new_nonce();
    let marked = crate::x11::connect_within(&display, ANSWER_TIMEOUT)
        .and_then(|(conn, root)| crate::x11::write_nonce(&conn, root, &nonce));
    if let Err(e) = marked {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.context(format!("mark the new Xvfb on {display}")));
    }
    // Not waited for: it outlives us, and init reaps it.
    drop(child);
    state.write(&display, Some((proc, &nonce)), Some(owner))?;
    if let Owner::Mcp(me) = owner {
        state.add_user(me)?;
    }
    let notes = apply_wm(state, &display, wm_want.as_deref())?;
    Ok((Up::Started { display, pid: proc.pid }, notes))
}

/// The size of `display`'s root window, within 2 s.
pub fn display_size(display: &str) -> Option<(u32, u32)> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, root) = crate::x11::connect_within(display, ANSWER_TIMEOUT).ok()?;
    let g = conn.get_geometry(root).ok()?.reply().ok()?;
    Some((g.width as u32, g.height as u32))
}

/// The first of [`WM_CANDIDATES`] on PATH.
pub fn find_wm() -> Option<&'static str> {
    WM_CANDIDATES.iter().copied().find(|w| on_path(w).is_some())
}

/// `bin`'s full path if it is on PATH.
pub fn on_path(bin: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(bin)).find(|p| p.is_file())
}

/// Bring the WM in line with `--wm` (None: leave it, and say if it died).
fn apply_wm(state: &State, display: &str, want: Option<&str>) -> Result<Vec<String>> {
    let current = state.wm();
    let Some(want) = want else {
        return Ok(match current {
            Some((cmd, p, false)) => vec![format!(
                "window manager {cmd} (pid {}) has died; see {}. `xcua up --wm auto` starts one again",
                p.pid,
                state.wm_log_path().display()
            )],
            _ => Vec::new(),
        });
    };
    if want == "none" {
        return Ok(stop_wm(state).into_iter().collect());
    }
    let cmd = if want == "auto" {
        find_wm()
            .ok_or_else(|| anyhow!("--wm auto: no window manager installed (looked for {})", WM_CANDIDATES.join(", ")))?
            .to_string()
    } else {
        want.to_string()
    };
    let cmd = if cmd == "twm" { twm_with_rc(state)? } else { cmd };
    if let Some((old, p, true)) = &current {
        if *old == cmd {
            return Ok(vec![format!("window manager {cmd} already running (pid {})", p.pid)]);
        }
    }
    let mut notes: Vec<String> = stop_wm(state).into_iter().collect();
    notes.push(start_wm(state, display, &cmd)?);
    Ok(notes)
}

/// twm's defaults make it place each new window by hand with the server
/// grabbed: nothing else, screenshots included, gets an answer until a
/// click. This rc places windows at once and never grabs. (This twm has no
/// click-to-focus; focus follows the pointer.)
const TWMRC: &str = "# Written by xcua for `up --wm auto`.\nRandomPlacement\nNoGrabServer\nDontMoveOff\nUsePPosition \"on\"\n";

/// `twm -f <state>/twmrc`, writing the rc.
fn twm_with_rc(state: &State) -> Result<String> {
    let rc = state.dir.join("twmrc");
    write_atomic(&rc, TWMRC)?;
    let path = rc.to_str().ok_or_else(|| anyhow!("state dir path is not UTF-8"))?;
    if path.contains(|c: char| c.is_whitespace() || "'\"$`\\".contains(c)) {
        bail!("state dir {path} has characters twm's command line cannot carry; use another --state-dir");
    }
    Ok(format!("twm -f {path}"))
}

/// Start `cmd` (through `sh -c exec`, so the pid is the WM's) on `display`.
fn start_wm(state: &State, display: &str, cmd: &str) -> Result<String> {
    let log = fs::File::create(state.wm_log_path()).with_context(|| format!("create {}", state.wm_log_path().display()))?;
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(format!("exec {cmd}"))
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = c.spawn().with_context(|| format!("start window manager {cmd:?}"))?;
    let p = Proc::now(child.id()).ok_or_else(|| anyhow!("window manager pid out of range"))?;
    // One that cannot open the display or parse its config exits at once.
    std::thread::sleep(Duration::from_millis(400));
    if let Some(status) = child.try_wait()? {
        let text = fs::read_to_string(state.wm_log_path()).unwrap_or_default();
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        bail!("window manager {cmd:?} exited at once ({status}): {last}");
    }
    write_atomic(&state.wm_pid_path(), &p.line())?;
    write_atomic(&state.wm_cmd_path(), &format!("{cmd}\n"))?;
    // Not waited for: it outlives us, and init reaps it.
    drop(child);
    Ok(format!("started window manager {cmd} on {display} (pid {}); log {}", p.pid, state.wm_log_path().display()))
}

/// Stop the WM xcua started, if it still runs; forget it either way.
/// Without `/proc` it is only forgotten: a live pid there may be anyone's.
fn stop_wm(state: &State) -> Option<String> {
    let (cmd, p, alive) = state.wm()?;
    remove(&state.wm_pid_path());
    remove(&state.wm_cmd_path());
    if !have_proc() {
        return Some(format!(
            "forgot window manager {cmd} (pid {}): without /proc its pid cannot be confirmed, so it is not signalled; it ends with its X server",
            p.pid
        ));
    }
    if !alive {
        return Some(format!("window manager {cmd} (pid {}) had already exited", p.pid));
    }
    let pid = valid_pid(p.pid as u64)?;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(2);
    while p.is_same() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if p.is_same() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    Some(format!("stopped window manager {cmd} (pid {})", p.pid))
}

/// `up` adopts any Xvfb so it stays; an `mcp` registers as a user and
/// adopts one whose starter is dead.
fn reuse(state: &State, owner: Owner, display: String, p: Proc) -> Result<Up> {
    match owner {
        Owner::Up => {
            if state.owner() != Some(Owner::Up) {
                state.set_owner(Some(Owner::Up))?;
            }
        }
        Owner::Mcp(me) => {
            state.add_user(me)?;
            let starter_dead = match state.owner() {
                Some(Owner::Mcp(o)) => !alive(o),
                Some(Owner::Up) => false,
                None => true,
            };
            if starter_dead {
                state.set_owner(Some(Owner::Mcp(me)))?;
            }
        }
    }
    Ok(Up::Reused { display, pid: p.pid })
}

/// Stop xcua's Xvfb (if it started one) and forget the display. With
/// `/proc` this needs no X connection, so a stopped Xvfb is still killed.
pub fn down(state: &State) -> Result<String> {
    let _lock = state.lock()?;
    let nonce = state.nonce();
    let wm = stop_wm(state).map(|m| format!("{m}\n")).unwrap_or_default();
    let msg = match (state.pid(), state.display()) {
        (Some(p), d) => match stop(&p, d.as_deref(), nonce.as_deref()) {
            Stopped::Yes => format!("stopped Xvfb {} (pid {})", d.unwrap_or_default(), p.pid),
            Stopped::NotOurs => {
                format!("pid {} in {} is not xcua's Xvfb any more; not signalled", p.pid, state.pid_path().display())
            }
            Stopped::StillAnswering => {
                // Keep the state: the Xvfb may still be there.
                return Ok(format!(
                    "sent SIGTERM to pid {} but {} still answers; without /proc xcua does not escalate (kill it yourself)",
                    p.pid,
                    d.unwrap_or_default()
                ));
            }
        },
        (None, _) if state.pid_path().exists() => {
            format!("{} holds no usable pid; not signalled", state.pid_path().display())
        }
        (None, Some(d)) => format!("forgot {d} (not started by xcua, left running)"),
        (None, None) => "nothing to stop".to_string(),
    };
    state.clear();
    Ok(format!("{wm}{msg}"))
}

/// What `xcua mcp` calls on exit: leave the users list, and stop xcua's
/// Xvfb when this was its last live user and `up` does not own it.
pub fn release(state: &State, me: u32) {
    let Ok(_lock) = state.lock() else { return };
    state.remove_user(me);
    let Some(p) = state.pid() else { return };
    if state.owner() == Some(Owner::Up) || !state.live_users().is_empty() {
        return;
    }
    let nonce = state.nonce();
    stop_wm(state);
    if stop(&p, state.display().as_deref(), nonce.as_deref()) != Stopped::StillAnswering {
        state.clear();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    Yes,
    /// Not provably xcua's Xvfb: nothing was signalled.
    NotOurs,
    /// Without /proc: SIGTERM sent, the display still answers.
    StillAnswering,
}

/// Stop xcua's Xvfb (see the module doc for when a signal is sent).
/// `display` and `nonce` identify it where `/proc` cannot.
fn stop(p: &Proc, display: Option<&str>, nonce: Option<&str>) -> Stopped {
    let Some(pid) = valid_pid(p.pid as u64) else { return Stopped::NotOurs };
    if have_proc() {
        if !p.is_xvfb() {
            return Stopped::NotOurs;
        }
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while p.is_xvfb() && Instant::now() < deadline {
            // Reap it if it is our child; harmless otherwise.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            std::thread::sleep(Duration::from_millis(50));
        }
        // SIGKILL also ends a stopped (SIGSTOP) process.
        if p.is_xvfb() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
        return Stopped::Yes;
    }
    let (Some(display), Some(nonce)) = (display, nonce) else { return Stopped::NotOurs };
    if !alive(p.pid) || nonce_of(display).as_deref() != Some(nonce) {
        return Stopped::NotOurs;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if !answers(display) {
            return Stopped::Yes;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Stopped::StillAnswering
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
        Some(p) => {
            let users = state.live_users();
            let owner = state.owner();
            let orphan = matches!(owner, Some(Owner::Mcp(o)) if !alive(o)) && users.is_empty();
            s.push_str(&format!(
                "xvfb pid: {} ({}), owner: {}{}, mcp users: {}\n",
                p.pid,
                match state.check(ANSWER_TIMEOUT) {
                    Liveness::Live(..) => "answering",
                    Liveness::Silent(..) => "not answering",
                    Liveness::Gone if p.is_xvfb() => "running, display gone",
                    Liveness::Gone => "gone",
                },
                owner.map(|o| o.as_line()).unwrap_or_else(|| "?".into()),
                if orphan { " (dead; the next `xcua up` or `mcp` adopts it)" } else { "" },
                if users.is_empty() { "none".to_string() } else { format!("{users:?}") }
            ));
        }
        None if state.display().is_some() => s.push_str("xvfb: not started by xcua (attached with `up --display`)\n"),
        None => {}
    }
    match state.wm() {
        Some((cmd, p, true)) => s.push_str(&format!("window manager: {cmd} (pid {}, running; it owns focus)\n", p.pid)),
        Some((cmd, p, false)) => s.push_str(&format!(
            "window manager: {cmd} (pid {}) DIED; see {}. `xcua up --wm auto` starts one again\n",
            p.pid,
            state.wm_log_path().display()
        )),
        None if state.pid().is_some() => {
            s.push_str("window manager: none (xcua gives the clicked window the keyboard; `xcua up --wm auto` starts one)\n")
        }
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

    fn tmp_state(tag: &str) -> State {
        let dir = std::env::temp_dir().join(format!("xcua-state-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        State { dir }
    }

    #[test]
    fn owner_round_trip() {
        for o in [Owner::Up, Owner::Mcp(42)] {
            assert_eq!(Owner::parse(&o.as_line()), Some(o));
        }
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert!(parse_size("12x7").is_err());
    }

    #[test]
    fn env_file_exports_and_reads_both_forms() {
        let st = tmp_state("env");
        st.write(":7", None, None).unwrap();
        assert_eq!(fs::read_to_string(st.env_path()).unwrap(), "export DISPLAY=:7\n");
        assert_eq!(st.display().as_deref(), Some(":7"));
        assert_eq!(parse_env("DISPLAY=:3\n").as_deref(), Some(":3"));
        assert_eq!(parse_env("# x\nexport DISPLAY=\":4\"\n").as_deref(), Some(":4"));
        assert_eq!(parse_env("export FOO=1\n"), None);
        // No temp file is left behind.
        let left: Vec<_> = fs::read_dir(&st.dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.iter().all(|n| !n.to_string_lossy().ends_with(".tmp")), "{left:?}");
        fs::remove_dir_all(&st.dir).unwrap();
    }

    #[test]
    fn pidfile_never_names_a_dangerous_pid() {
        let st = tmp_state("pid");
        for bad in ["0", "1", "-1", "2147483648", "4294967295", "99999999999", "x"] {
            fs::write(st.pid_path(), bad).unwrap();
            assert_eq!(st.pid(), None, "{bad}");
            let msg = down(&st).unwrap();
            assert!(msg.contains("not signalled"), "{bad}: {msg}");
        }
        // Our own pid: alive, but not an Xvfb, so not signalled (this test
        // would die otherwise).
        let me = Proc::now(std::process::id()).unwrap();
        assert!(me.start.is_some());
        assert!(!me.is_xvfb());
        st.write(":99", Some((me, "n")), Some(Owner::Up)).unwrap();
        let msg = down(&st).unwrap();
        assert!(msg.contains("not signalled"), "{msg}");
        // A recorded start time that no longer matches is another process.
        let other = Proc { pid: me.pid, start: me.start.map(|t| t + 1) };
        assert!(!other.is_same());
        assert!(me.is_same());
        fs::remove_dir_all(&st.dir).unwrap();
    }

    #[test]
    fn users_of_dead_processes_are_pruned() {
        let st = tmp_state("users");
        st.add_user(std::process::id()).unwrap();
        fs::create_dir_all(st.users_dir()).unwrap();
        // A pid with a start time that cannot match, and garbage.
        fs::write(st.users_dir().join(std::process::id().to_string() + "0"), "1 1\n").unwrap();
        fs::write(st.users_dir().join("notapid"), "").unwrap();
        assert_eq!(st.live_users(), vec![std::process::id()]);
        assert_eq!(fs::read_dir(st.users_dir()).unwrap().count(), 1);
        st.remove_user(std::process::id());
        assert!(st.live_users().is_empty());
        fs::remove_dir_all(&st.dir).unwrap();
    }

    #[test]
    fn lock_has_a_deadline_and_an_excl_fallback() {
        let st = tmp_state("lock");
        let held = st.lock().unwrap();
        let t0 = Instant::now();
        let e = st.lock_within(Duration::from_millis(300)).err().expect("second lock while held");
        assert!(format!("{e:#}").contains("locked by another xcua"), "{e:#}");
        assert!(t0.elapsed() < Duration::from_secs(2));
        drop(held);
        drop(st.lock_within(Duration::from_millis(300)).unwrap());

        // The O_EXCL fallback: exclusive, freed on drop, broken when stale.
        let a = st.lock_excl(Instant::now()).unwrap();
        let e = st.lock_excl(Instant::now() + Duration::from_millis(200)).err().expect("excl lock while held");
        assert!(format!("{e:#}").contains(&format!("pid {}", std::process::id())), "{e:#}");
        drop(a);
        assert!(!st.dir.join("lock.excl").exists());
        // A holder that is not alive (start time cannot match): broken.
        fs::write(st.dir.join("lock.excl"), format!("{} 1\n", std::process::id())).unwrap();
        drop(st.lock_excl(Instant::now()).unwrap());
        fs::remove_dir_all(&st.dir).unwrap();
    }

    /// A socket that accepts and never answers is silent, not gone, for as
    /// long as the patience runs; a missing socket is gone at once. The fake
    /// server is an abstract socket on a free high display number.
    #[test]
    fn silent_and_absent_servers_are_told_apart() {
        use std::os::linux::net::SocketAddrExt;
        let st = tmp_state("silent");
        let free = |n: u32| !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists();
        let (n, _listener) = (5000 + std::process::id() % 3000..9000)
            .filter(|&n| free(n))
            .find_map(|n| {
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(format!("/tmp/.X11-unix/X{n}")).ok()?;
                std::os::unix::net::UnixListener::bind_addr(&addr).ok().map(|l| (n, l))
            })
            .expect("a free display number");
        let display = format!(":{n}");
        let me = Proc::now(std::process::id()).unwrap();
        st.write(&display, Some((me, "n")), Some(Owner::Up)).unwrap();
        let t0 = Instant::now();
        assert!(matches!(st.check(Duration::from_millis(2500)), Liveness::Silent(..)));
        assert!(t0.elapsed() >= Duration::from_millis(2500), "gave up after {:?}", t0.elapsed());
        // up never kills on silence it cannot prove is a stopped Xvfb of
        // ours: here the pid is this test (running, not an Xvfb).
        assert!(!stopped(std::process::id()));
        let e = up(&st, None, (320, 240), Owner::Up).expect_err("up started a second server over a silent one");
        assert!(format!("{e:#}").contains("not stopped"), "{e:#}");
        drop(_listener);
        let t0 = Instant::now();
        assert_eq!(crate::x11::probe(&display, Duration::from_secs(2)), Probe::Absent);
        assert!(t0.elapsed() < Duration::from_millis(500));
        assert!(explicit(Some("/tmp/.X11-unix/X0")).is_err());
        fs::remove_dir_all(&st.dir).unwrap();
    }

    #[test]
    fn start_time_parses_a_comm_with_parens() {
        assert!(start_time(std::process::id()).is_some());
        assert_eq!(start_time(u32::MAX), None);
    }
}
