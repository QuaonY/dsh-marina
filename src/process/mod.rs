//! Process lifecycle management for a single DSH instance.
//!
//! Shared types and the per-instance manager live here; each OS backend
//! provides `ManagedProcess` / `AdoptedProcess` with the same contract:
//! spawn (optionally capturing output), stats, liveness, graceful-then-forced
//! stop, exit code and `reap` (release OS resources after exit).
//!
//! Windows creates the process suspended inside a Job Object; macOS uses a
//! process group (`setpgid`) with SIGTERM → grace → SIGKILL.

use crate::launch::LaunchSpec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("启动进程的参数不合法：{0}")]
    InvalidLaunchInput(String),
    #[error("平台进程操作失败：{0}")]
    Platform(String),
    #[error("等待进程结束失败")]
    WaitFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    ExitedGracefully,
    Forced,
    AlreadyExited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessStats {
    pub memory_bytes: u64,
    pub process_count: u32,
    pub runtime_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptableInstance {
    pub instance_id: String,
    pub process_id: u32,
}

/// Destination for captured child output.
#[derive(Debug, Clone)]
pub struct OutputCapture {
    /// Redacted output is appended here (one file per launch).
    pub log_file: PathBuf,
    /// Receives the backend port from the `dsh web:` line.
    pub endpoint: crate::health::WebEndpoint,
}

impl OutputCapture {
    /// Drain `reader` on a background thread: parse the `dsh web:` port line
    /// and append every line, redacted, to the log file (capped at 4 MiB).
    pub(crate) fn drain_lines<R: std::io::Read + Send + 'static>(self, reader: R) {
        let spawned = std::thread::Builder::new()
            .name("dsh-output".to_owned())
            .spawn(move || self.drain_loop(reader));
        // If the thread cannot start, the closure (and the reader) is dropped.
        drop(spawned);
    }

    fn drain_loop<R: std::io::Read>(self, reader: R) {
        use std::io::{BufRead, Write};
        const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
            .ok();
        let mut written = 0u64;
        let mut lines = std::io::BufReader::new(reader);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match lines.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end();
            if self.endpoint.port().is_none()
                && let Some(port) = crate::health::parse_web_port(line)
            {
                self.endpoint.set(port);
            }
            if let Some(file) = log.as_mut()
                && written < MAX_LOG_BYTES
            {
                let redacted = crate::launch::redact_tokens(line);
                written += redacted.len() as u64 + 1;
                let _ = writeln!(file, "{redacted}");
            }
        }
    }
}

/// Current in-memory state for one instance process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceProcessStatus {
    Stopped,
    Running { process_id: u32 },
    Exited { process_id: u32, exit_code: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub instance_id: String,
    pub process_id: u32,
    pub exit_code: u32,
    pub runtime_seconds: u64,
}

#[derive(Debug, Error)]
pub enum InstanceProcessError {
    #[error("实例已经在运行：{0}")]
    AlreadyRunning(String),
    #[error(transparent)]
    Process(#[from] ProcessError),
}

/// Keeps process handles alive for each instance started by Marina.
#[derive(Default)]
pub struct InstanceProcessManager {
    processes: HashMap<String, ManagedProcess>,
    adopted: HashMap<String, AdoptedProcess>,
}

impl InstanceProcessManager {
    /// Start an instance and retain its handles until it exits or is stopped.
    pub fn start(
        &mut self,
        instance_id: impl Into<String>,
        spec: &LaunchSpec,
    ) -> Result<u32, InstanceProcessError> {
        self.start_captured(instance_id, spec, None)
    }

    /// Start an instance, optionally capturing stdout/stderr (see
    /// [`ManagedProcess::spawn_captured`]).
    pub fn start_captured(
        &mut self,
        instance_id: impl Into<String>,
        spec: &LaunchSpec,
        capture: Option<OutputCapture>,
    ) -> Result<u32, InstanceProcessError> {
        let instance_id = instance_id.into();
        if let Some(process) = self.processes.get(&instance_id) {
            if process.is_running()? {
                return Err(InstanceProcessError::AlreadyRunning(instance_id));
            }
            process.reap()?;
        }
        self.processes.remove(&instance_id);

        let process = ManagedProcess::spawn_captured(spec, capture)?;
        let process_id = process.process_id();
        self.processes.insert(instance_id, process);
        Ok(process_id)
    }

    /// Take ownership of a process that was already running before Marina.
    pub fn adopt(
        &mut self,
        instance_id: impl Into<String>,
        process_id: u32,
    ) -> Result<(), ProcessError> {
        let instance_id = instance_id.into();
        if let Some(existing) = self.adopted.get(&instance_id)
            && existing.process_id == process_id
            && existing.is_running()?
        {
            return Ok(());
        }
        self.adopted.remove(&instance_id);
        self.adopted
            .insert(instance_id, AdoptedProcess::open(process_id)?);
        Ok(())
    }

    pub fn status(&self, instance_id: &str) -> Result<InstanceProcessStatus, InstanceProcessError> {
        if let Some(process) = self.adopted.get(instance_id) {
            return if process.is_running()? {
                Ok(InstanceProcessStatus::Running {
                    process_id: process.process_id,
                })
            } else {
                let exit_code = process.exit_code()?.ok_or(ProcessError::WaitFailed)?;
                Ok(InstanceProcessStatus::Exited {
                    process_id: process.process_id,
                    exit_code,
                })
            };
        }
        let Some(process) = self.processes.get(instance_id) else {
            return Ok(InstanceProcessStatus::Stopped);
        };
        if process.is_running()? {
            Ok(InstanceProcessStatus::Running {
                process_id: process.process_id(),
            })
        } else {
            let exit_code = process.exit_code()?.ok_or(ProcessError::WaitFailed)?;
            Ok(InstanceProcessStatus::Exited {
                process_id: process.process_id(),
                exit_code,
            })
        }
    }

    pub fn stats(&self, instance_id: &str) -> Result<Option<ProcessStats>, InstanceProcessError> {
        if let Some(process) = self.adopted.get(instance_id) {
            if !process.is_running()? {
                return Ok(None);
            }
            return Ok(Some(process.stats()?));
        }
        let Some(process) = self.processes.get(instance_id) else {
            return Ok(None);
        };
        if !process.is_running()? {
            return Ok(None);
        }
        Ok(Some(process.stats()?))
    }

    pub fn is_adopted(&self, instance_id: &str) -> bool {
        self.adopted.contains_key(instance_id)
    }

    pub fn running_instances(&self) -> Result<Vec<String>, InstanceProcessError> {
        let mut running = Vec::new();
        for (instance_id, process) in &self.processes {
            if process.is_running()? {
                running.push(instance_id.clone());
            }
        }
        for (instance_id, process) in &self.adopted {
            if process.is_running()? && !running.contains(instance_id) {
                running.push(instance_id.clone());
            }
        }
        Ok(running)
    }
    /// Remove naturally exited processes and return their exit information.
    pub fn reap_exited(&mut self) -> Result<Vec<ProcessExit>, InstanceProcessError> {
        let mut exited = Vec::new();
        let mut ids = Vec::new();
        for (instance_id, process) in &self.processes {
            if !process.is_running()? {
                let exit_code = process.exit_code()?.ok_or(ProcessError::WaitFailed)?;
                process.reap()?;
                exited.push(ProcessExit {
                    instance_id: instance_id.clone(),
                    process_id: process.process_id(),
                    exit_code,
                    runtime_seconds: process.started_at.elapsed().as_secs(),
                });
                ids.push(instance_id.clone());
            }
        }
        for instance_id in ids {
            self.processes.remove(&instance_id);
        }

        let mut adopted_ids = Vec::new();
        for (instance_id, process) in &self.adopted {
            if !process.is_running()? {
                let exit_code = process.exit_code()?.ok_or(ProcessError::WaitFailed)?;
                exited.push(ProcessExit {
                    instance_id: instance_id.clone(),
                    process_id: process.process_id,
                    exit_code,
                    runtime_seconds: process.started_at.elapsed().as_secs(),
                });
                adopted_ids.push(instance_id.clone());
            }
        }
        for instance_id in adopted_ids {
            self.adopted.remove(&instance_id);
        }
        Ok(exited)
    }

    pub fn stop(
        &mut self,
        instance_id: &str,
        grace_period: Duration,
    ) -> Result<StopOutcome, InstanceProcessError> {
        if let Some(process) = self.adopted.remove(instance_id) {
            return Ok(process.stop(grace_period)?);
        }
        let Some(process) = self.processes.remove(instance_id) else {
            return Ok(StopOutcome::AlreadyExited);
        };
        Ok(process.stop(grace_period)?)
    }

    /// Find running DSH processes that belong to known instances and adopt them.
    pub fn adopt_existing(
        &mut self,
        repository: &crate::instances::InstanceRepository,
    ) -> Result<Vec<String>, InstanceProcessError> {
        let candidates = scan_instance_processes(repository)?;
        let mut adopted = Vec::new();
        for candidate in candidates {
            if self.processes.contains_key(&candidate.instance_id)
                || self.adopted.contains_key(&candidate.instance_id)
            {
                continue;
            }
            match self.adopt(candidate.instance_id.clone(), candidate.process_id) {
                Ok(()) => adopted.push(candidate.instance_id),
                Err(_) => continue,
            }
        }
        Ok(adopted)
    }
}
