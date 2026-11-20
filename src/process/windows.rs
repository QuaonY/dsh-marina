//! Windows backend: the process is created suspended and assigned to a Job
//! Object before its first instruction runs. This closes the race where a
//! child could escape the job by spawning before assignment.

use super::{AdoptableInstance, OutputCapture, ProcessError, ProcessStats, StopOutcome};
use crate::launch::LaunchSpec;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
    HWND, LPARAM, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList, QueryInformationJobObject,
    TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SW_RESTORE,
    SetForegroundWindow, ShowWindow, WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

static BREAKAWAY_DENIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marina runs inside a Job that does not allow breakaway, so instances
/// started now may be terminated together with that outer Job (for example
/// when the terminal that launched Marina closes).
pub fn breakaway_denied() -> bool {
    BREAKAWAY_DENIED.load(std::sync::atomic::Ordering::Relaxed)
}

fn current_process_in_job() -> bool {
    let mut result = BOOL(0);
    unsafe { IsProcessInJob(GetCurrentProcess(), None, &mut result) }.is_ok() && result.as_bool()
}

const INFINITE: u32 = u32::MAX;
const TERMINATED_EXIT_CODE: u32 = 1;

impl From<windows::core::Error> for ProcessError {
    fn from(error: windows::core::Error) -> Self {
        ProcessError::Platform(format!("Windows 进程操作失败：{error}"))
    }
}

/// A DSH process that was already running before Marina started.  Windows does
/// not allow adding an existing process to a Job Object, so adoption uses
/// process-tree scanning for statistics and termination instead.
pub struct AdoptedProcess {
    process: HANDLE,
    pub(crate) process_id: u32,
    pub(crate) started_at: Instant,
}

unsafe impl Send for AdoptedProcess {}

impl AdoptedProcess {
    pub(crate) fn open(process_id: u32) -> Result<Self, ProcessError> {
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                process_id,
            )?
        };
        Ok(Self {
            process,
            process_id,
            started_at: Instant::now(),
        })
    }

    pub(crate) fn is_running(&self) -> Result<bool, ProcessError> {
        match unsafe { WaitForSingleObject(self.process, 0) }.0 {
            value if value == WAIT_TIMEOUT.0 => Ok(true),
            value if value == WAIT_OBJECT_0.0 => Ok(false),
            _ => Err(ProcessError::WaitFailed),
        }
    }

    pub(crate) fn exit_code(&self) -> Result<Option<u32>, ProcessError> {
        if self.is_running()? {
            return Ok(None);
        }
        let mut exit_code = 0;
        unsafe {
            GetExitCodeProcess(self.process, &mut exit_code)?;
        }
        Ok(Some(exit_code))
    }

    pub(crate) fn stats(&self) -> Result<ProcessStats, ProcessError> {
        let tree = process_tree(self.process_id)?;
        let memory_bytes = tree
            .iter()
            .filter(|entry| entry.in_tree)
            .map(|entry| entry.memory_bytes)
            .sum();
        let process_count = tree.iter().filter(|entry| entry.in_tree).count() as u32;
        Ok(ProcessStats {
            memory_bytes,
            process_count,
            runtime_seconds: self.started_at.elapsed().as_secs(),
        })
    }

    pub(crate) fn stop(self, grace_period: Duration) -> Result<StopOutcome, ProcessError> {
        if !self.is_running()? {
            return Ok(StopOutcome::AlreadyExited);
        }
        unsafe {
            EnumWindows(Some(close_process_window), LPARAM(self.process_id as isize))?;
        }
        let wait_ms = grace_period.as_millis().min((u32::MAX - 1) as u128) as u32;
        match unsafe { WaitForSingleObject(self.process, wait_ms) }.0 {
            value if value == WAIT_OBJECT_0.0 => Ok(StopOutcome::ExitedGracefully),
            value if value == WAIT_TIMEOUT.0 => {
                terminate_process_tree(self.process_id)?;
                let _ = unsafe { WaitForSingleObject(self.process, INFINITE) };
                Ok(StopOutcome::Forced)
            }
            _ => Err(ProcessError::WaitFailed),
        }
    }
}

impl Drop for AdoptedProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TreeEntry {
    process_id: u32,
    parent_process_id: u32,
    memory_bytes: u64,
    in_tree: bool,
}

fn process_tree(root_process_id: u32) -> Result<Vec<TreeEntry>, ProcessError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let mut entries = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            entries.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                entry.szExeFile,
            ));
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }

    let mut members = std::collections::HashSet::new();
    members.insert(root_process_id);
    loop {
        let mut added = false;
        for (process_id, parent_process_id, _) in &entries {
            if members.contains(parent_process_id) && members.insert(*process_id) {
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    Ok(entries
        .into_iter()
        .map(|(process_id, parent_process_id, _)| {
            let in_tree = members.contains(&process_id);
            TreeEntry {
                process_id,
                parent_process_id,
                // Only members need memory: querying every process in the
                // system snapshot costs hundreds of syscalls per monitor tick.
                memory_bytes: if in_tree {
                    process_memory(process_id)
                } else {
                    0
                },
                in_tree,
            }
        })
        .collect())
}

fn process_memory(process_id: u32) -> u64 {
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return 0;
    };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let memory =
        if unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb).as_bool() } {
            counters.WorkingSetSize as u64
        } else {
            0
        };
    unsafe {
        let _ = CloseHandle(process);
    }
    memory
}

fn terminate_process_tree(root_process_id: u32) -> Result<(), ProcessError> {
    let tree = process_tree(root_process_id)?;
    let mut members = tree
        .iter()
        .filter(|entry| entry.in_tree && entry.process_id != root_process_id)
        .collect::<Vec<_>>();
    // Children are terminated before their parents so the tree cannot reparent
    // while it is being walked.
    members.sort_by_key(|entry| std::cmp::Reverse(depth_of(&tree, entry.process_id)));
    for entry in members {
        if let Ok(process) = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                false,
                entry.process_id,
            )
        } {
            unsafe {
                let _ = TerminateProcess(process, TERMINATED_EXIT_CODE);
                let _ = CloseHandle(process);
            }
        }
    }
    if let Ok(process) = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            false,
            root_process_id,
        )
    } {
        unsafe {
            let _ = TerminateProcess(process, TERMINATED_EXIT_CODE);
            let _ = CloseHandle(process);
        }
    }
    Ok(())
}

fn depth_of(tree: &[TreeEntry], process_id: u32) -> usize {
    let mut depth = 0;
    let mut current = process_id;
    for _ in 0..64 {
        let Some(entry) = tree.iter().find(|entry| entry.process_id == current) else {
            break;
        };
        if entry.parent_process_id == current || entry.parent_process_id == 0 {
            break;
        }
        current = entry.parent_process_id;
        depth += 1;
    }
    depth
}

/// Find DSH processes whose `--user-data-dir` points at a Marina instance.
pub fn scan_instance_processes(
    repository: &crate::instances::InstanceRepository,
) -> Result<Vec<AdoptableInstance>, ProcessError> {
    let mut targets = HashMap::<String, String>::new();
    for instance in repository
        .list()
        .map_err(|error| ProcessError::InvalidLaunchInput(error.to_string()))?
    {
        if let Ok(paths) = repository.paths(&instance.id) {
            targets.insert(normalize_path(&paths.electron), instance.id);
        }
    }
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let mut candidates = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let name = String::from_utf16_lossy(
                &entry.szExeFile[..entry
                    .szExeFile
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(entry.szExeFile.len())],
            );
            if name.eq_ignore_ascii_case(crate::runtimes::DSH_EXECUTABLE_NAME)
                && let Some(command_line) = process_command_line(entry.th32ProcessID)
                && let Some(user_data) = user_data_argument(&command_line)
                && let Some(instance_id) = targets.get(&normalize_path(Path::new(&user_data)))
            {
                candidates.push(AdoptableInstance {
                    instance_id: instance_id.clone(),
                    process_id: entry.th32ProcessID,
                });
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    Ok(candidates)
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn user_data_argument(command_line: &str) -> Option<String> {
    const PREFIX: &str = "--user-data-dir=";
    let position = command_line.find(PREFIX)?;
    let start = position + PREFIX.len();
    let rest = &command_line[start..];
    // Windows quotes the whole argument (`"--user-data-dir=C:\a b"`) when the
    // path has spaces, so a quote before the prefix ends at the next quote.
    if command_line[..position].ends_with('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        let value = &rest[..end];
        return (!value.is_empty()).then(|| value.to_owned());
    }
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].to_owned());
    }
    let end = rest.find(' ').unwrap_or(rest.len());
    let value = rest[..end].trim_end_matches('"');
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        information_class: u32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

fn process_command_line(process_id: u32) -> Option<String> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut returned = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            PROCESS_COMMAND_LINE_INFORMATION,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
            &mut returned,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    if status < 0 {
        return None;
    }
    if (returned as usize) < std::mem::size_of::<UnicodeString>() {
        return None;
    }
    let start = buffer.as_ptr() as usize;
    let end = start + buffer.len();
    // The buffer has no guaranteed alignment for the header, and the returned
    // pointer/length come from another process: read it unaligned and check
    // that the string lies inside the buffer we allocated.
    let header = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<UnicodeString>()) };
    if header.buffer.is_null() || header.length == 0 || header.length % 2 != 0 {
        return None;
    }
    let string_start = header.buffer as usize;
    let string_end = string_start.checked_add(header.length as usize)?;
    if string_start < start || string_end > end {
        return None;
    }
    let units = unsafe { std::slice::from_raw_parts(header.buffer, header.length as usize / 2) };
    Some(String::from_utf16_lossy(units))
}

/// Find a running process whose executable path matches `executable`.
///
/// Used to manage the original DSH installation, which Marina does not launch
/// through a Job Object.
pub fn find_process_by_executable(executable: &Path) -> Result<Option<u32>, ProcessError> {
    let expected = executable
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = None;
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let process_id = entry.th32ProcessID;
            if process_id != 0
                && let Ok(process) =
                    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            {
                let mut buffer = [0u16; 1024];
                let mut length = buffer.len() as u32;
                let result = unsafe {
                    QueryFullProcessImageNameW(
                        process,
                        windows::Win32::System::Threading::PROCESS_NAME_FORMAT(0),
                        PWSTR(buffer.as_mut_ptr()),
                        &mut length,
                    )
                };
                let _ = unsafe { CloseHandle(process) };
                if result.is_ok() {
                    let path = String::from_utf16_lossy(&buffer[..length as usize])
                        .replace('/', "\\")
                        .to_ascii_lowercase();
                    if path == expected {
                        found = Some(process_id);
                        break;
                    }
                }
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    Ok(found)
}

/// Ask a process to close, wait, then terminate its tree.  This is the
/// non-Job-Object path used for adopted and external processes.
pub fn stop_process_tree_by_id(
    process_id: u32,
    grace_period: Duration,
) -> Result<StopOutcome, ProcessError> {
    AdoptedProcess::open(process_id)?.stop(grace_period)
}

/// Start `spec` outside any Job and without tracking it.  Used to hand a
/// `dsh://` link to a running instance: Electron's single-instance lock makes
/// this short-lived process forward its arguments and exit.
pub fn spawn_detached(spec: &LaunchSpec) -> Result<u32, ProcessError> {
    let mut command = std::process::Command::new(&spec.executable);
    command
        .args(&spec.arguments)
        .current_dir(&spec.working_directory);
    for name in &spec.environment_to_remove {
        command.env_remove(name);
    }
    for (name, value) in &spec.environment {
        command.env(name, value);
    }
    let child = command
        .spawn()
        .map_err(|e| ProcessError::InvalidLaunchInput(e.to_string()))?;
    Ok(child.id())
}

/// Every descendant of `root_process_id` (excluding the root itself).
pub fn descendant_process_ids(root_process_id: u32) -> Vec<u32> {
    process_tree(root_process_id)
        .map(|entries| {
            entries
                .into_iter()
                .filter(|entry| entry.in_tree && entry.process_id != root_process_id)
                .map(|entry| entry.process_id)
                .collect()
        })
        .unwrap_or_default()
}

/// Bring the first top-level window owned by `process_id` to the foreground.
pub fn focus_process_window(process_id: u32) -> bool {
    let mut context = FocusContext {
        process_id,
        found: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(focus_window_callback),
            LPARAM(&mut context as *mut FocusContext as isize),
        );
    }
    context.found
}

struct FocusContext {
    process_id: u32,
    found: bool,
}

unsafe extern "system" fn focus_window_callback(window: HWND, context: LPARAM) -> BOOL {
    let context = unsafe { &mut *(context.0 as *mut FocusContext) };
    let mut owner = 0u32;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut owner));
    }
    if owner == context.process_id && unsafe { IsWindowVisible(window).as_bool() } {
        // `SW_RESTORE` also un-maximizes a maximized window, so only undo a
        // minimize.  A maximized DSH window stays maximized.
        if unsafe { IsIconic(window).as_bool() } {
            let _ = unsafe { ShowWindow(window, SW_RESTORE) };
        }
        let _ = unsafe { SetForegroundWindow(window) };
        context.found = true;
        return BOOL(0);
    }
    BOOL(1)
}

/// Anonymous pipe for child stdout/stderr.  Only the write end is inheritable.
struct OutputPipe {
    read: HANDLE,
    write: HANDLE,
}

impl OutputPipe {
    fn create() -> Result<Self, ProcessError> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe {
            CreatePipe(&mut read, &mut write, Some(&attributes), 0)?;
            if let Err(error) = SetHandleInformation(read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) {
                let _ = CloseHandle(read);
                let _ = CloseHandle(write);
                return Err(error.into());
            }
        }
        Ok(Self { read, write })
    }

    fn close_both(self) {
        unsafe {
            let _ = CloseHandle(self.read);
            let _ = CloseHandle(self.write);
        }
    }
}

/// Owns a pipe read handle so it can move to the drain thread.
struct PipeReader(HANDLE);

// The handle is used by exactly one thread after the move.
unsafe impl Send for PipeReader {}

impl std::io::Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        match unsafe { ReadFile(self.0, Some(buffer), Some(&mut read), None) } {
            Ok(()) => Ok(read as usize),
            // ERROR_BROKEN_PIPE: every writer (the whole process tree) exited.
            Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            Err(error) => Err(std::io::Error::other(error)),
        }
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl OutputCapture {
    fn drain(self, read: HANDLE) {
        self.drain_lines(PipeReader(read));
    }
}

/// Owns the root process and its Job Object. Dropping these handles does not
/// terminate the process tree; explicit stop operations do that.
pub struct ManagedProcess {
    process: HANDLE,
    job: HANDLE,
    process_id: u32,
    pub(crate) started_at: Instant,
}

// Win32 process and Job handles are process-wide kernel handles, not thread-affine.
// This type owns each handle exactly once and manager access is synchronized.
unsafe impl Send for ManagedProcess {}

impl ManagedProcess {
    /// Start the configured executable and put it under a Job Object before resume.
    pub fn spawn(spec: &LaunchSpec) -> Result<Self, ProcessError> {
        Self::spawn_captured(spec, None)
    }

    /// Like [`ManagedProcess::spawn`], but routes the child's stdout/stderr
    /// into a pipe drained by a background thread.  The `dsh web:` line gives
    /// the backend port (token dropped) and every line is written, redacted,
    /// to the capture's log file.
    pub fn spawn_captured(
        spec: &LaunchSpec,
        capture: Option<OutputCapture>,
    ) -> Result<Self, ProcessError> {
        let environment = build_environment_block(spec)?;
        let mut command_line = build_command_line(spec.executable.as_os_str(), &spec.arguments)?;
        let executable = nul_terminated(spec.executable.as_os_str())?;
        let current_directory = nul_terminated(spec.working_directory.as_os_str())?;

        let pipe = if capture.is_some() {
            Some(OutputPipe::create()?)
        } else {
            None
        };
        let job = match unsafe { CreateJobObjectW(None, PCWSTR::null()) } {
            Ok(job) => job,
            Err(error) => {
                if let Some(pipe) = pipe {
                    pipe.close_both();
                }
                return Err(error.into());
            }
        };

        let mut startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        if let Some(pipe) = &pipe {
            startup.dwFlags = STARTF_USESTDHANDLES;
            startup.hStdOutput = pipe.write;
            startup.hStdError = pipe.write;
            startup.hStdInput = HANDLE::default();
        }
        let mut information = PROCESS_INFORMATION::default();
        // §6.3 / V11: when Marina itself runs inside a Job (some terminals do
        // that), break the child away so it gets only its instance Job.  The
        // outer Job may forbid it; then start normally and report it.
        let base_flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
        let in_job = current_process_in_job();
        let pristine_command_line = command_line.clone();
        let create = |flags, command_line: &mut Vec<u16>, information: &mut PROCESS_INFORMATION| unsafe {
            CreateProcessW(
                PCWSTR(executable.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                pipe.is_some(),
                flags,
                Some(environment.as_ptr().cast::<c_void>()),
                PCWSTR(current_directory.as_ptr()),
                &startup,
                information,
            )
        };
        let mut creation = create(
            if in_job {
                base_flags | CREATE_BREAKAWAY_FROM_JOB
            } else {
                base_flags
            },
            &mut command_line,
            &mut information,
        );
        if in_job
            && creation
                .as_ref()
                .is_err_and(|e| e.code() == ERROR_ACCESS_DENIED.to_hresult())
        {
            BREAKAWAY_DENIED.store(true, std::sync::atomic::Ordering::Relaxed);
            command_line = pristine_command_line;
            creation = create(base_flags, &mut command_line, &mut information);
        }
        // The child owns its inherited copy of the write end now.
        let reader = pipe.map(|pipe| {
            unsafe {
                let _ = CloseHandle(pipe.write);
            }
            pipe.read
        });
        if let Err(error) = creation {
            if let Some(read) = reader {
                unsafe {
                    let _ = CloseHandle(read);
                }
            }
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(error.into());
        }

        let process = information.hProcess;
        let thread = information.hThread;
        if let Err(error) = unsafe { AssignProcessToJobObject(job, process) } {
            unsafe {
                let _ = TerminateProcess(process, TERMINATED_EXIT_CODE);
                let _ = CloseHandle(thread);
                let _ = CloseHandle(process);
                let _ = CloseHandle(job);
            }
            return Err(error.into());
        }

        if unsafe { ResumeThread(thread) } == u32::MAX {
            let error = windows::core::Error::from_thread();
            unsafe {
                let _ = TerminateJobObject(job, TERMINATED_EXIT_CODE);
                let _ = CloseHandle(thread);
                let _ = CloseHandle(process);
                let _ = CloseHandle(job);
            }
            return Err(error.into());
        }
        unsafe {
            let _ = CloseHandle(thread);
        }
        if let (Some(read), Some(capture)) = (reader, capture) {
            capture.drain(read);
        }

        Ok(Self {
            process,
            job,
            process_id: information.dwProcessId,
            started_at: Instant::now(),
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    /// Sum the working set of every process currently assigned to the job.
    pub fn stats(&self) -> Result<ProcessStats, ProcessError> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(self.job),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast::<c_void>(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )?;
        }
        let active = accounting.ActiveProcesses.max(1) as usize;
        let buffer_size =
            size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() + (active - 1) * size_of::<usize>();
        let mut buffer = vec![0u8; buffer_size];
        let mut returned = 0u32;
        unsafe {
            QueryInformationJobObject(
                Some(self.job),
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast::<c_void>(),
                buffer_size as u32,
                Some(&mut returned),
            )?;
        }
        let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        let mut memory_bytes = 0u64;
        let count = list.NumberOfProcessIdsInList.min(active as u32);
        for index in 0..count as usize {
            let process_id = unsafe { *list.ProcessIdList.as_ptr().add(index) } as u32;
            let Ok(process) =
                (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
            else {
                continue;
            };
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb).as_bool() } {
                memory_bytes = memory_bytes.saturating_add(counters.WorkingSetSize as u64);
            }
            unsafe {
                let _ = CloseHandle(process);
            }
        }
        Ok(ProcessStats {
            memory_bytes,
            process_count: count,
            runtime_seconds: self.started_at.elapsed().as_secs(),
        })
    }

    /// Returns whether the root process is still active.
    pub fn is_running(&self) -> Result<bool, ProcessError> {
        match unsafe { WaitForSingleObject(self.process, 0) }.0 {
            value if value == WAIT_TIMEOUT.0 => Ok(true),
            value if value == WAIT_OBJECT_0.0 => Ok(false),
            _ => Err(ProcessError::WaitFailed),
        }
    }

    /// Request a normal window close first, then terminate the whole job after the grace period.
    pub fn stop(self, grace_period: Duration) -> Result<StopOutcome, ProcessError> {
        if !self.is_running()? {
            self.reap()?;
            return Ok(StopOutcome::AlreadyExited);
        }

        unsafe {
            EnumWindows(Some(close_process_window), LPARAM(self.process_id as isize))?;
        }
        let wait_ms = grace_period.as_millis().min((u32::MAX - 1) as u128) as u32;
        match unsafe { WaitForSingleObject(self.process, wait_ms) }.0 {
            value if value == WAIT_OBJECT_0.0 => {
                self.reap()?;
                Ok(StopOutcome::ExitedGracefully)
            }
            value if value == WAIT_TIMEOUT.0 => {
                self.reap()?;
                Ok(StopOutcome::Forced)
            }
            _ => Err(ProcessError::WaitFailed),
        }
    }

    /// Terminate the job (cleaning up any remaining tree members) and wait for
    /// every process in it to be gone.  Called after the root has exited.
    pub(crate) fn reap(&self) -> Result<(), ProcessError> {
        unsafe {
            TerminateJobObject(self.job, TERMINATED_EXIT_CODE)?;
        }
        match unsafe { WaitForSingleObject(self.job, INFINITE) }.0 {
            value if value == WAIT_OBJECT_0.0 => Ok(()),
            _ => Err(ProcessError::WaitFailed),
        }
    }

    pub fn exit_code(&self) -> Result<Option<u32>, ProcessError> {
        if self.is_running()? {
            return Ok(None);
        }
        let mut exit_code = 0;
        unsafe {
            GetExitCodeProcess(self.process, &mut exit_code)?;
        }
        Ok(Some(exit_code))
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.process);
            let _ = CloseHandle(self.job);
        }
    }
}

unsafe extern "system" fn close_process_window(window: HWND, process_id: LPARAM) -> BOOL {
    let mut owner_process_id = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut owner_process_id));
    }
    if owner_process_id == process_id.0 as u32 {
        let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    BOOL(1)
}

fn nul_terminated(value: &OsStr) -> Result<Vec<u16>, ProcessError> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(ProcessError::InvalidLaunchInput("embedded NUL".into()));
    }
    wide.push(0);
    Ok(wide)
}

fn build_environment_block(spec: &LaunchSpec) -> Result<Vec<u16>, ProcessError> {
    let removals = spec
        .environment_to_remove
        .iter()
        .map(|key| environment_key(key))
        .collect::<Result<Vec<_>, _>>()?;
    let mut entries = BTreeMap::<String, (OsString, OsString)>::new();

    for (key, value) in std::env::vars_os() {
        // Rust exposes Windows per-drive pseudo-variables (for example `=C:`).
        // They are not ordinary NAME=VALUE entries and the explicit cwd makes them unnecessary.
        if key.to_string_lossy().starts_with('=') {
            continue;
        }
        let normalized = environment_key(&key)?;
        if !removals.iter().any(|remove| remove == &normalized) {
            entries.insert(normalized, (key, value));
        }
    }
    for (key, value) in &spec.environment {
        let normalized = environment_key(key)?;
        if value.encode_wide().any(|unit| unit == 0) {
            return Err(ProcessError::InvalidLaunchInput(
                key.to_string_lossy().into_owned(),
            ));
        }
        entries.insert(normalized, (key.clone(), value.clone()));
    }

    let mut block = Vec::new();
    for (_, (key, value)) in entries {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn environment_key(key: &OsStr) -> Result<String, ProcessError> {
    let key = key.to_string_lossy();
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Err(ProcessError::InvalidLaunchInput(key.into_owned()));
    }
    Ok(key.to_ascii_uppercase())
}

fn build_command_line(
    executable: &OsStr,
    arguments: &[OsString],
) -> Result<Vec<u16>, ProcessError> {
    let mut command = quote_windows_argument(executable)?;
    for argument in arguments {
        command.push(b' ' as u16);
        command.extend(quote_windows_argument(argument)?);
    }
    command.push(0);
    Ok(command)
}

fn quote_windows_argument(argument: &OsStr) -> Result<Vec<u16>, ProcessError> {
    let wide: Vec<u16> = argument.encode_wide().collect();
    if wide.contains(&0) {
        return Err(ProcessError::InvalidLaunchInput(
            "argument contains NUL".into(),
        ));
    }

    let mut quoted = Vec::with_capacity(wide.len() + 2);
    quoted.push(b'"' as u16);
    let mut backslashes = 0;
    for unit in wide {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            quoted.push(unit);
            backslashes = 0;
        } else {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            quoted.push(unit);
            backslashes = 0;
        }
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    quoted.push(b'"' as u16);
    Ok(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{
        InstanceProcessError, InstanceProcessManager, InstanceProcessStatus, ProcessExit,
    };

    fn as_string(wide: Vec<u16>) -> String {
        String::from_utf16(&wide).unwrap()
    }

    #[test]
    fn quotes_windows_arguments_with_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            as_string(quote_windows_argument(OsStr::new("plain")).unwrap()),
            r#""plain""#
        );
        assert_eq!(
            as_string(quote_windows_argument(OsStr::new("two words")).unwrap()),
            r#""two words""#
        );
        assert_eq!(
            as_string(quote_windows_argument(OsStr::new(r#"a\"b"#)).unwrap()),
            r#""a\\\"b""#
        );
        assert_eq!(
            as_string(quote_windows_argument(OsStr::new(r"tail\")).unwrap()),
            r#""tail\\""#
        );
    }

    #[test]
    fn environment_block_applies_overrides_and_removals() {
        let spec = LaunchSpec {
            executable: "C:\\Program Files\\DSH.exe".into(),
            working_directory: "C:\\Program Files".into(),
            arguments: Vec::new(),
            environment: vec![("DSH_HOME".into(), "C:\\instances\\one\\home".into())],
            environment_to_remove: vec!["ELECTRON_RUN_AS_NODE".into()],
            diagnostic_file: "C:\\tmp\\dsh.log".into(),
        };
        let block = build_environment_block(&spec).unwrap();
        let decoded = String::from_utf16(&block).unwrap();
        assert!(decoded.contains("DSH_HOME=C:\\instances\\one\\home"));
        assert!(
            !decoded
                .to_ascii_uppercase()
                .contains("ELECTRON_RUN_AS_NODE=")
        );
        assert!(block.ends_with(&[0, 0]));
    }

    #[test]
    fn reaps_a_normally_exited_instance_and_reports_its_status() {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        let executable = std::path::PathBuf::from(system_root).join("System32\\where.exe");
        let spec = LaunchSpec {
            working_directory: executable.parent().unwrap().to_path_buf(),
            executable,
            arguments: vec!["cmd.exe".into()],
            environment: Vec::new(),
            environment_to_remove: Vec::new(),
            diagnostic_file: std::path::PathBuf::new(),
        };
        let mut manager = InstanceProcessManager::default();
        let process_id = manager.start("instance-one", &spec).unwrap();
        for _ in 0..100 {
            if matches!(
                manager.status("instance-one").unwrap(),
                InstanceProcessStatus::Exited { .. }
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Exited {
                process_id,
                exit_code: 0,
            }
        );
        assert_eq!(
            manager.reap_exited().unwrap(),
            vec![ProcessExit {
                instance_id: "instance-one".into(),
                process_id,
                exit_code: 0,
                runtime_seconds: 0,
            }]
        );
        assert_eq!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Stopped
        );
    }

    #[test]
    fn captures_the_dsh_web_line_and_redacts_the_token() {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        let executable = std::path::PathBuf::from(system_root)
            .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let directory = tempfile::tempdir().unwrap();
        let log_file = directory.path().join("stdout.log");
        let endpoint = crate::health::WebEndpoint::default();
        let spec = LaunchSpec {
            working_directory: executable.parent().unwrap().to_path_buf(),
            executable,
            arguments: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Console]::Out.WriteLine('dsh web: http://127.0.0.1:43210/?token=secret')".into(),
            ],
            environment: Vec::new(),
            environment_to_remove: Vec::new(),
            diagnostic_file: std::path::PathBuf::new(),
        };
        let mut manager = InstanceProcessManager::default();
        manager
            .start_captured(
                "instance-one",
                &spec,
                Some(OutputCapture {
                    log_file: log_file.clone(),
                    endpoint: endpoint.clone(),
                }),
            )
            .unwrap();
        for _ in 0..1000 {
            let logged = std::fs::read_to_string(&log_file).unwrap_or_default();
            if endpoint.port().is_some() && logged.contains("dsh web:") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(endpoint.port(), Some(43210));
        let logged = std::fs::read_to_string(&log_file).unwrap();
        assert!(logged.contains("token=***"), "{logged}");
        assert!(!logged.contains("secret"));
    }

    #[test]
    fn tracks_and_stops_a_long_running_instance_process() {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        let executable = std::path::PathBuf::from(system_root).join("System32\\ping.exe");
        let spec = LaunchSpec {
            working_directory: executable.parent().unwrap().to_path_buf(),
            executable,
            arguments: vec!["-n".into(), "20".into(), "127.0.0.1".into()],
            environment: Vec::new(),
            environment_to_remove: Vec::new(),
            diagnostic_file: std::path::PathBuf::new(),
        };
        let mut manager = InstanceProcessManager::default();
        let process_id = manager.start("instance-one", &spec).unwrap();
        assert_eq!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Running { process_id }
        );
        assert!(matches!(
            manager.start("instance-one", &spec),
            Err(InstanceProcessError::AlreadyRunning(id)) if id == "instance-one"
        ));
        assert_eq!(
            manager
                .stop("instance-one", Duration::from_millis(10))
                .unwrap(),
            StopOutcome::Forced
        );
        assert_eq!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Stopped
        );
    }

    #[test]
    fn flushes_the_log_tail_while_the_instance_stays_silent() {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        let executable = std::path::PathBuf::from(system_root)
            .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let directory = tempfile::tempdir().unwrap();
        let log_file = directory.path().join("stdout.log");
        let endpoint = crate::health::WebEndpoint::default();
        let spec = LaunchSpec {
            working_directory: executable.parent().unwrap().to_path_buf(),
            executable,
            arguments: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Console]::Out.WriteLine('dsh web: http://127.0.0.1:43211/?token=secret'); Start-Sleep -Seconds 15".into(),
            ],
            environment: Vec::new(),
            environment_to_remove: Vec::new(),
            diagnostic_file: std::path::PathBuf::new(),
        };
        let mut manager = InstanceProcessManager::default();
        manager
            .start_captured(
                "instance-one",
                &spec,
                Some(OutputCapture {
                    log_file: log_file.clone(),
                    endpoint: endpoint.clone(),
                }),
            )
            .unwrap();
        // The line must reach the file while the process is alive and silent:
        // the writer flushes on quiet periods, not only when output resumes.
        let mut flushed_while_running = false;
        for _ in 0..600 {
            if std::fs::read_to_string(&log_file)
                .unwrap_or_default()
                .contains("dsh web:")
            {
                flushed_while_running = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(flushed_while_running, "log tail never flushed while silent");
        assert!(matches!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Running { .. }
        ));
        let _ = manager.stop("instance-one", Duration::from_millis(10));
    }

    #[test]
    fn process_tree_queries_memory_only_for_tree_members() {
        let tree = process_tree(std::process::id()).unwrap();
        let root = tree
            .iter()
            .find(|entry| entry.process_id == std::process::id())
            .expect("the current process is in its own tree");
        assert!(root.in_tree);
        assert!(root.memory_bytes > 0);
        // Unrelated processes must not be opened for memory accounting.
        assert!(tree.iter().any(|entry| !entry.in_tree));
        assert!(
            tree.iter()
                .all(|entry| entry.in_tree || entry.memory_bytes == 0)
        );
    }
}
