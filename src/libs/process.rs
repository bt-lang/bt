//! BT process standard library.
//!
//! `process(program)` creates a command object with support for arguments, environment variables,
//! a working directory, synchronous execution, and spawning child processes. The new VM currently
//! uses a synchronous model, so pipe reads and writes provide only basic functionality. Complex
//! asynchronous streaming I/O should be added after the VM gains a task runner.

use crate::libs::bt;
use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Default unread buffer limit for a child-process pipe, preventing large output from increasing
/// memory usage for long-running processes.
const DEFAULT_PROCESS_PIPE_LIMIT: usize = 1024 * 1024;
/// Default size of each system read from a child-process pipe.
const DEFAULT_PROCESS_PIPE_READ_CHUNK: usize = 8 * 1024;
/// Default time a script read waits for a child-process pipe.
const DEFAULT_PROCESS_PIPE_TIMEOUT_MS: u64 = 100;
/// Environment variable for the unread buffer limit of a child-process pipe.
const PROCESS_PIPE_LIMIT_ENV: &str = "BT_PROCESS_PIPE_LIMIT";
/// Environment variable for the size of each read from a child-process pipe.
const PROCESS_PIPE_READ_CHUNK_ENV: &str = "BT_PROCESS_PIPE_READ_CHUNK";
/// Environment variable for the number of milliseconds a script read waits on a child-process pipe.
const PROCESS_PIPE_TIMEOUT_ENV: &str = "BT_PROCESS_PIPE_TIMEOUT_MS";
/// Startup flag that prevents a Windows child process from creating a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Process library object.
#[derive(Debug)]
pub struct BtProcess {
    /// Executable program.
    program: String,
    /// Command arguments.
    args: Vec<String>,
    /// Environment variable overrides.
    envs: IndexMap<String, String>,
    /// Whether to clear the inherited environment.
    env_clear: bool,
    /// Working directory.
    current_dir: Option<String>,
    /// Standard input text.
    stdin: Option<String>,
    /// Whether to inherit the parent process's stdio.
    inherit_stdio: bool,
    /// Whether to discard stdio.
    null_stdio: bool,
    /// Whether to hide the console window of a Windows child process.
    window_hidden: bool,
    /// Override for the unread buffer limit of each stdout/stderr pipe.
    pipe_limit: Option<usize>,
    /// Spawned child process.
    child: Rc<RefCell<Option<Child>>>,
    /// State of the spawned child process's stdout/stderr pipes.
    pipes: Rc<RefCell<ProcessPipeSet>>,
}

impl Clone for BtProcess {
    /// Clones the process configuration; the child-process handle remains shared so that
    /// `child().pid().wait()` chains access the same process.
    fn clone(&self) -> Self {
        Self {
            program: self.program.clone(),
            args: self.args.clone(),
            envs: self.envs.clone(),
            env_clear: self.env_clear,
            current_dir: self.current_dir.clone(),
            stdin: self.stdin.clone(),
            inherit_stdio: self.inherit_stdio,
            null_stdio: self.null_stdio,
            window_hidden: self.window_hidden,
            pipe_limit: self.pipe_limit,
            child: self.child.clone(),
            pipes: self.pipes.clone(),
        }
    }
}

impl PartialEq for BtProcess {
    /// Compares process objects by their shared child-process handles.
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.child, &other.child)
    }
}

impl BtProcess {
    /// Creates a process object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let program = args
            .first()
            .map(Value::to_string)
            .ok_or_else(|| "process() requires a program path argument".to_string())?;
        Ok(Value::Process(Self {
            program,
            args: Vec::new(),
            envs: IndexMap::new(),
            env_clear: false,
            current_dir: None,
            stdin: None,
            inherit_stdio: false,
            null_stdio: false,
            window_hidden: true,
            pipe_limit: None,
            child: Rc::new(RefCell::new(None)),
            pipes: Rc::new(RefCell::new(ProcessPipeSet::default())),
        }))
    }

    /// Calls a process method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "arg" => Ok(Value::Process(self.with_arg(args.first()))),
            "args" => Ok(Value::Process(self.with_args(args.first()))),
            "env" => Ok(Value::Process(self.with_env(&args)?)),
            "envs" => Ok(Value::Process(self.with_envs(args.first()))),
            "current_dir" => Ok(Value::Process(self.with_current_dir(args.first()))),
            "stdin" => Ok(Value::Process(self.with_stdin(args.first()))),
            "inherit_stdio" => Ok(Value::Process(self.with_inherit_stdio())),
            "null_stdio" => Ok(Value::Process(self.with_null_stdio())),
            "window_hidden" => Ok(Value::Process(self.with_window_hidden())),
            "pipe_limit" => Ok(Value::Process(self.with_pipe_limit(&args)?)),
            "env_clear" => Ok(Value::Process(self.with_env_clear())),
            "env_remove" => Ok(Value::Process(self.with_env_remove(args.first()))),
            "get_args" => Ok(Value::Array(Rc::new(RefCell::new(
                self.args.iter().cloned().map(Value::Str).collect(),
            )))),
            "get_current_dir" => Ok(self
                .current_dir
                .clone()
                .map(Value::Str)
                .unwrap_or(Value::Empty)),
            "get_envs" => Ok(Value::Object(Rc::new(RefCell::new(
                self.envs
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::Str(value.clone())))
                    .collect(),
            )))),
            "get_program" => Ok(Value::Str(self.program.clone())),
            "status" => self.status(),
            "output" => self.output(),
            "child" => self.child(),
            "pid" => Ok(self
                .child
                .borrow()
                .as_ref()
                .map(|child| Value::Int(child.id() as i64))
                .unwrap_or(Value::Empty)),
            "kill" => self.kill(),
            "suspend" => self.suspend(),
            "resume" => self.resume(),
            "wait" => self.wait(),
            "try_wait" => self.try_wait(),
            "child_running" => Ok(Value::Bool(self.child.borrow().is_some())),
            "stdout" => self.pipe_status(ProcessPipeKind::Stdout),
            "stdout_read" => self.pipe_read(ProcessPipeKind::Stdout),
            "stderr_read" => self.pipe_read(ProcessPipeKind::Stderr),
            "stdout_read_lines" => self.pipe_read_lines(ProcessPipeKind::Stdout),
            "stderr_read_lines" => self.pipe_read_lines(ProcessPipeKind::Stderr),
            _ => Err(format!("process library has no `{}` method", method)),
        }
    }

    /// Adds a single argument.
    fn with_arg(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        if let Some(value) = value {
            next.args.push(value.to_string());
        }
        next
    }

    /// Adds a list of arguments.
    fn with_args(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        if let Some(Value::Array(items)) = value {
            next.args
                .extend(items.borrow().iter().map(Value::to_string));
        }
        next
    }

    /// Sets a single environment variable.
    fn with_env(&self, args: &[Value]) -> Result<Self, String> {
        let key = args
            .first()
            .map(Value::to_string)
            .ok_or_else(|| "process.env() is missing the variable name".to_string())?;
        let value = args.get(1).map(Value::to_string).unwrap_or_default();
        let mut next = self.clone();
        next.envs.insert(key, value);
        Ok(next)
    }

    /// Sets multiple environment variables.
    fn with_envs(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        if let Some(Value::Object(items)) = value {
            for (key, value) in items.borrow().iter() {
                next.envs.insert(key.clone(), value.to_string());
            }
        }
        next
    }

    /// Sets the working directory.
    fn with_current_dir(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.current_dir = value.map(Value::to_string);
        next
    }

    /// Sets standard input.
    fn with_stdin(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.stdin = value.map(Value::to_string);
        next
    }

    /// Clears the inherited environment.
    fn with_env_clear(&self) -> Self {
        let mut next = self.clone();
        next.env_clear = true;
        next
    }

    /// Removes an environment variable override.
    fn with_env_remove(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        if let Some(value) = value {
            next.envs.shift_remove(&value.to_string());
        }
        next
    }

    /// Inherits the parent process's stdio.
    fn with_inherit_stdio(&self) -> Self {
        let mut next = self.clone();
        next.inherit_stdio = true;
        next.null_stdio = false;
        next.window_hidden = false;
        next
    }

    /// Discards the child process's stdio.
    fn with_null_stdio(&self) -> Self {
        let mut next = self.clone();
        next.null_stdio = true;
        next.inherit_stdio = false;
        next.window_hidden = true;
        next
    }

    /// Hides the Windows child process's console window.
    fn with_window_hidden(&self) -> Self {
        let mut next = self.clone();
        next.window_hidden = true;
        next.inherit_stdio = false;
        next
    }

    /// Sets the unread buffer limit for child-process stdout/stderr pipes.
    fn with_pipe_limit(&self, args: &[Value]) -> Result<Self, String> {
        let mut next = self.clone();
        next.pipe_limit = Some(required_positive_usize_arg(
            args,
            "process.pipe_limit()",
            "byte count",
        )?);
        Ok(next)
    }

    /// Builds the command.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if self.env_clear {
            command.env_clear();
        }
        bt::apply_env_overlay(&mut command);
        command.envs(self.envs.iter());
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        if self.inherit_stdio {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        } else if self.null_stdio {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        } else if self.stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        self.apply_window_hidden(&mut command);
        command
    }

    /// Builds the command for `child()` and opens readable pipes as needed.
    fn child_command(&self) -> Command {
        let mut command = self.command();
        if !self.inherit_stdio && !self.null_stdio {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            if self.stdin.is_some() {
                command.stdin(Stdio::piped());
            }
        }
        command
    }

    /// Applies the Windows console-window hiding policy from the current configuration.
    #[cfg(windows)]
    fn apply_window_hidden(&self, command: &mut Command) {
        if self.window_hidden && !self.inherit_stdio {
            command.creation_flags(CREATE_NO_WINDOW);
        }
    }

    /// No console-window hiding policy is needed on non-Windows platforms.
    #[cfg(not(windows))]
    fn apply_window_hidden(&self, _command: &mut Command) {}

    /// Executes synchronously and returns the status code.
    fn status(&self) -> Result<Value, String> {
        self.command()
            .status()
            .map(|status| Value::Int(status.code().unwrap_or(-1) as i64))
            .map_err(|err| format!("failed to execute process `{}`: {}", self.program, err))
    }

    /// Executes synchronously and returns the output object.
    fn output(&self) -> Result<Value, String> {
        let output = self
            .command()
            .output()
            .map_err(|err| format!("failed to execute process `{}`: {}", self.program, err))?;
        let mut object = IndexMap::new();
        object.insert(
            "status".to_string(),
            Value::Int(output.status.code().unwrap_or(-1) as i64),
        );
        object.insert(
            "stdout".to_string(),
            Value::Str(String::from_utf8_lossy(&output.stdout).to_string()),
        );
        object.insert(
            "stderr".to_string(),
            Value::Str(String::from_utf8_lossy(&output.stderr).to_string()),
        );
        Ok(Value::Object(Rc::new(RefCell::new(object))))
    }

    /// Spawns a child process.
    fn child(&self) -> Result<Value, String> {
        let config = match self.pipe_limit {
            Some(limit) => ProcessPipeConfig::from_env()?.with_limit(limit)?,
            None => ProcessPipeConfig::from_env()?,
        };
        let mut child = self
            .child_command()
            .spawn()
            .map_err(|err| format!("failed to spawn process `{}`: {}", self.program, err))?;
        if let Some(stdin) = &self.stdin {
            if let Some(mut child_stdin) = child.stdin.take() {
                if let Err(err) = child_stdin.write_all(stdin.as_bytes()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("failed to write to process stdin: {}", err));
                }
            }
        }
        let stdout = child
            .stdout
            .take()
            .map(|pipe| ProcessPipeReader::stdout(pipe, config));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| ProcessPipeReader::stderr(pipe, config));
        *self.pipes.borrow_mut() = ProcessPipeSet { stdout, stderr };
        *self.child.borrow_mut() = Some(child);
        Ok(Value::Process(self.clone()))
    }

    /// Kills the child process.
    fn kill(&self) -> Result<Value, String> {
        let mut child = self.child.borrow_mut();
        let Some(child) = child.as_mut() else {
            return Ok(Value::Bool(false));
        };
        if child
            .try_wait()
            .map_err(|err| format!("failed to read process status: {}", err))?
            .is_some()
        {
            return Ok(Value::Bool(false));
        }
        terminate_child_process_tree(child).map(Value::Bool)
    }

    /// Suspends the spawned child process and its descendant process tree.
    fn suspend(&self) -> Result<Value, String> {
        self.control_process_tree_threads(ProcessThreadAction::Suspend)
            .map(Value::Bool)
    }

    /// Resumes the suspended child process and its descendant process tree.
    fn resume(&self) -> Result<Value, String> {
        self.control_process_tree_threads(ProcessThreadAction::Resume)
            .map(Value::Bool)
    }

    /// Suspends or resumes the spawned child-process tree at the thread level.
    fn control_process_tree_threads(&self, action: ProcessThreadAction) -> Result<bool, String> {
        let mut child = self.child.borrow_mut();
        let Some(child) = child.as_mut() else {
            return Ok(false);
        };
        if child
            .try_wait()
            .map_err(|err| format!("failed to read process status: {}", err))?
            .is_some()
        {
            return Ok(false);
        }
        control_process_tree_threads(child.id(), action)
    }

    /// Waits for the child process to exit.
    fn wait(&self) -> Result<Value, String> {
        let mut child = self.child.borrow_mut();
        let Some(child) = child.as_mut() else {
            return Ok(Value::Empty);
        };
        child
            .wait()
            .map(|status| Value::Int(status.code().unwrap_or(-1) as i64))
            .map_err(|err| format!("failed to wait for process: {}", err))
    }

    /// Attempts to read the child process's exit status.
    fn try_wait(&self) -> Result<Value, String> {
        let mut child = self.child.borrow_mut();
        let Some(child) = child.as_mut() else {
            return Ok(Value::Empty);
        };
        child
            .try_wait()
            .map(|status| {
                status
                    .map(|s| Value::Int(s.code().unwrap_or(-1) as i64))
                    .unwrap_or(Value::Empty)
            })
            .map_err(|err| format!("failed to read process status: {}", err))
    }

    /// Returns the buffer status object for the specified child-process pipe.
    fn pipe_status(&self, kind: ProcessPipeKind) -> Result<Value, String> {
        let Some(pipe) = self.pipes.borrow().get(kind).cloned() else {
            return Ok(Value::Empty);
        };
        Ok(pipe.status_value())
    }

    /// Reads the currently available text from the specified child-process pipe.
    fn pipe_read(&self, kind: ProcessPipeKind) -> Result<Value, String> {
        let Some(pipe) = self.pipes.borrow().get(kind).cloned() else {
            return Ok(Value::Empty);
        };
        pipe.read_text()
            .map(|value| value.map(Value::Str).unwrap_or(Value::Empty))
    }

    /// Reads the currently available text from the specified child-process pipe by line.
    fn pipe_read_lines(&self, kind: ProcessPipeKind) -> Result<Value, String> {
        let Some(pipe) = self.pipes.borrow().get(kind).cloned() else {
            return Ok(Value::Empty);
        };
        let Some(text) = pipe.read_text()? else {
            return Ok(Value::Empty);
        };
        let lines = text
            .lines()
            .map(|line| Value::Str(line.to_string()))
            .collect();
        Ok(Value::Array(Rc::new(RefCell::new(lines))))
    }
}

/// Process-tree thread-control action.
#[derive(Clone, Copy)]
enum ProcessThreadAction {
    /// Suspends all accessible threads in the target process tree.
    Suspend,
    /// Resumes all accessible threads in the target process tree.
    Resume,
}

impl Drop for BtProcess {
    /// Terminates a still-running child process when the last Process reference is dropped, so a
    /// background process is not left behind after the desktop app exits.
    fn drop(&mut self) {
        if Rc::strong_count(&self.child) != 1 {
            return;
        }
        let Ok(mut child_slot) = self.child.try_borrow_mut() else {
            return;
        };
        let Some(child) = child_slot.as_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if terminate_child_process_tree(child).unwrap_or(false) {
            let _ = child.wait();
        }
    }
}

/// Terminates the spawned child process and its descendant process tree.
#[cfg(windows)]
pub(crate) fn terminate_child_process_tree(child: &mut Child) -> Result<bool, String> {
    let pid = child.id();
    match terminate_windows_process_tree(pid) {
        Ok(true) => Ok(true),
        Ok(false) => child
            .kill()
            .map(|_| true)
            .map_err(|err| format!("failed to terminate process: {}", err)),
        Err(err) => child.kill().map(|_| true).map_err(|fallback| {
            format!("{}; fallback process termination failed: {}", err, fallback)
        }),
    }
}

/// Terminates the spawned child process.
#[cfg(not(windows))]
pub(crate) fn terminate_child_process_tree(child: &mut Child) -> Result<bool, String> {
    child
        .kill()
        .map(|_| true)
        .map_err(|err| format!("failed to terminate process: {}", err))
}

/// Recursively terminates a process tree by parent-child relationships on Windows.
#[cfg(windows)]
fn terminate_windows_process_tree(root_pid: u32) -> Result<bool, String> {
    let processes = snapshot_windows_process_parents()?;
    let descendants = collect_descendant_pids(&processes, root_pid);
    let mut terminated = false;
    for pid in descendants.iter().rev().copied() {
        terminated |= terminate_windows_pid(pid)?;
    }
    terminated |= terminate_windows_pid(root_pid)?;
    Ok(terminated)
}

/// Reads the `(pid, parent_pid)` table from the current process snapshot on Windows.
#[cfg(windows)]
fn snapshot_windows_process_parents() -> Result<Vec<(u32, u32)>, String> {
    use std::mem;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "failed to read Windows process snapshot: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut output = Vec::new();
        let mut entry: PROCESSENTRY32W = mem::zeroed();
        entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                output.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Ok(output)
    }
}

/// Collects all descendants of a specified root process from a process parent-child table.
#[cfg(any(windows, test))]
fn collect_descendant_pids(processes: &[(u32, u32)], root_pid: u32) -> Vec<u32> {
    let mut output = Vec::new();
    collect_descendant_pids_inner(processes, root_pid, &mut output);
    output
}

/// Recursively collects descendant processes, keeping parents before their children.
#[cfg(any(windows, test))]
fn collect_descendant_pids_inner(processes: &[(u32, u32)], parent_pid: u32, output: &mut Vec<u32>) {
    for (pid, process_parent) in processes.iter().copied() {
        if process_parent == parent_pid && pid != parent_pid && !output.contains(&pid) {
            output.push(pid);
            collect_descendant_pids_inner(processes, pid, output);
        }
    }
}

/// Terminates a single PID on Windows; returns false if the process has exited or cannot be opened.
#[cfg(windows)]
fn terminate_windows_pid(pid: u32) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Ok(false);
        }
        let terminated = TerminateProcess(handle, 1) != 0;
        let error = std::io::Error::last_os_error();
        CloseHandle(handle);
        if !terminated {
            return Err(format!("failed to terminate process {}: {}", pid, error));
        }
        Ok(true)
    }
}

/// Suspends or resumes the spawned child-process tree at the thread level.
#[cfg(windows)]
fn control_process_tree_threads(
    root_pid: u32,
    action: ProcessThreadAction,
) -> Result<bool, String> {
    let processes = snapshot_windows_process_parents()?;
    let mut pids = collect_descendant_pids(&processes, root_pid);
    pids.push(root_pid);

    let mut changed = false;
    for pid in pids {
        changed |= control_windows_pid_threads(pid, action)?;
    }
    Ok(changed)
}

/// Process-tree suspension and resumption are not currently available on non-Windows platforms.
#[cfg(not(windows))]
fn control_process_tree_threads(
    _root_pid: u32,
    _action: ProcessThreadAction,
) -> Result<bool, String> {
    Err("the current platform does not support process.suspend()/resume()".to_string())
}

/// Suspends or resumes all threads of the specified process.
#[cfg(windows)]
fn control_windows_pid_threads(pid: u32, action: ProcessThreadAction) -> Result<bool, String> {
    use std::mem;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "failed to read Windows thread snapshot: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut changed = false;
        let mut entry: THREADENTRY32 = mem::zeroed();
        entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == pid {
                    changed |= control_windows_thread(entry.th32ThreadID, action)?;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Ok(changed)
    }
}

/// Suspends or resumes a single Windows thread.
#[cfg(windows)]
fn control_windows_thread(thread_id: u32, action: ProcessThreadAction) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    unsafe {
        let handle = OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id);
        if handle.is_null() {
            return Ok(false);
        }
        let previous_count = match action {
            ProcessThreadAction::Suspend => SuspendThread(handle),
            ProcessThreadAction::Resume => ResumeThread(handle),
        };
        let error = std::io::Error::last_os_error();
        CloseHandle(handle);
        if previous_count == u32::MAX {
            let action_label = match action {
                ProcessThreadAction::Suspend => "suspend",
                ProcessThreadAction::Resume => "resume",
            };
            return Err(format!(
                "failed to {} thread {}: {}",
                action_label, thread_id, error
            ));
        }
        Ok(true)
    }
}

/// Child-process pipe read configuration.
#[derive(Clone, Copy, Debug)]
struct ProcessPipeConfig {
    /// Maximum cumulative number of bytes read from a single pipe.
    limit: usize,
    /// Number of bytes in each background thread system read and the maximum number of bytes
    /// returned by a single script read.
    read_chunk: usize,
    /// Maximum time a script read waits for the background pipe thread when no data is available.
    timeout: Duration,
}

impl ProcessPipeConfig {
    /// Reads the pipe configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        let limit = read_env_usize(PROCESS_PIPE_LIMIT_ENV, DEFAULT_PROCESS_PIPE_LIMIT)?;
        let read_chunk =
            read_env_usize(PROCESS_PIPE_READ_CHUNK_ENV, DEFAULT_PROCESS_PIPE_READ_CHUNK)?;
        let timeout_ms = read_env_u64(PROCESS_PIPE_TIMEOUT_ENV, DEFAULT_PROCESS_PIPE_TIMEOUT_MS)?;
        Self::validate(limit, read_chunk)?;
        Ok(Self {
            limit,
            read_chunk,
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    /// Returns a new configuration with the specified unread buffer limit.
    fn with_limit(mut self, limit: usize) -> Result<Self, String> {
        Self::validate(limit, self.read_chunk)?;
        self.limit = limit;
        Ok(self)
    }

    /// Validates that the pipe read configuration remains usable and has explicit resource bounds.
    fn validate(limit: usize, read_chunk: usize) -> Result<(), String> {
        if limit == 0 {
            return Err(format!("{} must be greater than 0", PROCESS_PIPE_LIMIT_ENV));
        }
        if read_chunk == 0 {
            return Err(format!(
                "{} must be greater than 0",
                PROCESS_PIPE_READ_CHUNK_ENV
            ));
        }
        if read_chunk > limit {
            return Err(format!(
                "{} cannot be greater than {}",
                PROCESS_PIPE_READ_CHUNK_ENV, PROCESS_PIPE_LIMIT_ENV
            ));
        }
        Ok(())
    }
}

/// Reads a positive-integer method argument.
fn required_positive_usize_arg(args: &[Value], method: &str, name: &str) -> Result<usize, String> {
    let value = args
        .first()
        .ok_or_else(|| format!("{} is missing the {} argument", method, name))?;
    let parsed = value
        .to_string()
        .parse::<usize>()
        .map_err(|_| format!("the {} for {} must be a positive integer", name, method))?;
    if parsed == 0 {
        return Err(format!(
            "the {} for {} must be greater than 0",
            name, method
        ));
    }
    Ok(parsed)
}

/// Reads a usize environment variable.
fn read_env_usize(name: &str, default: usize) -> Result<usize, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{} must be a non-negative integer", name)),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(format!("failed to read {}: {}", name, err)),
    }
}

/// Reads a u64 environment variable.
fn read_env_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| format!("{} must be a non-negative integer", name)),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(format!("failed to read {}: {}", name, err)),
    }
}

/// Child-process pipe type.
#[derive(Clone, Copy, Debug)]
enum ProcessPipeKind {
    /// Child process's standard output pipe.
    Stdout,
    /// Child process's standard error pipe.
    Stderr,
}

impl ProcessPipeKind {
    /// Returns the pipe name used in scripts and error messages.
    fn name(self) -> &'static str {
        match self {
            ProcessPipeKind::Stdout => "stdout",
            ProcessPipeKind::Stderr => "stderr",
        }
    }
}

/// Collection of pipes for a spawned child process.
#[derive(Debug, Default)]
struct ProcessPipeSet {
    /// Standard output pipe reader.
    stdout: Option<ProcessPipeReader>,
    /// Standard error pipe reader.
    stderr: Option<ProcessPipeReader>,
}

impl ProcessPipeSet {
    /// Retrieves a reader by pipe type.
    fn get(&self, kind: ProcessPipeKind) -> Option<&ProcessPipeReader> {
        match kind {
            ProcessPipeKind::Stdout => self.stdout.as_ref(),
            ProcessPipeKind::Stderr => self.stderr.as_ref(),
        }
    }
}

/// Script-side reader for a single child-process pipe.
#[derive(Clone, Debug)]
struct ProcessPipeReader {
    /// Pipe type.
    kind: ProcessPipeKind,
    /// Pipe read configuration.
    config: ProcessPipeConfig,
    /// Buffer state shared by the background reader thread and script read methods.
    shared: Arc<ProcessPipeShared>,
}

impl ProcessPipeReader {
    /// Creates a stdout pipe reader.
    fn stdout(pipe: ChildStdout, config: ProcessPipeConfig) -> Self {
        Self::spawn(ProcessPipeKind::Stdout, pipe, config)
    }

    /// Creates a stderr pipe reader.
    fn stderr(pipe: ChildStderr, config: ProcessPipeConfig) -> Self {
        Self::spawn(ProcessPipeKind::Stderr, pipe, config)
    }

    /// Starts a background thread to read a child-process pipe.
    fn spawn<R>(kind: ProcessPipeKind, mut pipe: R, config: ProcessPipeConfig) -> Self
    where
        R: Read + Send + 'static,
    {
        let shared = Arc::new(ProcessPipeShared::default());
        let thread_shared = shared.clone();
        let thread_name = format!("bt-process-{}", kind.name());
        if let Err(err) = thread::Builder::new().name(thread_name).spawn(move || {
            let mut chunk = vec![0_u8; config.read_chunk];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => {
                        thread_shared.mark_closed();
                        break;
                    }
                    Ok(size) => thread_shared.push_bytes(kind, &chunk[..size], config.limit),
                    Err(err) => {
                        thread_shared.mark_error(format!(
                            "failed to read process {}: {}",
                            kind.name(),
                            err
                        ));
                        break;
                    }
                }
            }
        }) {
            shared.mark_error(format!(
                "failed to start process {} reader thread: {}",
                kind.name(),
                err
            ));
        }
        Self {
            kind,
            config,
            shared,
        }
    }

    /// Reads currently available text; waits for the default timeout when there is no data or
    /// close signal.
    fn read_text(&self) -> Result<Option<String>, String> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| format!("failed to read process {} status", self.kind.name()))?;
        let deadline = Instant::now() + self.config.timeout;
        loop {
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if !state.buffer.is_empty() {
                break;
            }
            if state.closed {
                if state.total_read == 0 && !state.empty_close_reported {
                    state.empty_close_reported = true;
                    return Ok(Some(String::new()));
                }
                return Ok(None);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let wait_for = deadline.saturating_duration_since(now);
            let (next_state, wait_result) = self
                .shared
                .ready
                .wait_timeout(state, wait_for)
                .map_err(|_| format!("failed to wait for process {} status", self.kind.name()))?;
            state = next_state;
            if wait_result.timed_out() && state.buffer.is_empty() && !state.closed {
                return Ok(None);
            }
        }

        let take = self.config.read_chunk.min(state.buffer.len());
        let mut bytes = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(byte) = state.buffer.pop_front() {
                bytes.push(byte);
            }
        }
        Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
    }

    /// Returns the current pipe status object.
    fn status_value(&self) -> Value {
        let mut object = IndexMap::new();
        match self.shared.state.lock() {
            Ok(state) => {
                object.insert("kind".to_string(), Value::Str(self.kind.name().to_string()));
                object.insert(
                    "available".to_string(),
                    Value::Int(state.buffer.len() as i64),
                );
                object.insert("closed".to_string(), Value::Bool(state.closed));
                object.insert("overflow".to_string(), Value::Bool(state.overflow));
                object.insert(
                    "total_read".to_string(),
                    Value::Int(state.total_read as i64),
                );
                object.insert("limit".to_string(), Value::Int(self.config.limit as i64));
                object.insert(
                    "read_chunk".to_string(),
                    Value::Int(self.config.read_chunk as i64),
                );
                object.insert(
                    "timeout_ms".to_string(),
                    Value::Int(self.config.timeout.as_millis() as i64),
                );
            }
            Err(_) => {
                object.insert("kind".to_string(), Value::Str(self.kind.name().to_string()));
                object.insert(
                    "error".to_string(),
                    Value::Str("pipe state lock is poisoned".to_string()),
                );
            }
        }
        Value::Object(Rc::new(RefCell::new(object)))
    }
}

/// Shared state of a child-process pipe.
#[derive(Debug)]
struct ProcessPipeShared {
    /// Lock-protected buffer and error state.
    state: Mutex<ProcessPipeState>,
    /// Condition variable used by the background reader thread to notify script readers.
    ready: Condvar,
}

impl Default for ProcessPipeShared {
    /// Creates empty shared pipe state.
    fn default() -> Self {
        Self {
            state: Mutex::new(ProcessPipeState::default()),
            ready: Condvar::new(),
        }
    }
}

impl ProcessPipeShared {
    /// Appends bytes read by the background thread and enters an error state when the unread
    /// buffer exceeds its limit.
    fn push_bytes(&self, kind: ProcessPipeKind, bytes: &[u8], limit: usize) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.error.is_some() {
            self.ready.notify_all();
            return;
        }
        state.total_read = state.total_read.saturating_add(bytes.len());
        if state.buffer.len().saturating_add(bytes.len()) > limit {
            state.overflow = true;
            state.error = Some(format!(
                "process {} unread output exceeds the {} limit of {} bytes",
                kind.name(),
                PROCESS_PIPE_LIMIT_ENV,
                limit
            ));
            self.ready.notify_all();
            return;
        }
        state.buffer.extend(bytes);
        self.ready.notify_all();
    }

    /// Marks the pipe as closed.
    fn mark_closed(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.ready.notify_all();
    }

    /// Marks the pipe read as failed.
    fn mark_error(&self, message: String) {
        if let Ok(mut state) = self.state.lock() {
            state.error = Some(message);
        }
        self.ready.notify_all();
    }
}

/// Child-process pipe buffer state.
#[derive(Debug, Default)]
struct ProcessPipeState {
    /// Bytes that have been read but not yet consumed by the script.
    buffer: VecDeque<u8>,
    /// Cumulative number of bytes read from the system for the current pipe, used only for status
    /// observation.
    total_read: usize,
    /// Whether the pipe has received EOF.
    closed: bool,
    /// Pipe read or resource-limit error.
    error: Option<String>,
    /// Whether the cumulative read limit has been exceeded.
    overflow: bool,
    /// Whether closure with zero-byte output has already been reported to the script as an empty
    /// string.
    empty_close_reported: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as StdMutex, MutexGuard};

    /// Test lock for process-pipe environment variables.
    static PROCESS_PIPE_ENV_LOCK: StdMutex<()> = StdMutex::new(());

    /// Guard that temporarily overrides pipe environment variables during a test.
    struct PipeEnvGuard {
        /// Global lock held while environment variables are modified.
        _lock: MutexGuard<'static, ()>,
        /// Cumulative read-limit environment variable before the test begins.
        previous_limit: Option<String>,
        /// Read-chunk-size environment variable before the test begins.
        previous_read_chunk: Option<String>,
        /// Read-timeout environment variable before the test begins.
        previous_timeout: Option<String>,
    }

    impl PipeEnvGuard {
        /// Clears pipe environment variables and sets the specified values required by the test.
        fn new(values: &[(&str, &str)]) -> Self {
            let lock = PROCESS_PIPE_ENV_LOCK
                .lock()
                .expect("process-pipe environment-variable test lock should be available");
            let guard = Self {
                _lock: lock,
                previous_limit: std::env::var(PROCESS_PIPE_LIMIT_ENV).ok(),
                previous_read_chunk: std::env::var(PROCESS_PIPE_READ_CHUNK_ENV).ok(),
                previous_timeout: std::env::var(PROCESS_PIPE_TIMEOUT_ENV).ok(),
            };
            std::env::remove_var(PROCESS_PIPE_LIMIT_ENV);
            std::env::remove_var(PROCESS_PIPE_READ_CHUNK_ENV);
            std::env::remove_var(PROCESS_PIPE_TIMEOUT_ENV);
            for (key, value) in values {
                std::env::set_var(key, value);
            }
            guard
        }
    }

    impl Drop for PipeEnvGuard {
        /// Restores the pipe environment variables from before the test.
        fn drop(&mut self) {
            restore_env(PROCESS_PIPE_LIMIT_ENV, &self.previous_limit);
            restore_env(PROCESS_PIPE_READ_CHUNK_ENV, &self.previous_read_chunk);
            restore_env(PROCESS_PIPE_TIMEOUT_ENV, &self.previous_timeout);
        }
    }

    /// Restores a single environment variable.
    fn restore_env(name: &str, value: &Option<String>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    /// Constructs a BT array value.
    fn value_array(items: &[&str]) -> Value {
        Value::Array(Rc::new(RefCell::new(
            items
                .iter()
                .map(|value| Value::Str((*value).to_string()))
                .collect(),
        )))
    }

    /// Creates a basic process object.
    fn new_process(program: &str) -> BtProcess {
        let Value::Process(process) = BtProcess::new(vec![Value::Str(program.to_string())])
            .expect("process() should create a process object")
        else {
            panic!("process() should return a Process value");
        };
        process
    }

    /// Creates a process object with arguments.
    fn process_with_args(program: &str, args: &[&str]) -> BtProcess {
        let process = new_process(program);
        let Value::Process(process) = process
            .call_method("args", vec![value_array(args)])
            .expect("process.args() should return a process object")
        else {
            panic!("process.args() should return a Process value");
        };
        process
    }

    /// Creates a process object that executes a platform shell script.
    fn shell_process(script: &str) -> BtProcess {
        if cfg!(windows) {
            process_with_args("cmd", &["/C", script])
        } else {
            process_with_args("sh", &["-c", script])
        }
    }

    /// Creates a process object that writes its standard input to standard output.
    fn stdin_echo_process() -> BtProcess {
        if cfg!(windows) {
            process_with_args("cmd", &["/C", "more"])
        } else {
            process_with_args("cat", &[])
        }
    }

    /// Spawns a child process and waits for it to exit.
    fn run_child_to_exit(process: &BtProcess) {
        process
            .call_method("child", Vec::new())
            .expect("child() should spawn a child process");
        process
            .call_method("wait", Vec::new())
            .expect("wait() should wait for the child process to exit");
    }

    /// Reading stdout before spawning a child process should return empty.
    #[test]
    fn stdout_read_returns_empty_without_child() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            new_process("cmd")
        } else {
            new_process("sh")
        };

        let value = process
            .call_method("stdout_read", Vec::new())
            .expect("reading stdout without a spawned child process should not fail");

        assert_eq!(value, Value::Empty);
    }

    /// A null_stdio child process has no stdout pipe, so reading it should return empty.
    #[test]
    fn stdout_read_returns_empty_without_pipe() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("exit /B 0")
        } else {
            shell_process(":")
        };
        let Value::Process(process) = process
            .call_method("null_stdio", Vec::new())
            .expect("null_stdio() should return a process object")
        else {
            panic!("null_stdio() should return a Process value");
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stdout_read", Vec::new())
            .expect("reading without a stdout pipe should not fail");

        assert_eq!(value, Value::Empty);
    }

    /// `stdout_read` reads text from the child's standard output.
    #[test]
    fn stdout_read_returns_child_output() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("echo BT_PIPE")
        } else {
            shell_process("printf BT_PIPE")
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stdout_read", Vec::new())
            .expect("stdout_read() should read standard output");

        assert!(value.to_string().contains("BT_PIPE"));
    }

    /// `stderr_read` reads text from the child's standard error.
    #[test]
    fn stderr_read_returns_child_error_output() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("echo BT_ERR 1>&2")
        } else {
            shell_process("printf BT_ERR >&2")
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stderr_read", Vec::new())
            .expect("stderr_read() should read standard error");

        assert!(value.to_string().contains("BT_ERR"));
    }

    /// `stdout_read_lines` splits standard output into an array of lines.
    #[test]
    fn stdout_read_lines_returns_line_array() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("echo one&&echo two")
        } else {
            shell_process("printf 'one\\ntwo\\n'")
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stdout_read_lines", Vec::new())
            .expect("stdout_read_lines() should read an array of lines");

        assert_eq!(value.to_string(), "[\"one\",\"two\"]");
    }

    /// The first read of zero-byte stdout should return an empty string; subsequent reads should
    /// return empty.
    #[test]
    fn empty_child_output_is_distinct_from_missing_pipe() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("exit /B 0")
        } else {
            shell_process(":")
        };
        run_child_to_exit(&process);

        let first = process
            .call_method("stdout_read", Vec::new())
            .expect("the first read of zero-byte output should succeed");
        let second = process
            .call_method("stdout_read", Vec::new())
            .expect("the second read of zero-byte output should succeed");

        assert_eq!(first, Value::Str(String::new()));
        assert_eq!(second, Value::Empty);
    }

    /// `read_chunk` limits one script read; repeated reads should
    /// continue consuming the remaining buffer.
    #[test]
    fn stdout_read_respects_read_chunk_and_drains_repeatedly() {
        let _env = PipeEnvGuard::new(&[
            (PROCESS_PIPE_LIMIT_ENV, "64"),
            (PROCESS_PIPE_READ_CHUNK_ENV, "4"),
        ]);
        let process = if cfg!(windows) {
            shell_process("echo abcdef")
        } else {
            shell_process("printf abcdef")
        };
        run_child_to_exit(&process);

        let first = process
            .call_method("stdout_read", Vec::new())
            .expect("the first stdout_read() should succeed");
        let second = process
            .call_method("stdout_read", Vec::new())
            .expect("the second stdout_read() should succeed");

        assert_eq!(first.to_string().len(), 4);
        assert!(!second.to_string().is_empty());
    }

    /// Previously consumed output should not count toward the unread buffer limit.
    #[test]
    fn stdout_pipe_limit_counts_unread_buffer_not_history() {
        let _env = PipeEnvGuard::new(&[
            (PROCESS_PIPE_LIMIT_ENV, "8"),
            (PROCESS_PIPE_READ_CHUNK_ENV, "4"),
            (PROCESS_PIPE_TIMEOUT_ENV, "2000"),
        ]);
        let process = if cfg!(windows) {
            process_with_args(
                "powershell",
                &[
                    "-NoProfile",
                    "-Command",
                    "[Console]::Out.Write('abcd'); Start-Sleep -Milliseconds 200; [Console]::Out.Write('efgh')",
                ],
            )
        } else {
            process_with_args("sh", &["-c", "printf abcd; sleep 0.2; printf efgh"])
        };
        process
            .call_method("child", Vec::new())
            .expect("child() should spawn a child process");

        let first = process
            .call_method("stdout_read", Vec::new())
            .expect("the first stdout_read() should succeed");
        process
            .call_method("wait", Vec::new())
            .expect("wait() should wait for the child process to exit");
        let second = process
            .call_method("stdout_read", Vec::new())
            .expect("the second stdout_read() should succeed");

        assert_eq!(
            format!("{}{}", first.to_string(), second.to_string()),
            "abcdefgh"
        );
    }

    /// Exceeding BT_PROCESS_PIPE_LIMIT with unread buffered data should return a clear error.
    #[test]
    fn stdout_read_reports_pipe_limit_overflow() {
        let _env = PipeEnvGuard::new(&[
            (PROCESS_PIPE_LIMIT_ENV, "8"),
            (PROCESS_PIPE_READ_CHUNK_ENV, "4"),
        ]);
        let process = if cfg!(windows) {
            shell_process("echo abcdefghijkl")
        } else {
            shell_process("printf abcdefghijkl")
        };
        run_child_to_exit(&process);

        let err = process
            .call_method("stdout_read", Vec::new())
            .expect_err("reading should fail after the output limit is exceeded");

        assert!(err.contains(PROCESS_PIPE_LIMIT_ENV));
    }

    /// `pipe_limit()` overrides the unread-buffer limit for this process object.
    #[test]
    fn pipe_limit_overrides_process_pipe_limit_env() {
        let _env = PipeEnvGuard::new(&[
            (PROCESS_PIPE_LIMIT_ENV, "8"),
            (PROCESS_PIPE_READ_CHUNK_ENV, "4"),
        ]);
        let process = if cfg!(windows) {
            shell_process("echo abcdefghijkl")
        } else {
            shell_process("printf abcdefghijkl")
        };
        let Value::Process(process) = process
            .call_method("pipe_limit", vec![Value::Int(64)])
            .expect("pipe_limit() should return a process object")
        else {
            panic!("pipe_limit() should return a Process value");
        };
        run_child_to_exit(&process);

        let mut output = String::new();
        for _ in 0..8 {
            match process
                .call_method("stdout_read", Vec::new())
                .expect("stdout_read() should succeed after increasing pipe_limit")
            {
                Value::Str(text) => output.push_str(&text),
                Value::Empty => break,
                other => panic!(
                    "stdout_read() should return a string or empty; got {:?}",
                    other
                ),
            }
        }

        assert!(output.contains("abcdefgh"));
    }

    /// `child()` writes configured stdin and closes the pipe; `stdout_read` must not
    /// deadlock.
    #[test]
    fn stdin_text_can_be_read_back_from_stdout_without_deadlock() {
        let _env = PipeEnvGuard::new(&[]);
        let process = stdin_echo_process();
        let Value::Process(process) = process
            .call_method("stdin", vec![Value::Str("BT_STDIN\n".to_string())])
            .expect("stdin() should return a process object")
        else {
            panic!("stdin() should return a Process value");
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stdout_read", Vec::new())
            .expect("stdout_read() should read the stdin echo");

        assert!(value.to_string().contains("BT_STDIN"));
    }

    /// When child() has no configured stdin, it should close the child process's input to prevent
    /// the desktop app from inheriting an invalid handle or the child process from waiting for input.
    #[test]
    fn child_without_stdin_closes_child_input() {
        let _env = PipeEnvGuard::new(&[(PROCESS_PIPE_TIMEOUT_ENV, "20")]);
        let process = stdin_echo_process();
        process
            .call_method("child", Vec::new())
            .expect("child() should spawn a child process");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match process
                .call_method("try_wait", Vec::new())
                .expect("try_wait() should check the child process status")
            {
                Value::Int(_) => break,
                Value::Empty if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = process.call_method("kill", Vec::new());
                    panic!("a child process without configured stdin should receive EOF and exit");
                }
            }
        }

        let value = process
            .call_method("stdout_read", Vec::new())
            .expect("reading zero-byte stdout should not fail");

        assert!(value.to_string().trim().is_empty());
    }

    /// `window_hidden()` restores non-interactive background spawning, while
    /// inherit_stdio() should preserve explicit console interaction.
    #[test]
    fn window_hidden_tracks_interactive_stdio_choice() {
        let process = if cfg!(windows) {
            new_process("cmd")
        } else {
            new_process("sh")
        };
        assert!(process.window_hidden);
        assert!(!process.inherit_stdio);

        let Value::Process(process) = process
            .call_method("inherit_stdio", Vec::new())
            .expect("inherit_stdio() should return a process object")
        else {
            panic!("inherit_stdio() should return a Process value");
        };
        assert!(process.inherit_stdio);
        assert!(!process.window_hidden);

        let Value::Process(process) = process
            .call_method("window_hidden", Vec::new())
            .expect("window_hidden() should return a process object")
        else {
            panic!("window_hidden() should return a Process value");
        };
        assert!(!process.inherit_stdio);
        assert!(process.window_hidden);
    }

    /// `stdout()` returns an observable status object for the current pipe.
    #[test]
    fn stdout_returns_pipe_status_object() {
        let _env = PipeEnvGuard::new(&[]);
        let process = if cfg!(windows) {
            shell_process("echo BT_STATUS")
        } else {
            shell_process("printf BT_STATUS")
        };
        run_child_to_exit(&process);

        let value = process
            .call_method("stdout", Vec::new())
            .expect("stdout() should return pipe status");

        let Value::Object(object) = value else {
            panic!("stdout() should return an object");
        };
        let object = object.borrow();
        assert_eq!(
            object.get("kind"),
            Some(&Value::Str(ProcessPipeKind::Stdout.name().to_string()))
        );
        assert!(matches!(object.get("limit"), Some(Value::Int(_))));
    }

    /// Windows process-tree collection should retain nested child processes in parent-child order.
    #[test]
    fn collect_descendant_pids_returns_nested_children() {
        let processes = [(10, 1), (11, 10), (12, 11), (13, 10), (14, 99)];

        let descendants = collect_descendant_pids(&processes, 10);

        assert_eq!(descendants, vec![11, 12, 13]);
    }
}
