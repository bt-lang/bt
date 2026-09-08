//! Native process ownership, bounded output capture, and asynchronous cancellation.

use super::MAX_REQUEST_BYTES;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Retained raw bytes per stream, independent of total process output.
const TAIL_BYTES: usize = 1024 * 1024;
/// Process-wide cap shared by all extension instances using this SDK service.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// One extension instance's task table, rooted at its canonical project directory.
pub struct ProcessHost {
    /// Immutable filesystem boundary for declared media paths.
    root: PathBuf,
    /// Live and terminal handles, explicitly released by close or host destruction.
    tasks: HashMap<u64, Task>,
    /// Monotonic identifier that is never reused within this host.
    next_id: u64,
    /// Active workers, including cancelled tasks whose handles were already closed.
    active: Arc<AtomicUsize>,
}

/// A handle and its worker's shared cancellation and progress state.
struct Task {
    /// Cancellation is checked between bounded pipe-drain batches.
    cancel: Arc<AtomicBool>,
    /// Short-held mutex protecting bounded output buffers and task metadata.
    status: Arc<Mutex<Status>>,
    /// Caller-owned output paths and their atomic final-disposal policy.
    cleanup: Arc<Cleanup>,
}

/// Output ownership remains available after the native process finishes.
struct Cleanup {
    /// Only empty ordinary files explicitly reserved and declared at spawn.
    paths: Vec<PathBuf>,
    /// Set under the status lock so terminalization cannot miss close-time disposal.
    discard: AtomicBool,
}

/// A stream tail stores bytes without repeatedly shifting a one-MiB allocation.
#[derive(Default)]
struct Tail {
    /// Oldest-to-newest retained bytes.
    bytes: VecDeque<u8>,
    /// Whether bytes have been discarded because the cap was exceeded.
    truncated: bool,
}

impl Tail {
    /// Append output while preserving the fixed maximum retained size.
    fn append(&mut self, bytes: &[u8]) {
        let excess = (self.bytes.len() + bytes.len()).saturating_sub(TAIL_BYTES);
        if excess > 0 {
            self.truncated = true;
            self.bytes.drain(..excess.min(self.bytes.len()));
        }
        self.bytes
            .extend(bytes.iter().skip(bytes.len().saturating_sub(TAIL_BYTES)));
    }

    /// Decode a snapshot, replacing invalid UTF-8 and a split leading code point.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

/// Mutable worker state; polling never waits for process completion.
struct Status {
    /// Stable protocol state name.
    state: &'static str,
    /// Time the task was accepted, including process startup.
    started: Instant,
    /// Frozen duration once the task reaches a terminal state.
    elapsed: Option<Duration>,
    /// Bounded captured standard output.
    stdout: Tail,
    /// Bounded captured standard error.
    stderr: Tail,
    /// Exit code, absent for queued/running tasks and signal termination.
    exit_code: Option<i32>,
}

impl Status {
    /// Freeze the final state only after the owned child has been reaped.
    fn finish(&mut self, state: &'static str, exit_code: Option<i32>) {
        self.state = state;
        self.exit_code = exit_code;
        self.elapsed = Some(self.started.elapsed());
    }

    /// Produce a bounded protocol snapshot suitable for immediate return to WASM.
    fn snapshot(&self) -> Value {
        json!({"state": self.state, "stdout": self.stdout.text(),
            "stderr": self.stderr.text(), "stdout_truncated": self.stdout.truncated,
            "stderr_truncated": self.stderr.truncated, "exit_code": self.exit_code,
            "elapsed_ms": self.elapsed.unwrap_or_else(|| self.started.elapsed()).as_millis() as u64})
    }
}

/// Worker reservations survive handle close, so cancellation cannot bypass limits.
struct Permit {
    /// The per-instance reservation released along with the global reservation.
    local: Arc<AtomicUsize>,
}

impl Drop for Permit {
    /// Return both reservations on completion, spawn failure, or unwinding.
    fn drop(&mut self) {
        self.local.fetch_sub(1, Ordering::AcqRel);
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ProcessHost {
    /// Create a service with a canonical existing directory as its immutable root.
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|error| format!("Invalid process root: {error}"))?;
        if !root.is_dir() {
            return Err("Process root must be a directory".into());
        }
        Ok(Self {
            root,
            tasks: HashMap::new(),
            next_id: 1,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Dispatch spawn/poll/cancel/close JSON, returning business JSON without an envelope.
    ///
    /// This is a process capability, not an executable sandbox: callers must trust
    /// the chosen program. Programs must not create detached descendants. Windows
    /// job objects and Unix process groups clean up ordinary descendants. Capture
    /// never waits for descendant pipe handles. Abrupt Unix host death is not covered.
    /// `cleanup_paths` must name empty ordinary files reserved by the caller using
    /// create-new semantics; failed tasks remove them after their child is reaped.
    /// Closing with `discard_output:true` additionally removes these owned files on
    /// success, including when success happened immediately before close. Ordinary
    /// close preserves successful output. The shared status lock makes this handoff
    /// atomic with worker terminalization.
    pub fn request(&mut self, raw: &str) -> Result<String, String> {
        if raw.len() > MAX_REQUEST_BYTES {
            return Err("Host process request exceeds 64 KiB".into());
        }
        let request: Value = serde_json::from_str(raw)
            .map_err(|error| format!("Invalid process request: {error}"))?;
        let op = request
            .get("op")
            .and_then(Value::as_str)
            .ok_or("Process request requires op")?;
        if op == "spawn" {
            return self.spawn(&request).map(|value| value.to_string());
        }
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("Process request requires an unsigned id")?;
        if op == "close" {
            let discard = match request.get("discard_output") {
                Some(value) => value.as_bool().ok_or("discard_output must be a boolean")?,
                None => false,
            };
            let task = self.tasks.remove(&id).ok_or("Unknown process task id")?;
            task.cancel.store(true, Ordering::Release);
            let status = task
                .status
                .lock()
                .map_err(|_| "Process state lock failed")?;
            if discard {
                task.cleanup.discard.store(true, Ordering::Release);
                if !matches!(status.state, "queued" | "running") {
                    cleanup_outputs(&self.root, &task.cleanup.paths);
                }
            }
            return Ok(json!({"closed": true}).to_string());
        }
        let task = self.tasks.get(&id).ok_or("Unknown process task id")?;
        match op {
            "cancel" => task.cancel.store(true, Ordering::Release),
            "poll" => (),
            _ => return Err("Unknown process request operation".into()),
        }
        Ok(task
            .status
            .lock()
            .map_err(|_| "Process state lock failed")?
            .snapshot()
            .to_string())
    }

    /// Validate a relative path without allowing lexical traversal or symlink escape.
    fn path(&self, raw: &str, must_exist: bool) -> Result<PathBuf, String> {
        let relative = Path::new(raw);
        if raw.is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err("Process paths must be project-relative without parent traversal".into());
        }
        let path = self.root.join(relative);
        let resolved = if must_exist || path.exists() {
            path.canonicalize()
                .map_err(|error| format!("Cannot resolve process path: {error}"))?
        } else {
            let parent = path
                .parent()
                .ok_or("Process output path has no parent")?
                .canonicalize()
                .map_err(|error| format!("Cannot resolve output directory: {error}"))?;
            parent.join(
                path.file_name()
                    .ok_or("Process output path has no file name")?,
            )
        };
        if !resolved.starts_with(&self.root) || resolved == self.root {
            return Err("Process path escapes the project root".into());
        }
        Ok(path)
    }

    /// Read and validate one optional array of explicitly declared filesystem paths.
    fn paths(
        &self,
        request: &Value,
        field: &str,
        must_exist: bool,
    ) -> Result<Vec<PathBuf>, String> {
        let Some(value) = request.get(field) else {
            return Ok(Vec::new());
        };
        let values = value
            .as_array()
            .ok_or_else(|| format!("{field} must be an array"))?;
        if values.len() > 256 {
            return Err(format!("{field} exceeds 256 paths"));
        }
        values
            .iter()
            .map(|value| {
                self.path(
                    value.as_str().ok_or("Process path must be a string")?,
                    must_exist,
                )
            })
            .collect()
    }

    /// Reserve bounded capacity before transferring the command to a background worker.
    fn spawn(&mut self, request: &Value) -> Result<Value, String> {
        if self.tasks.len() >= 32 {
            return Err("Process task handle limit reached (32)".into());
        }
        if self.active.load(Ordering::Acquire) >= 4 {
            return Err("Process active task limit reached (4)".into());
        }
        let program = request
            .get("program")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or("Process program must be a nonempty string")?
            .to_owned();
        let args = request
            .get("args")
            .and_then(Value::as_array)
            .ok_or("Process args must be an array")?;
        if args.len() > 256 {
            return Err("Process args exceeds 256 arguments".into());
        }
        let args: Vec<String> = args
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "Process arguments must be strings".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let timeout = match request.get("timeout_ms") {
            Some(value) => value
                .as_u64()
                .filter(|value| (1..=300_000).contains(value))
                .ok_or("Process timeout_ms must be an integer from 1 to 300000")?,
            None => 60_000,
        };
        self.paths(request, "read_paths", true)?;
        self.paths(request, "write_paths", false)?;
        let cleanup = self.paths(request, "cleanup_paths", true)?;
        for path in &cleanup {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("Cannot inspect cleanup file: {error}"))?;
            if !metadata.file_type().is_file() || metadata.len() != 0 {
                return Err(
                    "Cleanup paths must be empty ordinary files reserved by the caller".into(),
                );
            }
        }
        let cleanup = Arc::new(Cleanup {
            paths: cleanup,
            discard: AtomicBool::new(false),
        });
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or("Process task id limit reached")?;
        ACTIVE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < 32).then_some(active + 1)
            })
            .map_err(|_| "Global process active task limit reached (32)")?;
        self.active.fetch_add(1, Ordering::AcqRel);
        let permit = Permit {
            local: self.active.clone(),
        };
        let status = Arc::new(Mutex::new(Status {
            state: "queued",
            started: Instant::now(),
            elapsed: None,
            stdout: Tail::default(),
            stderr: Tail::default(),
            exit_code: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let worker_cleanup = cleanup.clone();
        let root = self.root.clone();
        std::thread::Builder::new()
            .name("bt-extension-process".into())
            .spawn(move || {
                let _permit = permit;
                run(
                    &program,
                    &args,
                    &root,
                    worker_cleanup,
                    Duration::from_millis(timeout),
                    worker_cancel,
                    worker_status,
                );
            })
            .map_err(|error| format!("Cannot start process worker: {error}"))?;
        let id = self.next_id;
        self.next_id = next_id;
        self.tasks.insert(
            id,
            Task {
                status,
                cancel,
                cleanup,
            },
        );
        Ok(json!({"id": id}))
    }
}

impl Drop for ProcessHost {
    /// Request asynchronous cleanup of every child without blocking an async host thread.
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.cancel.store(true, Ordering::Release);
        }
    }
}

/// Child ownership guard makes worker unwinding kill and reap its precise process.
struct OwnedChild {
    /// The direct child, always killed and reaped before releasing worker ownership.
    child: Child,
    /// Windows closes the job even when the BT process exits without Rust destructors.
    #[cfg(windows)]
    _job: Job,
}

/// A non-inheritable Windows job terminates descendants when its last handle closes.
#[cfg(windows)]
struct Job(
    /// Sole owned handle to the unnamed kill-on-close job object.
    windows_sys::Win32::Foundation::HANDLE,
);

#[cfg(windows)]
impl Job {
    /// Attach the direct child before processing its output; fail closed on any error.
    fn attach(child: &Child) -> Result<Self, std::io::Error> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // A null security descriptor creates a non-inheritable, unnamed job handle.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Self(handle);
        // All unspecified job limits are intentionally disabled.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // The kernel copies this stack structure; the child and job handles stay live.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Nested jobs are supported on current Windows; a restrictive parent may reject assignment.
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

#[cfg(windows)]
impl Drop for Job {
    /// Closing the private job kills any descendants that survived the direct child.
    fn drop(&mut self) {
        // This handle is owned solely by this guard and is closed exactly once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

impl Drop for OwnedChild {
    /// Kill is harmless after natural exit; wait prevents a zombie on Unix.
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // The child starts a private group, so this cannot target BT's own group.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Delete caller-owned failed outputs only while their current parent remains in root.
fn cleanup_outputs(root: &Path, paths: &[PathBuf]) {
    for path in paths {
        let valid_parent = path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent.starts_with(root));
        let regular_file =
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file());
        if valid_parent && regular_file {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Run a shell-free command with bounded nonblocking output capture and cancellation.
fn run(
    program: &str,
    args: &[String],
    root: &Path,
    cleanup: Arc<Cleanup>,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
) {
    if cancel.load(Ordering::Acquire) {
        cleanup_outputs(root, &cleanup.paths);
        status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .finish("cancelled", None);
        return;
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: background jobs never steal focus.
    }
    let mut child = match command.spawn() {
        Ok(child) => {
            #[cfg(windows)]
            {
                let mut child = child;
                match Job::attach(&child) {
                    Ok(job) => OwnedChild { child, _job: job },
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup_outputs(root, &cleanup.paths);
                        let mut state = status.lock().unwrap_or_else(|poison| poison.into_inner());
                        state
                            .stderr
                            .append(format!("Cannot isolate process job: {error}").as_bytes());
                        state.finish("failed", None);
                        return;
                    }
                }
            }
            #[cfg(not(windows))]
            {
                OwnedChild { child }
            }
        }
        Err(error) => {
            cleanup_outputs(root, &cleanup.paths);
            let mut state = status.lock().unwrap_or_else(|poison| poison.into_inner());
            state
                .stderr
                .append(format!("Cannot start process: {error}").as_bytes());
            state.finish("failed", None);
            return;
        }
    };
    status
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .state = "running";
    let mut output = child.child.stdout.take();
    let mut errors = child.child.stderr.take();
    let started = status
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .started;
    let (final_state, code) = loop {
        drain(output.as_mut(), &status, false);
        drain(errors.as_mut(), &status, true);
        let stop = if cancel.load(Ordering::Acquire) {
            Some("cancelled")
        } else if started.elapsed() >= timeout {
            Some("timed_out")
        } else {
            None
        };
        if let Some(stop) = stop {
            let _ = child.child.kill();
            let code = child.child.wait().ok().and_then(|exit| exit.code());
            break (stop, code);
        }
        match child.child.try_wait() {
            Ok(Some(exit)) => {
                break (
                    if exit.success() {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    exit.code(),
                )
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.child.kill();
                let _ = child.child.wait();
                status
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .stderr
                    .append(error.to_string().as_bytes());
                break ("failed", None);
            }
        }
    };
    // Only available bytes are drained. Descendants retaining inherited pipe handles
    // cannot delay completion indefinitely; extensions must use direct-child tools.
    drain(output.as_mut(), &status, false);
    drain(errors.as_mut(), &status, true);
    drop(output);
    drop(errors);
    drop(child);
    // Close either observes a terminal state and disposes files itself, or sets the
    // flag before this critical section. No successful-output disposal can be lost.
    let mut status = status.lock().unwrap_or_else(|poison| poison.into_inner());
    if final_state != "succeeded" || cleanup.discard.load(Ordering::Acquire) {
        cleanup_outputs(root, &cleanup.paths);
    }
    status.finish(final_state, code);
}

/// Drain at most two MiB per turn so continuous output cannot starve cancellation.
fn drain<R: Read + PipeReady>(pipe: Option<&mut R>, status: &Mutex<Status>, stderr: bool) {
    let Some(pipe) = pipe else {
        return;
    };
    let mut buffer = [0_u8; 8192];
    for _ in 0..256 {
        if !pipe.ready() {
            break;
        }
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let mut status = status.lock().unwrap_or_else(|poison| poison.into_inner());
                if stderr {
                    status.stderr.append(&buffer[..count]);
                } else {
                    status.stdout.append(&buffer[..count]);
                }
            }
        }
    }
}

/// Check pipe availability before a read, without spawning blocking reader threads.
trait PipeReady {
    /// Return true only when an immediate read can make progress or observe EOF.
    fn ready(&self) -> bool;
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawHandle> PipeReady for T {
    /// Anonymous pipes support PeekNamedPipe; broken pipes are treated as drained.
    fn ready(&self) -> bool {
        #[link(name = "kernel32")]
        extern "system" {
            /// Inspect available pipe bytes without consuming output or waiting.
            fn PeekNamedPipe(
                handle: *mut std::ffi::c_void,
                buffer: *mut std::ffi::c_void,
                buffer_size: u32,
                bytes_read: *mut u32,
                bytes_available: *mut u32,
                bytes_left: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        // The pipe handle is live, and only the valid byte-count pointer is written.
        unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            ) != 0
                && available > 0
        }
    }
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> PipeReady for T {
    /// A single reader plus zero-timeout poll makes the following pipe read immediate.
    fn ready(&self) -> bool {
        let mut descriptor = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Poll writes exactly this stack-allocated descriptor and never waits.
        unsafe { libc::poll(&mut descriptor, 1, 0) > 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disambiguate parallel fixtures when the Windows wall clock has coarse resolution.
    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(1);

    /// Create a unique isolated project for native process ownership checks.
    fn host() -> (ProcessHost, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "bt-process-{}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        (ProcessHost::new(path.clone()).unwrap(), path)
    }

    /// Use a noninteractive platform command without relying on a shell in the service.
    fn command(script: &str) -> Value {
        #[cfg(windows)]
        {
            json!({"op":"spawn","program":"powershell.exe","args":["-NoProfile","-NonInteractive","-Command",script]})
        }
        #[cfg(unix)]
        {
            json!({"op":"spawn","program":"sh","args":["-c",script]})
        }
    }

    /// Poll with an independent deadline so test regressions cannot hang the suite.
    fn terminal(host: &mut ProcessHost, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let value: Value = serde_json::from_str(
                &host
                    .request(&json!({"op":"poll","id":id}).to_string())
                    .unwrap(),
            )
            .unwrap();
            if !matches!(value["state"].as_str(), Some("queued" | "running")) {
                return value;
            }
            assert!(Instant::now() < deadline, "process did not terminate");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    /// Capture both streams, preserve success outputs, then release the task handle.
    fn captures_and_closes() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script = "[Console]::Out.Write('hello'); [Console]::Error.Write('diagnostic')";
        #[cfg(unix)]
        let script = "printf hello; printf diagnostic >&2";
        let reply: Value =
            serde_json::from_str(&host.request(&command(script).to_string()).unwrap()).unwrap();
        let id = reply["id"].as_u64().unwrap();
        let result = terminal(&mut host, id);
        assert_eq!(result["state"], "succeeded");
        assert_eq!(result["stdout"], "hello");
        assert_eq!(result["stderr"], "diagnostic");
        host.request(&json!({"op":"close","id":id}).to_string())
            .unwrap();
        assert!(host
            .request(&json!({"op":"poll","id":id}).to_string())
            .is_err());
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Timeouts and cancellation reap the child and remove only declared owned outputs.
    fn timeout_cancel_and_cleanup() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script = "Start-Sleep -Seconds 30";
        #[cfg(unix)]
        let script = "exec sleep 30";
        for cancel in [false, true] {
            std::fs::File::create(root.join("owned.tmp")).unwrap();
            let mut request = command(script);
            request["timeout_ms"] = json!(if cancel { 30_000 } else { 50 });
            request["cleanup_paths"] = json!(["owned.tmp"]);
            let reply: Value =
                serde_json::from_str(&host.request(&request.to_string()).unwrap()).unwrap();
            let id = reply["id"].as_u64().unwrap();
            if cancel {
                host.request(&json!({"op":"cancel","id":id}).to_string())
                    .unwrap();
            }
            assert_eq!(
                terminal(&mut host, id)["state"],
                if cancel { "cancelled" } else { "timed_out" }
            );
            assert!(!root.join("owned.tmp").exists());
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Reject traversal and enforce tails without depending on process timing.
    fn validates_paths_and_bounds_tail() {
        let (mut host, root) = host();
        let mut request = command("");
        request["read_paths"] = json!(["../outside"]);
        assert!(host.request(&request.to_string()).is_err());
        let mut tail = Tail::default();
        tail.append(&vec![b'a'; TAIL_BYTES]);
        tail.append(b"end");
        assert!(tail.truncated);
        assert_eq!(tail.bytes.len(), TAIL_BYTES);
        assert!(tail.text().ends_with("end"));
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Concurrent jobs are rejected at capacity and host destruction reclaims outputs.
    fn bounds_workers_and_cleans_on_drop() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script = "Start-Sleep -Seconds 30";
        #[cfg(unix)]
        let script = "exec sleep 30";
        for index in 0..4 {
            let filename = format!("owned-{index}.tmp");
            std::fs::File::create(root.join(&filename)).unwrap();
            let mut request = command(script);
            request["cleanup_paths"] = json!([filename]);
            host.request(&request.to_string()).unwrap();
        }
        assert!(host
            .request(&command(script).to_string())
            .unwrap_err()
            .contains("active task limit"));
        let active = host.active.clone();
        drop(host);
        let deadline = Instant::now() + Duration::from_secs(10);
        while active.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "drop did not clean up workers");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// A failed executable launch reaches a terminal state and releases owned output.
    fn launch_failure_releases_output() {
        let (mut host, root) = host();
        std::fs::File::create(root.join("owned.tmp")).unwrap();
        let request = json!({"op":"spawn", "program":root.join("missing-executable").to_str().unwrap(),
            "args":[], "cleanup_paths":["owned.tmp"]});
        let reply: Value =
            serde_json::from_str(&host.request(&request.to_string()).unwrap()).unwrap();
        let result = terminal(&mut host, reply["id"].as_u64().unwrap());
        assert_eq!(result["state"], "failed");
        assert!(result["stderr"]
            .as_str()
            .unwrap()
            .contains("Cannot start process"));
        assert!(!root.join("owned.tmp").exists());
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Large output on both pipes is drained without deadlock or unbounded capture.
    fn drains_large_output_with_bounded_tails() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script = "[Console]::Out.Write(('x' * 2097152) + 'out'); [Console]::Error.Write(('y' * 2097152) + 'err')";
        #[cfg(unix)]
        let script =
            "head -c 2097152 /dev/zero; printf out; head -c 2097152 /dev/zero >&2; printf err >&2";
        let reply: Value =
            serde_json::from_str(&host.request(&command(script).to_string()).unwrap()).unwrap();
        let result = terminal(&mut host, reply["id"].as_u64().unwrap());
        assert_eq!(result["state"], "succeeded");
        for (stream, ending) in [("stdout", "out"), ("stderr", "err")] {
            let text = result[stream].as_str().unwrap();
            assert_eq!(text.len(), TAIL_BYTES);
            assert!(text.ends_with(ending));
            assert_eq!(result[format!("{stream}_truncated")], true);
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Retained terminal handles are bounded independently of active workers.
    fn bounds_terminal_handles() {
        let (mut host, root) = host();
        let request = json!({"op":"spawn", "program":root.join("missing-executable").to_str().unwrap(), "args":[]});
        for _ in 0..32 {
            let reply: Value =
                serde_json::from_str(&host.request(&request.to_string()).unwrap()).unwrap();
            assert_eq!(
                terminal(&mut host, reply["id"].as_u64().unwrap())["state"],
                "failed"
            );
        }
        assert!(host
            .request(&request.to_string())
            .unwrap_err()
            .contains("handle limit"));
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Close can discard an unpolled successful output while ordinary close preserves it.
    fn close_disposal_after_unpolled_success() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script =
            "[System.IO.File]::WriteAllText((Join-Path (Get-Location) 'owned.tmp'), 'payload')";
        #[cfg(unix)]
        let script = "printf payload > owned.tmp";
        for discard in [false, true] {
            std::fs::File::create(root.join("owned.tmp")).unwrap();
            let mut request = command(script);
            request["cleanup_paths"] = json!(["owned.tmp"]);
            let reply: Value =
                serde_json::from_str(&host.request(&request.to_string()).unwrap()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            // Observe the private reservation rather than calling protocol poll: this
            // reproduces a client that does not learn about success before closing.
            while host.active.load(Ordering::Acquire) > 0 {
                assert!(Instant::now() < deadline, "process did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                std::fs::read_to_string(root.join("owned.tmp")).unwrap(),
                "payload"
            );
            host.request(
                &json!({"op":"close", "id":reply["id"], "discard_output":discard}).to_string(),
            )
            .unwrap();
            assert_eq!(root.join("owned.tmp").exists(), !discard);
            if !discard {
                std::fs::remove_file(root.join("owned.tmp")).unwrap();
            }
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    /// Close during execution transfers disposal to the worker before releasing its handle.
    fn close_disposal_while_running() {
        let (mut host, root) = host();
        #[cfg(windows)]
        let script = "Start-Sleep -Seconds 30";
        #[cfg(unix)]
        let script = "exec sleep 30";
        std::fs::File::create(root.join("owned.tmp")).unwrap();
        let mut request = command(script);
        request["cleanup_paths"] = json!(["owned.tmp"]);
        let reply: Value =
            serde_json::from_str(&host.request(&request.to_string()).unwrap()).unwrap();
        let id = reply["id"].as_u64().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.tasks[&id].status.lock().unwrap().state == "queued" {
            assert!(Instant::now() < deadline, "process did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        host.request(&json!({"op":"close", "id":id, "discard_output":true}).to_string())
            .unwrap();
        while host.active.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "closed process was not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("owned.tmp").exists());
        std::fs::remove_dir(root).unwrap();
    }
}
