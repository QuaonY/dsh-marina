//! macOS backend: each instance runs in its own process group (`setpgid` via
//! `Command::process_group(0)`), stopped with SIGTERM → grace → SIGKILL to the
//! whole group.  Adoption uses `kill(pid, 0)` liveness and `ps` tree scans;
//! the exit code of a non-child process is not observable on macOS, so adopted
//! exits report 0 (documented limitation — crash detection for adopted
//! instances relies on the log/health signals instead).

use super::{AdoptableInstance, OutputCapture, ProcessError, ProcessStats, StopOutcome};
use crate::launch::LaunchSpec;
use std::cell::Cell;
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// macOS has no Job Objects; the breakaway warning never applies.
pub fn breakaway_denied() -> bool {
    false
}

fn signal_group(process_id: u32, signal: libc::c_int) {
    unsafe {
        libc::killpg(process_id as libc::pid_t, signal);
    }
}

fn signal_pid(process_id: u32, signal: libc::c_int) {
    unsafe {
        libc::kill(process_id as libc::pid_t, signal);
    }
}

fn pid_alive(process_id: u32) -> bool {
    unsafe { libc::kill(process_id as libc::pid_t, 0) == 0 }
}

/// Decode a `waitpid` status into an exit code (signal death maps to 128+sig
/// so it reads as a crash, mirroring the Windows non-zero convention).
fn status_exit_code(status: libc::c_int) -> u32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status) as u32
    } else {
        1
    }
}

/// Run `ps` with the given format arguments and return its output lines.
fn ps_lines(args: &[&str]) -> Vec<String> {
    let Ok(output) = std::process::Command::new("ps").args(args).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `(pid, ppid)` for every process, from `ps`.
fn process_parents() -> Vec<(u32, u32)> {
    ps_lines(&["-axo", "pid=,ppid="])
        .into_iter()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

/// Every descendant of `root` (excluding the root itself), via `ps` ppid walk.
pub fn descendant_process_ids(root_process_id: u32) -> Vec<u32> {
    let entries = process_parents();
    let mut members = std::collections::HashSet::new();
    members.insert(root_process_id);
    loop {
        let mut added = false;
        for (pid, ppid) in &entries {
            if members.contains(ppid) && members.insert(*pid) {
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    members
        .into_iter()
        .filter(|pid| *pid != root_process_id)
        .collect()
}

/// Resident memory (bytes) and member count of a process group, from `ps`.
fn group_stats(process_id: u32) -> (u64, u32) {
    let mut memory = 0u64;
    let mut count = 0u32;
    for line in ps_lines(&["-axo", "pid=,pgid=,rss="]) {
        let mut parts = line.split_whitespace();
        let Some(_pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let Some(pgid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let Some(rss_kb) = parts.next().and_then(|p| p.parse::<u64>().ok()) else {
            continue;
        };
        if pgid == process_id {
            memory = memory.saturating_add(rss_kb * 1024);
            count += 1;
        }
    }
    (memory, count)
}

/// Resident memory of a process tree (adopted processes are not group leaders).
fn tree_stats(root_process_id: u32) -> (u64, u32) {
    let descendants = descendant_process_ids(root_process_id);
    let mut memory = 0u64;
    let mut count = 0u32;
    for line in ps_lines(&["-axo", "pid=,rss="]) {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let Some(rss_kb) = parts.next().and_then(|p| p.parse::<u64>().ok()) else {
            continue;
        };
        if pid == root_process_id || descendants.contains(&pid) {
            memory = memory.saturating_add(rss_kb * 1024);
            count += 1;
        }
    }
    (memory, count)
}

/// A DSH process that was already running before Marina started.
pub struct AdoptedProcess {
    pub(crate) process_id: u32,
    pub(crate) started_at: Instant,
}

unsafe impl Send for AdoptedProcess {}

impl AdoptedProcess {
    pub(crate) fn open(process_id: u32) -> Result<Self, ProcessError> {
        if !pid_alive(process_id) {
            return Err(ProcessError::Platform(format!("进程 {process_id} 不存在")));
        }
        Ok(Self {
            process_id,
            started_at: Instant::now(),
        })
    }

    pub(crate) fn is_running(&self) -> Result<bool, ProcessError> {
        Ok(pid_alive(self.process_id))
    }

    pub(crate) fn exit_code(&self) -> Result<Option<u32>, ProcessError> {
        if self.is_running()? {
            return Ok(None);
        }
        // macOS cannot report the exit status of a non-child process.
        Ok(Some(0))
    }

    pub(crate) fn stats(&self) -> Result<ProcessStats, ProcessError> {
        let (memory_bytes, process_count) = tree_stats(self.process_id);
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
        signal_pid(self.process_id, libc::SIGTERM);
        let deadline = Instant::now() + grace_period;
        while Instant::now() < deadline {
            if !pid_alive(self.process_id) {
                return Ok(StopOutcome::ExitedGracefully);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Kill descendants first so the tree cannot reparent while walked.
        for pid in descendant_process_ids(self.process_id) {
            signal_pid(pid, libc::SIGKILL);
        }
        signal_pid(self.process_id, libc::SIGKILL);
        Ok(StopOutcome::Forced)
    }
}

/// Owns the root child of one instance.  Dropping does not terminate the
/// process group; explicit stop operations do that.
pub struct ManagedProcess {
    process_id: u32,
    pub(crate) started_at: Instant,
    exit_status: Cell<Option<libc::c_int>>,
}

unsafe impl Send for ManagedProcess {}

impl ManagedProcess {
    pub fn spawn(spec: &LaunchSpec) -> Result<Self, ProcessError> {
        Self::spawn_captured(spec, None)
    }

    /// Start the configured executable as a new process group leader, with
    /// stdout/stderr optionally drained into the capture log.
    pub fn spawn_captured(
        spec: &LaunchSpec,
        capture: Option<OutputCapture>,
    ) -> Result<Self, ProcessError> {
        let mut command = std::process::Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .current_dir(&spec.working_directory)
            .process_group(0);
        for name in &spec.environment_to_remove {
            command.env_remove(name);
        }
        for (name, value) in &spec.environment {
            command.env(name, value);
        }
        if capture.is_some() {
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        }
        let mut child = command
            .spawn()
            .map_err(|e| ProcessError::InvalidLaunchInput(e.to_string()))?;
        let process_id = child.id();
        if let Some(capture) = capture {
            if let Some(stdout) = child.stdout.take() {
                capture.clone().drain_lines(stdout);
            }
            if let Some(stderr) = child.stderr.take() {
                capture.drain_lines(stderr);
            }
        }
        // Dropping the `Child` does not kill the process; liveness and reaping
        // go through `waitpid` so `&self` methods stay usable from the manager.
        drop(child);
        Ok(Self {
            process_id,
            started_at: Instant::now(),
            exit_status: Cell::new(None),
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    /// Reap the zombie if the child has exited; cache its status.
    fn poll_exit(&self) -> Option<libc::c_int> {
        if let Some(status) = self.exit_status.get() {
            return Some(status);
        }
        let mut status: libc::c_int = 0;
        let result =
            unsafe { libc::waitpid(self.process_id as libc::pid_t, &mut status, libc::WNOHANG) };
        if result == self.process_id as libc::pid_t {
            self.exit_status.set(Some(status));
            Some(status)
        } else if result < 0 {
            // ECHILD: already reaped elsewhere; treat as exited with unknown code.
            self.exit_status.set(Some(0));
            Some(0)
        } else {
            None
        }
    }

    pub fn stats(&self) -> Result<ProcessStats, ProcessError> {
        let (memory_bytes, process_count) = group_stats(self.process_id);
        Ok(ProcessStats {
            memory_bytes,
            process_count: process_count.max(1),
            runtime_seconds: self.started_at.elapsed().as_secs(),
        })
    }

    pub fn is_running(&self) -> Result<bool, ProcessError> {
        Ok(self.poll_exit().is_none())
    }

    /// SIGTERM the group, wait out the grace period, then SIGKILL whatever
    /// is left.  Mirrors the Windows WM_CLOSE → job-terminate contract.
    pub fn stop(self, grace_period: Duration) -> Result<StopOutcome, ProcessError> {
        if self.poll_exit().is_some() {
            signal_group(self.process_id, libc::SIGKILL);
            return Ok(StopOutcome::AlreadyExited);
        }
        signal_group(self.process_id, libc::SIGTERM);
        let deadline = Instant::now() + grace_period;
        while Instant::now() < deadline {
            if self.poll_exit().is_some() {
                // The root exited; sweep any remaining group members.
                signal_group(self.process_id, libc::SIGKILL);
                return Ok(StopOutcome::ExitedGracefully);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        signal_group(self.process_id, libc::SIGKILL);
        // Give the kernel a moment to tear the group down.
        for _ in 0..100 {
            if self.poll_exit().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(StopOutcome::Forced)
    }

    /// Release the zombie after exit (the macOS counterpart of terminating
    /// and waiting on the Job Object).
    pub(crate) fn reap(&self) -> Result<(), ProcessError> {
        self.poll_exit();
        Ok(())
    }

    pub fn exit_code(&self) -> Result<Option<u32>, ProcessError> {
        Ok(self.poll_exit().map(status_exit_code))
    }
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

    let mut candidates = Vec::new();
    for line in ps_lines(&["-axww", "-o", "pid=,command="]) {
        let line = line.trim();
        let Some((pid_text, command)) = line.split_once(|c: char| c.is_whitespace()) else {
            continue;
        };
        let Ok(process_id) = pid_text.parse::<u32>() else {
            continue;
        };
        let command = command.trim_start();
        let Some(executable) = command.split_whitespace().next() else {
            continue;
        };
        let name = Path::new(executable)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let expected = crate::runtimes::DSH_EXECUTABLE_NAME;
        let matches_name = name.eq_ignore_ascii_case(expected)
            || name.eq_ignore_ascii_case(expected.trim_end_matches(".exe"));
        if matches_name
            && let Some(user_data) = user_data_argument(command)
            && let Some(instance_id) = targets.get(&normalize_path(Path::new(&user_data)))
        {
            candidates.push(AdoptableInstance {
                instance_id: instance_id.clone(),
                process_id,
            });
        }
    }
    Ok(candidates)
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Extract `--user-data-dir=<path>` from a `ps` command line.  `ps` does not
/// preserve argument quoting, so a value containing spaces is read up to the
/// next ` --` flag boundary (best effort; Marina instance paths rarely contain
/// ` --`).
fn user_data_argument(command_line: &str) -> Option<String> {
    const PREFIX: &str = "--user-data-dir=";
    let position = command_line.find(PREFIX)?;
    let rest = &command_line[position + PREFIX.len()..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest
        .find(" --")
        .or_else(|| rest.find('"'))
        .unwrap_or(rest.len());
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Find a running process whose executable path matches `executable`.
pub fn find_process_by_executable(executable: &Path) -> Result<Option<u32>, ProcessError> {
    let expected = executable.to_string_lossy().to_ascii_lowercase();
    for line in ps_lines(&["-axo", "pid="]) {
        let Ok(process_id) = line.trim().parse::<u32>() else {
            continue;
        };
        if process_id == 0 {
            continue;
        }
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = unsafe {
            libc::proc_pidpath(
                process_id as libc::pid_t,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len() as u32,
            )
        };
        if length <= 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&buffer[..length as usize]).to_ascii_lowercase();
        if path == expected {
            return Ok(Some(process_id));
        }
    }
    Ok(None)
}

/// Ask a process to exit, wait, then kill its tree (adopted/external path).
pub fn stop_process_tree_by_id(
    process_id: u32,
    grace_period: Duration,
) -> Result<StopOutcome, ProcessError> {
    AdoptedProcess::open(process_id)?.stop(grace_period)
}

/// Start `spec` without tracking it (hands a `dsh://` link to a running
/// instance; Electron's single-instance lock forwards and exits).
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

/// Bring the app owning `process_id` to the foreground via System Events.
pub fn focus_process_window(process_id: u32) -> bool {
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is {process_id}) to true"
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{InstanceProcessError, InstanceProcessManager, InstanceProcessStatus};

    fn sleep_spec(seconds: &str) -> LaunchSpec {
        LaunchSpec {
            executable: "/bin/sleep".into(),
            working_directory: "/tmp".into(),
            arguments: vec![seconds.into()],
            environment: Vec::new(),
            environment_to_remove: Vec::new(),
            diagnostic_file: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn tracks_and_stops_a_long_running_instance_process() {
        let mut manager = InstanceProcessManager::default();
        let process_id = manager.start("instance-one", &sleep_spec("20")).unwrap();
        assert_eq!(
            manager.status("instance-one").unwrap(),
            InstanceProcessStatus::Running { process_id }
        );
        assert!(matches!(
            manager.start("instance-one", &sleep_spec("20")),
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
    fn captures_the_dsh_web_line_and_redacts_the_token() {
        let directory = tempfile::tempdir().unwrap();
        let log_file = directory.path().join("stdout.log");
        let endpoint = crate::health::WebEndpoint::default();
        let spec = LaunchSpec {
            executable: "/bin/echo".into(),
            working_directory: "/tmp".into(),
            arguments: vec!["dsh web: http://127.0.0.1:43210/?token=secret".into()],
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
        for _ in 0..500 {
            if endpoint.port().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(endpoint.port(), Some(43210));
        let logged = std::fs::read_to_string(&log_file).unwrap();
        assert!(logged.contains("token=***"), "{logged}");
        assert!(!logged.contains("secret"));
    }
}
