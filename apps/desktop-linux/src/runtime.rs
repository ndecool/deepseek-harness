//! Supervises one `dsh web` child: launch, readiness, crash detection, and process-group shutdown.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant};

const READY_PREFIX: &str = "dsh web: ";
const LOG_TAIL_LINES: usize = 200;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How to start dsh: program plus leading arguments, before `web --no-open --port 0`.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Prepended to PATH so plugin subprocesses resolve the same `node`.
    pub path_prefix: Option<PathBuf>,
    pub source: &'static str,
}

impl Launch {
    /// Resolution order: `DSH_LINUX_COMMAND`, bundled runtime, workspace checkout (debug builds), `dsh` on PATH.
    pub fn resolve(resource_dir: Option<&Path>, env: &HashMap<String, String>) -> Result<Self, String> {
        if let Some(command) = env.get("DSH_LINUX_COMMAND").filter(|c| !c.is_empty()) {
            return Ok(Self { program: command.into(), args: vec![], path_prefix: None, source: "DSH_LINUX_COMMAND" });
        }
        if let Some(resources) = resource_dir {
            let runtime = resources.join("runtime");
            let node = runtime.join("node").join("bin").join("node");
            let entry = runtime.join("dsh").join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js");
            if node.is_file() && entry.is_file() {
                return Ok(Self {
                    program: node,
                    args: vec![entry.to_string_lossy().into_owned()],
                    path_prefix: Some(runtime.join("node").join("bin")),
                    source: "bundled runtime",
                });
            }
        }
        let path = env.get("PATH").map(String::as_str).unwrap_or("/usr/local/bin:/usr/bin:/bin");
        if cfg!(debug_assertions) {
            let entry = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/lib/bin.js");
            if let (true, Some(node)) = (entry.is_file(), which("node", path)) {
                return Ok(Self {
                    program: node,
                    args: vec![entry.canonicalize().unwrap_or(entry).to_string_lossy().into_owned()],
                    path_prefix: None,
                    source: "workspace checkout",
                });
            }
        }
        if let Some(dsh) = which("dsh", path) {
            return Ok(Self { program: dsh, args: vec![], path_prefix: None, source: "PATH" });
        }
        Err("no dsh runtime found: not bundled, DSH_LINUX_COMMAND unset, and no `dsh` on PATH \
             (install it with `npm install -g @deepseek-ai/dsh`)"
            .into())
    }
}

fn which(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(name)).find(|candidate| {
        candidate.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Ready { url: String },
    Failed { message: String },
}

#[derive(Serialize)]
pub struct StatusSnapshot {
    pub phase: &'static str,
    pub message: Option<String>,
    pub source: Option<&'static str>,
    pub log: Vec<String>,
    pub log_path: String,
}

struct State {
    phase: Phase,
    generation: u64,
    pid: Option<i32>,
    stopping: bool,
    source: Option<&'static str>,
}

type Callback = Box<dyn Fn(&Phase) + Send + Sync>;

pub struct Supervisor {
    state: Mutex<State>,
    exited: Condvar,
    log: Mutex<VecDeque<String>>,
    log_file: Mutex<Option<File>>,
    log_path: PathBuf,
    resource_dir: Option<PathBuf>,
    env: HashMap<String, String>,
    on_change: Callback,
    /// Serializes restart and shutdown requests from the tray, the status page, and app exit.
    lifecycle: Mutex<()>,
}

impl Supervisor {
    pub fn new(
        resource_dir: Option<PathBuf>,
        env: HashMap<String, String>,
        log_path: PathBuf,
        on_change: Callback,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State { phase: Phase::Starting, generation: 0, pid: None, stopping: false, source: None }),
            exited: Condvar::new(),
            log: Mutex::new(VecDeque::with_capacity(LOG_TAIL_LINES)),
            log_file: Mutex::new(open_log(&log_path)),
            log_path,
            resource_dir,
            env,
            on_change,
            lifecycle: Mutex::new(()),
        })
    }

    pub fn phase(&self) -> Phase {
        self.state.lock().phase.clone()
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let state = self.state.lock();
        let (phase, message) = match &state.phase {
            Phase::Starting => ("starting", None),
            Phase::Ready { .. } => ("ready", None),
            Phase::Failed { message } => ("failed", Some(message.clone())),
        };
        StatusSnapshot {
            phase,
            message,
            source: state.source,
            log: self.log.lock().iter().cloned().collect(),
            log_path: self.log_path.to_string_lossy().into_owned(),
        }
    }

    /// Stop any running child, then launch a fresh one.
    pub fn restart(self: &Arc<Self>) {
        let _lifecycle = self.lifecycle.lock();
        self.stop();
        let launch = match Launch::resolve(self.resource_dir.as_deref(), &self.env) {
            Ok(launch) => launch,
            Err(message) => {
                self.record("launcher", &message);
                let generation = self.state.lock().generation;
                self.transition(generation, Phase::Failed { message });
                return;
            }
        };
        let generation = {
            let mut state = self.state.lock();
            state.generation += 1;
            state.stopping = false;
            state.phase = Phase::Starting;
            state.source = Some(launch.source);
            state.generation
        };
        (self.on_change)(&Phase::Starting);
        self.record("launcher", &format!("starting dsh ({}): {} {}", launch.source, launch.program.display(), launch.args.join(" ")));

        let mut env = self.env.clone();
        if let Some(prefix) = &launch.path_prefix {
            let path = env.get("PATH").cloned().unwrap_or_default();
            env.insert("PATH".into(), format!("{}:{path}", prefix.display()));
        }
        env.insert("NO_COLOR".into(), "1".into());
        env.insert("FORCE_COLOR".into(), "0".into());
        let home = env.get("HOME").cloned().unwrap_or_else(|| "/".into());
        let spawned = Command::new(&launch.program)
            .args(&launch.args)
            .args(["web", "--no-open", "--host", "127.0.0.1", "--port", "0"])
            .env_clear()
            .envs(&env)
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so shutdown reaches plugin subprocesses too.
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let message = format!("could not start {}: {error}", launch.program.display());
                self.record("launcher", &message);
                self.transition(generation, Phase::Failed { message });
                return;
            }
        };
        self.state.lock().pid = Some(child.id() as i32);

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let this = Arc::clone(self);
        std::thread::spawn(move || this.pump(generation, "stdout", stdout, true));
        let this = Arc::clone(self);
        std::thread::spawn(move || this.pump(generation, "stderr", stderr, false));

        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let status = child.wait();
            let mut state = this.state.lock();
            if state.generation != generation {
                return;
            }
            state.pid = None;
            let stopping = state.stopping;
            drop(state);
            this.exited.notify_all();
            if !stopping {
                let message = match status {
                    Ok(status) => format!("dsh exited unexpectedly ({status})"),
                    Err(error) => format!("lost track of dsh: {error}"),
                };
                this.record("launcher", &message);
                this.transition(generation, Phase::Failed { message });
            }
        });

        let this = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(STARTUP_TIMEOUT);
            let still_starting = {
                let state = this.state.lock();
                state.generation == generation && state.phase == Phase::Starting
            };
            if still_starting {
                let message = format!("dsh did not report its Web UI URL within {}s", STARTUP_TIMEOUT.as_secs());
                this.record("launcher", &message);
                this.transition(generation, Phase::Failed { message });
                this.stop();
            }
        });
    }

    /// Stop the runtime for application exit.
    pub fn shutdown(&self) {
        let _lifecycle = self.lifecycle.lock();
        self.stop();
    }

    /// SIGTERM the child's process group, escalating to SIGKILL after the grace period.
    fn stop(&self) {
        let mut state = self.state.lock();
        let Some(pid) = state.pid else { return };
        state.stopping = true;
        // SAFETY: negative pid addresses the process group created at spawn.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + STOP_GRACE;
        while state.pid.is_some() {
            if self.exited.wait_until(&mut state, deadline).timed_out() {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                self.exited.wait_for(&mut state, Duration::from_secs(2));
                break;
            }
        }
        drop(state);
        self.record("launcher", "dsh stopped");
    }

    fn pump(&self, generation: u64, stream: &str, source: impl Read, watch_ready: bool) {
        for line in BufReader::new(source).lines() {
            let Ok(line) = line else { break };
            if watch_ready {
                if let Some(url) = ready_url(&line) {
                    self.transition_from_starting(generation, url);
                }
            }
            self.record(stream, &line);
        }
    }

    fn transition_from_starting(&self, generation: u64, url: String) {
        {
            let mut state = self.state.lock();
            if state.generation != generation || state.phase != Phase::Starting {
                return;
            }
            state.phase = Phase::Ready { url: url.clone() };
        }
        (self.on_change)(&Phase::Ready { url });
    }

    fn transition(&self, generation: u64, phase: Phase) {
        {
            let mut state = self.state.lock();
            if state.generation != generation {
                return;
            }
            state.phase = phase.clone();
        }
        (self.on_change)(&phase);
    }

    fn record(&self, stream: &str, line: &str) {
        let line = format!("[{stream}] {}", redact_token(line));
        if let Some(file) = self.log_file.lock().as_mut() {
            let _ = writeln!(file, "{line}");
        }
        let mut log = self.log.lock();
        if log.len() == LOG_TAIL_LINES {
            log.pop_front();
        }
        log.push_back(line);
    }
}

/// Rotate the previous session's log to `.1` and open a fresh owner-only file.
fn open_log(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
    OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path).ok()
}

/// The tokenized loopback URL from `dsh web: http://127.0.0.1:PORT/?token=… (LAN: …)`.
fn ready_url(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(READY_PREFIX)?;
    let candidate = rest.split_whitespace().next()?;
    let url = url::Url::parse(candidate).ok()?;
    let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || url.host_str() == Some("localhost");
    (url.scheme() == "http" && loopback).then(|| url.to_string())
}

/// Launch tokens are bearer credentials; keep them out of logs and the status page.
fn redact_token(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(index) = rest.find("token=") {
        let (head, tail) = rest.split_at(index + "token=".len());
        out.push_str(head);
        out.push_str("<redacted>");
        let end = tail.find(|c: char| c == '&' || c == ' ' || c == ')' || c == '#').unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{ready_url, redact_token};

    #[test]
    fn ready_line_yields_loopback_url_and_ignores_lan_suffix() {
        let line = "dsh web: http://127.0.0.1:4567/?token=abc (LAN: http://192.168.1.5:4567/?token=abc)";
        assert_eq!(ready_url(line).as_deref(), Some("http://127.0.0.1:4567/?token=abc"));
    }

    #[test]
    fn non_ready_and_non_loopback_lines_are_ignored() {
        assert_eq!(ready_url("dsh web: opening the default browser; pass --no-open to disable"), None);
        assert_eq!(ready_url("dsh web: https://proxy.example:8443/web/?token=t"), None);
        assert_eq!(ready_url("[web] dsh web: http://127.0.0.1:1/"), None);
    }

    #[test]
    fn tokens_are_redacted() {
        assert_eq!(
            redact_token("dsh web: http://127.0.0.1:1/?token=s3cr3t&x=1 (LAN: http://h/?token=s3cr3t)"),
            "dsh web: http://127.0.0.1:1/?token=<redacted>&x=1 (LAN: http://h/?token=<redacted>)"
        );
    }
}
