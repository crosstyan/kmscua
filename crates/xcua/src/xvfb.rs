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
//! wedged Xvfb cannot hang `up`, `down` or an exiting `mcp`. With `/proc`,
//! an Xvfb that is xcua's (pid and start time) but does not answer is
//! killed before a new one is started.
//! - `xvfb.log`: the Xvfb's stderr. `recordings/`: stills recordings.
//!
//! Which display a command uses: `--display :N` when given (`--display
//! env` spells out the shell's `$DISPLAY`), else xcua's live Xvfb, else a
//! display recorded by `xcua up --display`, else a new Xvfb. `$DISPLAY` is
//! never used implicitly. `--display` on any command but `up` uses that
//! display for the one run and leaves the state dir alone; `up --display`
//! records the attach, and refuses while xcua's own Xvfb is running.
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

/// How long an Xvfb gets to report its display and answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a display gets to answer a probe.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a command waits for another xcua to let go of the state dir.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

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

    fn parse(s: &str) -> Option<Self> {
        let mut it = s.split_whitespace();
        let pid = it.next()?.parse::<u64>().ok()?;
        let pid = valid_pid(pid)? as u32;
        Some(Self { pid, start: it.next().and_then(|t| t.parse().ok()) })
    }

    fn line(&self) -> String {
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
    fn is_same(&self) -> bool {
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
        remove(&self.nonce_path());
        remove(&self.owner_path());
        let _ = fs::remove_dir_all(self.users_dir());
    }

    /// xcua's own Xvfb, if its display answers with xcua's nonce. Without a
    /// recorded nonce (state from an older xcua), by pid and start time.
    pub fn live(&self) -> Option<(String, Proc)> {
        let (display, pid) = (self.display()?, self.pid()?);
        let ours = match self.nonce() {
            Some(n) => nonce_of(&display).as_deref() == Some(n.as_str()),
            None => pid.is_xvfb() && answers(&display),
        };
        ours.then_some((display, pid))
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
        Some(d) => Ok(Some(d.to_string())),
    }
}

/// `xcua up`, or the auto-up of `mcp`/`screenshot`/`doctor` (with
/// `display` None): make sure a display answers, under the state lock (see
/// the module doc for the order and the ownership rule). An `mcp` caller
/// (`Owner::Mcp`) is registered as a user of xcua's Xvfb.
pub fn up(state: &State, display: Option<&str>, size: (u32, u32), owner: Owner) -> Result<Up> {
    let _lock = state.lock()?;
    if let Some(d) = display {
        if let Some((own, p)) = state.live() {
            if own == d {
                return reuse(state, owner, own, p);
            }
            bail!(
                "xcua's own Xvfb {own} (pid {}) is running for {}; run `xcua down` first, or use --display without `up`",
                p.pid,
                state.dir.display()
            );
        }
        if !answers(d) {
            bail!("no X server answers on {d}");
        }
        state.clear();
        state.write(d, None, None)?;
        return Ok(Up::Attached { display: d.to_string() });
    }
    if let Some((d, p)) = state.live() {
        return reuse(state, owner, d, p);
    }
    if let Some(d) = state.attached() {
        return Ok(Up::Attached { display: d });
    }
    // xcua's Xvfb by pid and start time, but not answering: stopped or
    // wedged. Kill it rather than leave it behind.
    if let Some(p) = state.pid().filter(|p| p.is_xvfb()) {
        log::warn!("xcua's Xvfb (pid {}) does not answer; killing it", p.pid);
        stop(&p, None, None);
    }
    // Whatever the state dir said is stale now.
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
    Ok(Up::Started { display, pid: proc.pid })
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
    Ok(msg)
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
                if p.is_xvfb() || (!have_proc() && state.live().is_some()) { "alive" } else { "gone" },
                owner.map(|o| o.as_line()).unwrap_or_else(|| "?".into()),
                if orphan { " (dead; the next `xcua up` or `mcp` adopts it)" } else { "" },
                if users.is_empty() { "none".to_string() } else { format!("{users:?}") }
            ));
        }
        None if state.display().is_some() => s.push_str("xvfb: not started by xcua (attached with `up --display`)\n"),
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

    #[test]
    fn start_time_parses_a_comm_with_parens() {
        assert!(start_time(std::process::id()).is_some());
        assert_eq!(start_time(u32::MAX), None);
    }
}
