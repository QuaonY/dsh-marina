//! End-to-end process lifecycle tests against `fake-desktop`, a stub of the
//! DSH desktop app (src/bin/fake-desktop.rs). Each test picks the stub's
//! behaviour by writing `<DSH_HOME>/fake-mode.txt`.
//!
//! Every process a test starts is opened and held by its `Harness`. The
//! handle keeps the pid from being reused, and `Harness::drop` kills the whole
//! tree, so nothing leaks when an assertion panics.

#![cfg(windows)]

use dsh_marina::crash::diagnostic_excerpt;
use dsh_marina::health::{WebEndpoint, probe_port};
use dsh_marina::instances::{InstanceConfig, InstancePaths, InstanceRepository};
use dsh_marina::launch::LaunchSpec;
use dsh_marina::process::{
    AdoptableInstance, InstanceProcessManager, InstanceProcessStatus, OutputCapture, StopOutcome,
    descendant_process_ids, scan_instance_processes, stop_process_tree_by_id,
};
use dsh_marina::runtimes::DSH_EXECUTABLE_NAME;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Exit code Marina uses when it terminates a process or job.
const TERMINATED_EXIT_CODE: u32 = 1;

fn fake_desktop() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-desktop"))
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn http_get(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the fake backend");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("send request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

/// A process handle held for the whole test.
struct WatchedProcess {
    handle: HANDLE,
    process_id: u32,
}

impl WatchedProcess {
    fn open(process_id: u32) -> Self {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                process_id,
            )
        }
        .unwrap_or_else(|error| panic!("cannot open process {process_id}: {error}"));
        Self { handle, process_id }
    }

    fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) }.0 == WAIT_TIMEOUT.0
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let wait_ms = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
        unsafe { WaitForSingleObject(self.handle, wait_ms) }.0 == WAIT_OBJECT_0.0
    }

    fn exit_code(&self) -> Option<u32> {
        if self.is_running() {
            return None;
        }
        let mut exit_code = 0u32;
        unsafe { GetExitCodeProcess(self.handle, &mut exit_code) }.ok()?;
        Some(exit_code)
    }

    fn kill(&self) {
        if self.is_running() {
            unsafe {
                let _ = TerminateProcess(self.handle, TERMINATED_EXIT_CODE);
            }
            let _ = self.wait_for_exit(Duration::from_secs(5));
        }
    }
}

impl Drop for WatchedProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Captured stdout of one launch.
struct Capture {
    log_file: PathBuf,
    endpoint: WebEndpoint,
}

impl Capture {
    fn log(&self) -> String {
        fs::read_to_string(&self.log_file).unwrap_or_default()
    }

    fn wait_for_port(&self) -> u16 {
        assert!(
            wait_until(READY_TIMEOUT, || self.endpoint.port().is_some()),
            "no `dsh web:` line was captured; log so far:\n{}",
            self.log()
        );
        self.endpoint.port().unwrap()
    }
}

/// One instance in its own temporary data root, plus cleanup of everything
/// started for it.
struct Harness {
    manager: InstanceProcessManager,
    instance_ids: Vec<String>,
    watched: Vec<Rc<WatchedProcess>>,
    repository: InstanceRepository,
    config: InstanceConfig,
    paths: InstancePaths,
    // Declared last so it is removed after the processes are gone.
    temp: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with_data_dir("data")
    }

    fn with_data_dir(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create a temporary directory");
        let repository = InstanceRepository::new(temp.path().join(name));
        let config = repository
            .create_blank("fake desktop")
            .expect("create an instance");
        let paths = repository.paths(&config.id).expect("instance paths");
        Self {
            manager: InstanceProcessManager::default(),
            instance_ids: Vec::new(),
            watched: Vec::new(),
            repository,
            config,
            paths,
            temp,
        }
    }

    fn id(&self) -> String {
        self.config.id.clone()
    }

    fn set_mode(&self, mode: &str) {
        fs::write(self.paths.home.join("fake-mode.txt"), mode).expect("write fake-mode.txt");
    }

    fn spec(&self, executable: &Path) -> LaunchSpec {
        LaunchSpec::for_instance(executable, &self.config, &self.paths).expect("launch spec")
    }

    /// Copy the stub under the real DSH file name, which adoption scans for.
    fn install_dsh_copy(&self) -> PathBuf {
        let directory = self.temp.path().join("runtime");
        fs::create_dir_all(&directory).expect("create the runtime directory");
        let executable = directory.join(DSH_EXECUTABLE_NAME);
        fs::copy(fake_desktop(), &executable).expect("copy fake-desktop");
        executable
    }

    fn watch(&mut self, process_id: u32) -> Rc<WatchedProcess> {
        let process = Rc::new(WatchedProcess::open(process_id));
        self.watched.push(Rc::clone(&process));
        process
    }

    /// Start through the manager with stdout inherited.
    fn start(&mut self, spec: &LaunchSpec) -> Rc<WatchedProcess> {
        let id = self.id();
        self.instance_ids.push(id.clone());
        let process_id = self.manager.start(id, spec).expect("start the instance");
        self.watch(process_id)
    }

    /// Start through the manager with stdout captured like Marina does.
    fn start_captured(&mut self, spec: &LaunchSpec) -> (Rc<WatchedProcess>, Capture) {
        let id = self.id();
        let capture = Capture {
            log_file: self
                .paths
                .marina
                .join(format!("stdout-{}.log", self.watched.len())),
            endpoint: WebEndpoint::default(),
        };
        self.instance_ids.push(id.clone());
        let process_id = self
            .manager
            .start_captured(
                id,
                spec,
                Some(OutputCapture {
                    log_file: capture.log_file.clone(),
                    endpoint: capture.endpoint.clone(),
                }),
            )
            .expect("start the instance");
        (self.watch(process_id), capture)
    }

    /// Start outside any Job Object, the way a DSH left running by an
    /// earlier Marina (or started by hand) looks.
    fn spawn_unmanaged(&mut self, spec: &LaunchSpec) -> Rc<WatchedProcess> {
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .current_dir(&spec.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in &spec.environment {
            command.env(key, value);
        }
        for key in &spec.environment_to_remove {
            command.env_remove(key);
        }
        let child = command.spawn().expect("spawn fake-desktop");
        let process = self.watch(child.id());
        // Dropping `Child` neither waits for nor kills the process.
        drop(child);
        process
    }

    /// Start through a throwaway manager (which quotes every argument) and
    /// forget it, so the process is left running for adoption.
    fn spawn_like_marina_quoting(&mut self, spec: &LaunchSpec) -> Rc<WatchedProcess> {
        let mut manager = InstanceProcessManager::default();
        let process_id = manager.start(self.id(), spec).expect("start the instance");
        drop(manager);
        self.watch(process_id)
    }

    /// Wait for the `spawn-child` stub to report its child and watch it.
    fn child_process(&mut self) -> Rc<WatchedProcess> {
        let pid_file = self.paths.home.join("child.pid");
        let mut process_id = None;
        let found = wait_until(READY_TIMEOUT, || {
            process_id = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            process_id.is_some()
        });
        assert!(found, "fake-desktop never wrote {}", pid_file.display());
        self.watch(process_id.unwrap())
    }

    fn status(&self) -> InstanceProcessStatus {
        self.manager
            .status(&self.config.id)
            .expect("instance status")
    }

    fn wait_for_exit_status(&self) -> InstanceProcessStatus {
        let exited = wait_until(EXIT_TIMEOUT, || {
            matches!(self.status(), InstanceProcessStatus::Exited { .. })
        });
        assert!(exited, "instance did not exit: {:?}", self.status());
        self.status()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The held handles keep these pids from being reused, so tree
        // termination by pid cannot hit an unrelated process.
        for process in &self.watched {
            if process.is_running() {
                let _ = stop_process_tree_by_id(process.process_id, Duration::ZERO);
            }
        }
        for id in &self.instance_ids {
            let _ = self.manager.stop(id, Duration::ZERO);
        }
        for process in &self.watched {
            process.kill();
        }
    }
}

#[test]
fn run_mode_announces_its_port_and_the_log_redacts_the_token() {
    let mut harness = Harness::new();
    harness.set_mode("run");
    let spec = harness.spec(&fake_desktop());
    let (process, capture) = harness.start_captured(&spec);
    let id = harness.id();

    assert_eq!(
        harness.status(),
        InstanceProcessStatus::Running {
            process_id: process.process_id
        }
    );
    let port = capture.wait_for_port();
    assert!(probe_port(port, Duration::from_secs(2)));
    let response = http_get(port);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    assert!(
        wait_until(READY_TIMEOUT, || capture.log().contains("dsh web:")),
        "{}",
        capture.log()
    );
    let log = capture.log();
    assert!(
        log.contains(&format!("dsh web: http://127.0.0.1:{port}/?token=***")),
        "{log}"
    );
    assert!(!log.contains("secret123"), "{log}");

    let stats = harness
        .manager
        .stats(&id)
        .unwrap()
        .expect("stats of a running instance");
    assert!(stats.process_count >= 1, "{stats:?}");
    assert!(stats.memory_bytes > 0, "{stats:?}");
    assert_eq!(harness.manager.running_instances().unwrap(), vec![id]);
}

#[test]
fn stopping_a_running_instance_terminates_it_and_closes_its_port() {
    let mut harness = Harness::new();
    harness.set_mode("run");
    let spec = harness.spec(&fake_desktop());
    let (process, capture) = harness.start_captured(&spec);
    let id = harness.id();
    let port = capture.wait_for_port();

    let outcome = harness
        .manager
        .stop(&id, Duration::from_millis(500))
        .unwrap();
    // fake-desktop has no window, so WM_CLOSE cannot end it gracefully.
    assert_eq!(outcome, StopOutcome::Forced);
    assert!(process.wait_for_exit(EXIT_TIMEOUT));
    assert_eq!(harness.status(), InstanceProcessStatus::Stopped);
    assert!(harness.manager.running_instances().unwrap().is_empty());
    assert_eq!(harness.manager.stats(&id).unwrap(), None);
    assert!(wait_until(EXIT_TIMEOUT, || !probe_port(
        port,
        Duration::from_millis(200)
    )));
    assert_eq!(
        harness.manager.stop(&id, Duration::ZERO).unwrap(),
        StopOutcome::AlreadyExited
    );
}

#[test]
fn a_crash_is_reported_with_its_non_zero_exit_code() {
    let mut harness = Harness::new();
    harness.set_mode("crash");
    let spec = harness.spec(&fake_desktop());
    let process = harness.start(&spec);
    let id = harness.id();

    // The stub waits about a second before exiting.
    assert_eq!(
        harness.status(),
        InstanceProcessStatus::Running {
            process_id: process.process_id
        }
    );
    assert_eq!(
        harness.wait_for_exit_status(),
        InstanceProcessStatus::Exited {
            process_id: process.process_id,
            exit_code: 1,
        }
    );
    assert!(harness.manager.running_instances().unwrap().is_empty());

    let exits = harness.manager.reap_exited().unwrap();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].instance_id, id);
    assert_eq!(exits[0].process_id, process.process_id);
    assert_eq!(exits[0].exit_code, 1);
    assert_eq!(harness.status(), InstanceProcessStatus::Stopped);
    assert!(harness.manager.reap_exited().unwrap().is_empty());
}

#[test]
fn an_exited_instance_can_be_started_again() {
    let mut harness = Harness::new();
    harness.set_mode("crash");
    let spec = harness.spec(&fake_desktop());
    let first = harness.start(&spec);
    harness.wait_for_exit_status();

    // No reap in between: start must replace the exited entry itself.
    harness.set_mode("run");
    let spec = harness.spec(&fake_desktop());
    let (second, capture) = harness.start_captured(&spec);
    assert_ne!(first.process_id, second.process_id);
    assert_eq!(
        harness.status(),
        InstanceProcessStatus::Running {
            process_id: second.process_id
        }
    );
    assert!(probe_port(capture.wait_for_port(), Duration::from_secs(2)));
}

#[test]
fn a_startup_failure_leaves_the_diagnostic_file_for_the_crash_report() {
    let mut harness = Harness::new();
    harness.set_mode("diagnostic");
    let spec = harness.spec(&fake_desktop());
    assert!(spec.diagnostic_file.starts_with(&harness.paths.marina));
    assert!(!spec.diagnostic_file.exists());
    let process = harness.start(&spec);

    assert_eq!(
        harness.wait_for_exit_status(),
        InstanceProcessStatus::Exited {
            process_id: process.process_id,
            exit_code: 1,
        }
    );
    let excerpt = diagnostic_excerpt(&spec.diagnostic_file, 4000)
        .unwrap_or_else(|| panic!("{} is missing", spec.diagnostic_file.display()));
    assert!(excerpt.contains("Uncaught Exception"), "{excerpt}");
    assert!(
        excerpt.contains("fake-desktop simulated startup failure"),
        "{excerpt}"
    );
    let exits = harness.manager.reap_exited().unwrap();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].exit_code, 1);
}

#[test]
fn stopping_a_managed_instance_kills_its_child_processes() {
    let mut harness = Harness::new();
    harness.set_mode("spawn-child");
    let spec = harness.spec(&fake_desktop());
    let (parent, capture) = harness.start_captured(&spec);
    let child = harness.child_process();
    let id = harness.id();

    assert!(child.is_running());
    assert!(descendant_process_ids(parent.process_id).contains(&child.process_id));
    assert!(wait_until(READY_TIMEOUT, || {
        harness
            .manager
            .stats(&id)
            .ok()
            .flatten()
            .is_some_and(|stats| stats.process_count >= 2)
    }));
    // The child inherited the captured stdout, so its line arrives through
    // the parent's pipe.
    assert!(probe_port(capture.wait_for_port(), Duration::from_secs(2)));

    let outcome = harness
        .manager
        .stop(&id, Duration::from_millis(200))
        .unwrap();
    assert_eq!(outcome, StopOutcome::Forced);
    assert!(
        child.wait_for_exit(EXIT_TIMEOUT),
        "child {} survived",
        child.process_id
    );
    assert!(parent.wait_for_exit(EXIT_TIMEOUT));
    assert_eq!(child.exit_code(), Some(TERMINATED_EXIT_CODE));
    assert_eq!(harness.status(), InstanceProcessStatus::Stopped);
}

#[test]
fn stopping_an_unmanaged_process_tree_by_pid_kills_its_children() {
    let mut harness = Harness::new();
    harness.set_mode("spawn-child");
    let spec = harness.spec(&fake_desktop());
    let parent = harness.spawn_unmanaged(&spec);
    let child = harness.child_process();
    assert!(descendant_process_ids(parent.process_id).contains(&child.process_id));

    let outcome = stop_process_tree_by_id(parent.process_id, Duration::from_millis(200)).unwrap();
    assert_eq!(outcome, StopOutcome::Forced);
    assert!(parent.wait_for_exit(EXIT_TIMEOUT));
    assert!(
        child.wait_for_exit(EXIT_TIMEOUT),
        "child {} survived",
        child.process_id
    );
}

#[test]
fn a_fresh_manager_adopts_a_running_instance_and_stops_its_tree() {
    let mut harness = Harness::new();
    harness.set_mode("spawn-child");
    let executable = harness.install_dsh_copy();
    let spec = harness.spec(&executable);
    let (parent, capture) = harness.start_captured(&spec);
    let child = harness.child_process();
    let id = harness.id();
    capture.wait_for_port();

    // Marina exits: dropping its manager closes the handles but leaves the
    // instance running.
    drop(std::mem::take(&mut harness.manager));
    assert!(parent.is_running());
    assert!(child.is_running());

    // The child has no `--user-data-dir`, so only the parent matches.
    assert_eq!(
        scan_instance_processes(&harness.repository).unwrap(),
        vec![AdoptableInstance {
            instance_id: id.clone(),
            process_id: parent.process_id,
        }]
    );

    let mut fresh = InstanceProcessManager::default();
    assert_eq!(
        fresh.adopt_existing(&harness.repository).unwrap(),
        vec![id.clone()]
    );
    assert!(fresh.is_adopted(&id));
    assert_eq!(
        fresh.status(&id).unwrap(),
        InstanceProcessStatus::Running {
            process_id: parent.process_id
        }
    );
    assert_eq!(fresh.running_instances().unwrap(), vec![id.clone()]);
    let stats = fresh
        .stats(&id)
        .unwrap()
        .expect("stats of an adopted instance");
    assert!(stats.process_count >= 2, "{stats:?}");
    assert!(stats.memory_bytes > 0, "{stats:?}");
    assert!(
        fresh
            .adopt_existing(&harness.repository)
            .unwrap()
            .is_empty()
    );

    let outcome = fresh.stop(&id, Duration::from_millis(200)).unwrap();
    assert_eq!(outcome, StopOutcome::Forced);
    assert!(parent.wait_for_exit(EXIT_TIMEOUT));
    assert!(
        child.wait_for_exit(EXIT_TIMEOUT),
        "child {} survived",
        child.process_id
    );
    assert!(!fresh.is_adopted(&id));
    assert_eq!(fresh.status(&id).unwrap(), InstanceProcessStatus::Stopped);
}

#[test]
fn an_adopted_instance_that_crashes_is_reaped_with_its_exit_code() {
    let mut harness = Harness::new();
    harness.set_mode("hang");
    let executable = harness.install_dsh_copy();
    let spec = harness.spec(&executable);
    let process = harness.spawn_unmanaged(&spec);
    let id = harness.id();

    let mut fresh = InstanceProcessManager::default();
    assert!(wait_until(READY_TIMEOUT, || {
        fresh
            .adopt_existing(&harness.repository)
            .is_ok_and(|adopted| adopted == [id.clone()])
    }));

    // Simulate a crash of the adopted process.
    unsafe {
        let _ = TerminateProcess(process.handle, 7);
    }
    assert!(process.wait_for_exit(EXIT_TIMEOUT));
    assert_eq!(
        fresh.status(&id).unwrap(),
        InstanceProcessStatus::Exited {
            process_id: process.process_id,
            exit_code: 7,
        }
    );
    let exits = fresh.reap_exited().unwrap();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].instance_id, id);
    assert_eq!(exits[0].exit_code, 7);
    assert!(!fresh.is_adopted(&id));
}

#[test]
fn adoption_finds_instances_under_a_data_root_with_spaces() {
    // Marina quotes the whole argument ("--user-data-dir=C:\a b\electron"), so
    // the value after `--user-data-dir=` does not start with a quote. A Windows
    // user name with a space puts the default data root in this situation.
    let mut harness = Harness::with_data_dir("Marina Data");
    harness.set_mode("hang");
    let executable = harness.install_dsh_copy();
    let spec = harness.spec(&executable);
    let process = harness.spawn_like_marina_quoting(&spec);
    let id = harness.id();

    let found = wait_until(Duration::from_secs(5), || {
        scan_instance_processes(&harness.repository).is_ok_and(|candidates| {
            candidates
                == [AdoptableInstance {
                    instance_id: id.clone(),
                    process_id: process.process_id,
                }]
        })
    });
    assert!(
        found,
        "the instance under a path with a space was not found"
    );
}

#[test]
fn stopping_a_hung_instance_forces_termination_after_the_grace_period() {
    let mut harness = Harness::new();
    harness.set_mode("hang");
    let spec = harness.spec(&fake_desktop());
    let process = harness.start(&spec);
    let id = harness.id();
    thread::sleep(Duration::from_millis(300));
    assert!(process.is_running());

    let grace = Duration::from_millis(1500);
    let started = Instant::now();
    let outcome = harness.manager.stop(&id, grace).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome, StopOutcome::Forced);
    assert!(
        elapsed >= grace.saturating_sub(Duration::from_millis(100)),
        "stopped after {elapsed:?}, before the grace period ended"
    );
    assert!(
        elapsed < grace + Duration::from_secs(10),
        "stop took {elapsed:?}"
    );
    // stop() returns only after the job is empty.
    assert!(process.wait_for_exit(Duration::ZERO));
    assert_eq!(process.exit_code(), Some(TERMINATED_EXIT_CODE));
    assert_eq!(harness.status(), InstanceProcessStatus::Stopped);
}

#[test]
fn stopping_a_hung_unmanaged_process_by_pid_is_bounded() {
    let mut harness = Harness::new();
    harness.set_mode("hang");
    let spec = harness.spec(&fake_desktop());
    let process = harness.spawn_unmanaged(&spec);

    let grace = Duration::from_millis(500);
    let started = Instant::now();
    let outcome = stop_process_tree_by_id(process.process_id, grace).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome, StopOutcome::Forced);
    assert!(
        elapsed < grace + Duration::from_secs(10),
        "stop took {elapsed:?}"
    );
    assert!(process.wait_for_exit(EXIT_TIMEOUT));
    assert_eq!(process.exit_code(), Some(TERMINATED_EXIT_CODE));
}
