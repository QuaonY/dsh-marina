//! Launch pipeline (design §FR-2, §FR-4, §FR-5, §7.3, §7.10): pre-launch
//! snapshot, launch history, captured stdout, crash attribution, last-good
//! marking and optional auto-rollback.  Everything here blocks and runs on
//! background threads.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dsh_marina::crash;
use dsh_marina::health::{self, WebEndpoint};
use dsh_marina::history::{LaunchHistoryStore, LaunchOutcome};
use dsh_marina::launch::LaunchSpec;
use dsh_marina::process::{InstanceProcessStatus, OutputCapture, StopOutcome};
use dsh_marina::snapshots::SnapshotStore;

use crate::AppState;

/// One Marina-started launch that has not finished yet.
struct LaunchSession {
    process_id: u32,
    snapshot_id: Option<String>,
    diagnostic: PathBuf,
    endpoint: WebEndpoint,
    started: Instant,
    /// Wall-clock start, to find crash dumps written by this launch.
    started_wall: std::time::SystemTime,
    electron: PathBuf,
    runtime_version: String,
    version_recorded: bool,
    marked_good: bool,
    /// This launch is the retry after an automatic rollback; never roll back twice.
    auto_retry: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcState {
    Running,
    Adopted,
}

/// Latest view of every instance, shared with the UI.
#[derive(Default)]
pub struct Lifecycle {
    sessions: HashMap<String, LaunchSession>,
    /// Instances whose last launch crashed; cleared on the next start or dismissal.
    pub crashed: HashSet<String>,
    /// Instances with a running process, refreshed by `tick`.
    pub running: HashMap<String, ProcState>,
    pub process_ids: HashMap<String, u32>,
    pub memory: HashMap<String, u64>,
    pub disk: HashMap<String, u64>,
    /// Disk usage of each managed runtime, keyed by runtime ID.
    pub runtime_disk: HashMap<String, u64>,
    /// Instances being rolled back and restarted automatically.
    pub rolling_back: HashSet<String>,
    /// Messages for the UI: `(is_error, text)`.
    pub notices: Vec<(bool, String)>,
    /// The "launched inside a Job" warning was shown once.
    breakaway_warned: bool,
    last_disk_scan: Option<Instant>,
}

pub type SharedLifecycle = Mutex<Lifecycle>;

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, Lifecycle> {
    state
        .lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolve the runtime, snapshot the profile, start with captured output and
/// record the launch.  Returns the process ID.
pub fn launch(state: &AppState, id: &str) -> Result<u32, String> {
    launch_inner(state, id, false, &[])
}

/// Hand a `dsh://` link to an instance (design §14 协议关联).  A stopped
/// instance starts with the link; a running one gets a second launch with the
/// same `--user-data-dir`, which Electron turns into `second-instance` on the
/// running process, so the extra process exits on its own.
pub fn open_link(state: &AppState, id: &str, url: &str) -> Result<bool, String> {
    let url = dsh_marina::links::validate(url)?;
    let running = state
        .processes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .running_instances()
        .unwrap_or_default();
    if !running.iter().any(|r| r == id) {
        launch_inner(state, id, false, &[url.into()])?;
        return Ok(false);
    }
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    let mut spec = LaunchSpec::for_instance(&runtime.executable, &instance, &paths)
        .map_err(|e| format!("无法构建启动参数：{e}"))?;
    spec.arguments.push(url.into());
    dsh_marina::process::spawn_detached(&spec).map_err(|e| format!("无法把链接交给实例：{e}"))?;
    Ok(true)
}

fn launch_inner(
    state: &AppState,
    id: &str,
    auto_retry: bool,
    extra_arguments: &[std::ffi::OsString],
) -> Result<u32, String> {
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    let mut spec = LaunchSpec::for_instance(&runtime.executable, &instance, &paths)
        .map_err(|e| format!("无法构建启动参数：{e}"))?;
    spec.arguments.extend(extra_arguments.iter().cloned());

    // §14: DSH's own updater may rewrite a managed runtime in place.
    if state.registry.verify_fingerprint(&runtime) == Some(false) {
        dsh_marina::applog::warn(&format!("DSH {} 的程序文件和安装时不一致", runtime.version));
        lock(state).notices.push((
            true,
            format!("DSH {} 的程序文件和安装时不一样（可能被 DSH 自己的更新改动过），建议在设置里重新安装这个版本", runtime.version),
        ));
    }

    // A failed snapshot must not block the launch; attribution just gets weaker.
    let snapshot_id = match SnapshotStore::new(&paths.root).pre_launch() {
        Ok(snapshot) => snapshot.map(|s| s.id),
        Err(e) => {
            dsh_marina::applog::warn(&format!("实例 {id} 的启动前快照失败：{e}"));
            None
        }
    };

    std::fs::create_dir_all(&paths.marina).map_err(|e| format!("无法创建日志目录：{e}"))?;
    let log_file = paths.marina.join(format!(
        "stdout-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let endpoint = WebEndpoint::default();
    let capture = OutputCapture {
        log_file,
        endpoint: endpoint.clone(),
    };

    let process_id = state
        .processes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .start_captured(id, &spec, Some(capture))
        .map_err(|e| e.to_string())?;

    dsh_marina::applog::info(&format!(
        "已启动 {id}（PID {process_id}，DSH {}）",
        runtime.version
    ));
    if dsh_marina::process::breakaway_denied() && !lock(state).breakaway_warned {
        let mut lifecycle = lock(state);
        lifecycle.breakaway_warned = true;
        lifecycle.notices.push((
            true,
            "Marina 是从一个限制子进程的环境（例如某些终端）里启动的，关闭那个环境时实例可能被一起结束。建议从开始菜单或桌面快捷方式启动 Marina".to_owned(),
        ));
    }
    if let Err(e) = LaunchHistoryStore::new(&paths.root).record_started(
        process_id,
        Some(spec.diagnostic_file.clone()),
        snapshot_id.clone(),
    ) {
        dsh_marina::applog::warn(&format!("实例 {id} 的启动记录写入失败：{e}"));
    }

    let mut lifecycle = lock(state);
    lifecycle.crashed.remove(id);
    lifecycle.running.insert(id.to_owned(), ProcState::Running);
    lifecycle.process_ids.insert(id.to_owned(), process_id);
    lifecycle.sessions.insert(
        id.to_owned(),
        LaunchSession {
            process_id,
            snapshot_id,
            diagnostic: spec.diagnostic_file,
            endpoint,
            started: Instant::now(),
            started_wall: std::time::SystemTime::now(),
            electron: paths.electron.clone(),
            runtime_version: runtime.version,
            version_recorded: false,
            marked_good: false,
            auto_retry,
        },
    );
    Ok(process_id)
}

/// Stop an instance and record how it ended.
pub fn stop(state: &AppState, id: &str, grace: Duration) -> Result<StopOutcome, String> {
    let outcome = state
        .processes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop(id, grace)
        .map_err(|e| e.to_string())?;
    let session = {
        let mut lifecycle = lock(state);
        lifecycle.running.remove(id);
        lifecycle.process_ids.remove(id);
        lifecycle.memory.remove(id);
        lifecycle.crashed.remove(id);
        lifecycle.sessions.remove(id)
    };
    if let Some(session) = session
        && let Ok(paths) = state.repository.paths(id)
    {
        let recorded = match outcome {
            StopOutcome::Forced => LaunchOutcome::ForcedStop,
            StopOutcome::ExitedGracefully | StopOutcome::AlreadyExited => LaunchOutcome::Stopped,
        };
        let _ = LaunchHistoryStore::new(&paths.root).record_finished(
            session.process_id,
            recorded,
            None,
            Some(session.started.elapsed().as_secs()),
        );
    }
    Ok(outcome)
}

pub fn dismiss_crash(state: &AppState, id: &str) {
    lock(state).crashed.remove(id);
}

pub fn take_notices(state: &AppState) -> Vec<(bool, String)> {
    std::mem::take(&mut lock(state).notices)
}

/// Adopt DSH processes left over from a previous Marina run and clean the
/// 30-day trash (design §7.10).
pub fn startup(state: &AppState) {
    match dsh_marina::storage::cleanup_trash(state.repository.data_root()) {
        Ok(removed) if removed > 0 => {
            dsh_marina::applog::info(&format!("清理了 {removed} 个过期的回收站条目"))
        }
        Ok(_) => {}
        Err(e) => dsh_marina::applog::warn(&format!("回收站清理失败：{e}")),
    }
    let adopted = state
        .processes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .adopt_existing(&state.repository);
    match adopted {
        Ok(ids) if !ids.is_empty() => {
            lock(state)
                .notices
                .push((false, format!("已接管 {} 个正在运行的实例", ids.len())));
        }
        Ok(_) => {}
        Err(e) => dsh_marina::applog::warn(&format!("接管已有进程失败：{e}")),
    }
    finalize_stale_history(state);
}

/// Records left in `Running` by a previous Marina process (killed, power loss)
/// are closed as forced stops — except for instances adopted just now.
fn finalize_stale_history(state: &AppState) {
    let live: HashSet<String> = {
        let processes = state
            .processes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .repository
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|config| {
                matches!(
                    processes.status(&config.id),
                    Ok(InstanceProcessStatus::Running { .. })
                )
            })
            .map(|config| config.id)
            .collect()
    };
    for config in state.repository.list().unwrap_or_default() {
        if live.contains(&config.id) {
            continue;
        }
        let Ok(paths) = state.repository.paths(&config.id) else {
            continue;
        };
        match LaunchHistoryStore::new(&paths.root).finalize_stale() {
            Ok(closed) if closed > 0 => dsh_marina::applog::info(&format!(
                "实例 {} 有 {closed} 条未收尾的启动记录，已标记为强制结束",
                config.name
            )),
            Ok(_) => {}
            Err(e) => {
                dsh_marina::applog::warn(&format!("修正实例 {} 的启动历史失败：{e}", config.name))
            }
        }
    }
}

struct Exit {
    id: String,
    process_id: u32,
    exit_code: u32,
    runtime_seconds: u64,
}

/// Periodic monitor: reap exits, refresh stats, mark stable launches.
pub fn tick(state: &Arc<AppState>, instance_ids: &[String]) {
    // Never wait for the process lock: a stop may hold it for the grace period.
    let Ok(mut processes) = state.processes.try_lock() else {
        return;
    };
    let exits: Vec<Exit> = processes
        .reap_exited()
        .unwrap_or_default()
        .into_iter()
        .map(|e| Exit {
            id: e.instance_id,
            process_id: e.process_id,
            exit_code: e.exit_code,
            runtime_seconds: e.runtime_seconds,
        })
        .collect();
    let mut running = HashMap::new();
    let mut process_ids = HashMap::new();
    let mut memory = HashMap::new();
    for id in instance_ids {
        if let Ok(InstanceProcessStatus::Running { process_id }) = processes.status(id) {
            let kind = if processes.is_adopted(id) {
                ProcState::Adopted
            } else {
                ProcState::Running
            };
            running.insert(id.clone(), kind);
            process_ids.insert(id.clone(), process_id);
            if let Ok(Some(stats)) = processes.stats(id) {
                memory.insert(id.clone(), stats.memory_bytes);
            }
        }
    }
    drop(processes);

    let mut finished = Vec::new();
    let mut stable = Vec::new();
    let mut probe = Vec::new();
    {
        let mut lifecycle = lock(state);
        lifecycle.running = running;
        lifecycle.process_ids = process_ids;
        lifecycle.memory = memory;
        for exit in exits {
            let session = lifecycle
                .sessions
                .remove(&exit.id)
                .filter(|s| s.process_id == exit.process_id);
            finished.push((exit, session));
        }
        for (id, session) in lifecycle.sessions.iter_mut() {
            if !session.version_recorded
                && let Some(port) = session.endpoint.port()
            {
                probe.push((id.clone(), port, session.runtime_version.clone()));
            }
            if !session.marked_good {
                let threshold = state
                    .repository
                    .load(id)
                    .map(|c| c.safety.stable_seconds)
                    .unwrap_or(180);
                if session.started.elapsed().as_secs() >= threshold {
                    session.marked_good = true;
                    stable.push((
                        id.clone(),
                        session.snapshot_id.clone(),
                        session.runtime_version.clone(),
                    ));
                }
            }
        }
    }

    for (id, port, version) in probe {
        if health::probe_port(port, Duration::from_millis(300)) {
            let _ = state.repository.set_last_used_version(&id, version);
            if let Some(session) = lock(state).sessions.get_mut(&id) {
                session.version_recorded = true;
            }
        }
    }

    for (id, snapshot_id, version) in stable {
        let _ = state.repository.set_last_used_version(&id, version);
        if let Some(snapshot_id) = snapshot_id
            && let Ok(paths) = state.repository.paths(&id)
            && let Err(e) = SnapshotStore::new(&paths.root).mark_last_good(&snapshot_id)
        {
            dsh_marina::applog::warn(&format!("标记实例 {id} 的可用快照失败：{e}"));
        }
    }

    for (exit, session) in finished {
        handle_exit(state, exit, session);
    }

    refresh_disk_usage(state, instance_ids);
}

fn handle_exit(state: &Arc<AppState>, exit: Exit, session: Option<LaunchSession>) {
    let Ok(paths) = state.repository.paths(&exit.id) else {
        return;
    };
    let history = LaunchHistoryStore::new(&paths.root);
    let name = state
        .repository
        .load(&exit.id)
        .map(|c| c.name)
        .unwrap_or_else(|_| exit.id.clone());

    if exit.exit_code == 0 {
        let _ = history.record_finished(
            exit.process_id,
            LaunchOutcome::Exited,
            Some(0),
            Some(exit.runtime_seconds),
        );
        return;
    }

    let _ = history.record_finished(
        exit.process_id,
        LaunchOutcome::Crashed,
        Some(exit.exit_code),
        Some(exit.runtime_seconds),
    );
    let Some(session) = session else {
        // An adopted process exited: no snapshot to compare against.
        lock(state).crashed.insert(exit.id.clone());
        return;
    };

    // Second signal (§FR-5): Crashpad dumps under the instance's userData.
    let reports = crash::crash_reports_since(&session.electron, session.started_wall);
    let store = SnapshotStore::new(&paths.root);
    let mut suspects = crash::compute_suspects(
        &store,
        session.snapshot_id.as_deref(),
        Some(&session.diagnostic),
    );
    if suspects.is_empty()
        && !reports.is_empty()
        && let Some(before) = session
            .snapshot_id
            .as_deref()
            .and_then(|id| store.get(id).ok())
        && let Ok(Some(last_good)) = store.last_good()
    {
        let texts: Vec<String> = reports
            .iter()
            .take(3)
            .map(|r| crash::read_report_text(r))
            .collect();
        suspects = crash::suspects_from_texts(&before, &last_good, &texts);
    }
    if !reports.is_empty() {
        dsh_marina::applog::warn(&format!(
            "{}：新增 {} 份崩溃报告，例如 {}",
            exit.id,
            reports.len(),
            reports[0].display()
        ));
    }
    if !suspects.is_empty() {
        let _ = history.set_suspects(exit.process_id, suspects.clone());
    }

    // A crash after the stable period is a runtime crash: report only (§FR-5).
    let startup_crash = !session.marked_good;
    let config = state.repository.load(&exit.id).ok();
    let auto_rollback = config.as_ref().is_some_and(|c| c.safety.auto_rollback);
    let last_good = store.last_good().ok().flatten();

    if startup_crash
        && auto_rollback
        && !session.auto_retry
        && let Some(last_good) = last_good
    {
        let mut lifecycle = lock(state);
        lifecycle.rolling_back.insert(exit.id.clone());
        lifecycle.notices.push((
            false,
            format!("「{name}」启动失败，正在自动回滚到上次正常状态…"),
        ));
        drop(lifecycle);
        // Reinstalling dependencies can take minutes; keep the monitor ticking.
        let state = state.clone();
        let id = exit.id.clone();
        std::thread::spawn(move || {
            let result = rollback_and_restart(&state, &id, &last_good.id);
            let mut lifecycle = lock(&state);
            lifecycle.rolling_back.remove(&id);
            match result {
                Ok(()) => {
                    let mut message = format!("「{name}」已自动恢复到上次正常状态");
                    if !suspects.is_empty() {
                        message.push_str(&format!("，嫌疑插件：{}", suspects.join("、")));
                    }
                    lifecycle.notices.push((false, message));
                }
                Err(e) => {
                    lifecycle.crashed.insert(id.clone());
                    lifecycle
                        .notices
                        .push((true, format!("「{name}」自动回滚失败：{e}")));
                }
            }
        });
        return;
    }

    let mut lifecycle = lock(state);
    lifecycle.crashed.insert(exit.id.clone());
    let kind = if startup_crash {
        "启动失败"
    } else {
        "运行中崩溃"
    };
    let report = if reports.is_empty() {
        String::new()
    } else {
        format!("，生成了 {} 份崩溃报告", reports.len())
    };
    dsh_marina::applog::error(&format!("{} {kind}, exit code {}", exit.id, exit.exit_code));
    lifecycle.notices.push((
        true,
        format!("「{name}」{kind}（退出码 {}）{report}", exit.exit_code),
    ));
}

/// Restore a snapshot, make node_modules match it, then start again (§7.4).
fn rollback_and_restart(state: &AppState, id: &str, snapshot_id: &str) -> Result<(), String> {
    let warning = restore_and_install(state, id, snapshot_id)?;
    if let Some(warning) = warning {
        dsh_marina::applog::warn(&warning);
    }
    launch_inner(state, id, true, &[]).map(|_| ())
}

/// Restore a snapshot and run `dsh plugin install`.  The install is best
/// effort: its failure is returned as a warning, not an error.
pub fn restore_and_install(
    state: &AppState,
    id: &str,
    snapshot_id: &str,
) -> Result<Option<String>, String> {
    require_stopped(state, id, "恢复快照")?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    SnapshotStore::new(&paths.root)
        .restore(snapshot_id)
        .map_err(|e| format!("恢复快照失败：{e}"))?;
    Ok(install_plugins(state, id).err())
}

/// `dsh plugin --profile desktop install` for a stopped instance.
pub fn install_plugins(state: &AppState, id: &str) -> Result<(), String> {
    require_stopped(state, id, "重新安装插件依赖")?;
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    dsh_marina::dsh_cli::install(&runtime.executable, &paths.home)
        .map(|_| ())
        .map_err(|e| format!("重新安装插件依赖失败：{e}"))
}

/// FR-12: `(last used version, version about to run)` when they differ by a
/// major version.
pub fn compat_warning(state: &AppState, id: &str) -> Option<(String, String)> {
    let instance = state.repository.load(id).ok()?;
    let previous = instance.runtime.last_used_version?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let next = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .ok()?
        .version;
    dsh_marina::runtimes::is_major_change(&previous, &next).then_some((previous, next))
}

pub fn restart(state: &AppState, id: &str, grace: Duration) -> Result<u32, String> {
    stop(state, id, grace)?;
    launch(state, id)
}

/// The desktop app keeps the profile files open while it runs, so every write
/// to them asks for a stopped instance first, whatever the caller does.
pub(crate) fn require_stopped(state: &AppState, id: &str, action: &str) -> Result<(), String> {
    let running = state
        .processes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .running_instances()
        .unwrap_or_default();
    if running.iter().any(|r| r == id) {
        return Err(format!("实例正在运行，请先停止再{action}。"));
    }
    Ok(())
}

/// Safe mode (§FR-5): back up the profile as a snapshot, then disable every
/// third-party plugin.  Returns how many plugins were disabled.
pub fn safe_mode(state: &AppState, id: &str) -> Result<usize, String> {
    require_stopped(state, id, "修改插件配置")?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    SnapshotStore::new(&paths.root)
        .create("安全模式前自动备份")
        .map_err(|e| format!("备份插件配置失败：{e}"))?;
    dsh_marina::plugins::safe_mode(&paths.home).map_err(|e| e.to_string())
}

/// `dsh plugin --profile desktop remove <package>` for a stopped instance.
pub fn uninstall_plugin(state: &AppState, id: &str, package: &str) -> Result<(), String> {
    require_stopped(state, id, "卸载插件")?;
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    SnapshotStore::new(&paths.root)
        .create(format!("卸载插件 {package} 前自动备份"))
        .map_err(|e| format!("备份插件配置失败：{e}"))?;
    dsh_marina::dsh_cli::remove(&runtime.executable, &paths.home, package)
        .map(|_| ())
        .map_err(|e| format!("卸载插件失败：{e}"))
}

/// After an import or copy (§FR-6): plugin settings that point at absolute
/// paths outside the new DSH_HOME will keep writing to the old place, which
/// breaks isolation.  Returns the distinct paths found.
pub fn outside_paths(state: &AppState, id: &str) -> Vec<String> {
    use dsh_marina::sensitive::{SensitiveKind, scan_text};
    let Ok(paths) = state.repository.paths(id) else {
        return Vec::new();
    };
    let home = paths
        .home
        .to_string_lossy()
        .to_lowercase()
        .replace('/', "\\");
    let profile = paths.home.join("profiles").join("desktop");
    let mut found = Vec::new();
    for file in dsh_marina::snapshots::PROFILE_FILES {
        let Ok(text) = std::fs::read_to_string(profile.join(file)) else {
            continue;
        };
        for finding in scan_text(file, &text, None) {
            if finding.kind != SensitiveKind::AbsolutePath {
                continue;
            }
            let normalized = finding
                .excerpt
                .to_lowercase()
                .replace("\\\\", "\\")
                .replace('/', "\\");
            if !normalized.contains(&home) && !found.contains(&finding.excerpt) {
                found.push(finding.excerpt);
            }
        }
    }
    found
}

/// FR-5 二分排查: the enabled plugins to bisect, after a safety snapshot.
pub fn bisect_begin(state: &AppState, id: &str) -> Result<Vec<String>, String> {
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    SnapshotStore::new(&paths.root)
        .create("二分排查前自动备份")
        .map_err(|e| format!("备份插件配置失败：{e}"))?;
    let plugins = dsh_marina::plugins::list_plugins(&paths.home).map_err(|e| e.to_string())?;
    Ok(plugins
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| p.name)
        .collect())
}

/// One round: stop the instance, enable every candidate except `disabled`,
/// then start it so the user can check.
pub fn bisect_round(
    state: &AppState,
    id: &str,
    candidates: &[String],
    disabled: &[String],
) -> Result<(), String> {
    stop(state, id, crate::STOP_GRACE)?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let enabled: Vec<String> = candidates
        .iter()
        .filter(|c| !disabled.contains(c))
        .cloned()
        .collect();
    dsh_marina::plugins::enable_plugins(&paths.home, &enabled).map_err(|e| e.to_string())?;
    dsh_marina::plugins::disable_plugins(&paths.home, disabled).map_err(|e| e.to_string())?;
    launch(state, id).map(|_| ())
}

/// End the bisect: re-enable `all`, then disable `culprit` if one was found.
pub fn bisect_finish(
    state: &AppState,
    id: &str,
    all: &[String],
    culprit: Option<&str>,
) -> Result<(), String> {
    stop(state, id, crate::STOP_GRACE)?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    dsh_marina::plugins::enable_plugins(&paths.home, all).map_err(|e| e.to_string())?;
    if let Some(culprit) = culprit {
        dsh_marina::plugins::disable_plugins(&paths.home, &[culprit.to_owned()])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// FR-3 离线安装: add local `.tgz` packages to a stopped instance with the
/// bundled CLI.  The previous configuration is kept as a snapshot.
pub fn add_plugin_packages(state: &AppState, id: &str, packages: &[PathBuf]) -> Result<(), String> {
    require_stopped(state, id, "安装插件")?;
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    SnapshotStore::new(&paths.root)
        .create("离线安装插件前自动备份".to_owned())
        .map_err(|e| format!("备份插件配置失败：{e}"))?;
    let specs: Vec<String> = packages
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    dsh_marina::dsh_cli::add_packages(&runtime.executable, &paths.home, &specs)
        .map(|_| ())
        .map_err(|e| format!("安装插件失败：{e}"))
}

/// 插件市场: install `spec` (`name@version`) from the npm registry into the
/// stopped instance `id`.  The previous configuration is kept as a snapshot,
/// so a plugin that breaks the instance can be rolled back.
pub fn install_market_plugin(
    state: &AppState,
    id: &str,
    spec: &str,
    title: &str,
) -> Result<(), String> {
    require_stopped(state, id, "安装插件")?;
    let instance = state.repository.load(id).map_err(|e| e.to_string())?;
    let paths = state.repository.paths(id).map_err(|e| e.to_string())?;
    if !paths
        .home
        .join("profiles")
        .join("desktop")
        .join("package.json")
        .is_file()
    {
        return Err(format!(
            "「{}」还没有启动过，先启动一次让 DSH 完成初始化，关掉后再安装",
            instance.name
        ));
    }
    let settings = state.runtime_store.load().unwrap_or_default();
    let runtime = state
        .registry
        .resolve(&instance.runtime.dsh_version, &settings)
        .map_err(|e| format!("无法解析运行时：{e}"))?;
    SnapshotStore::new(&paths.root)
        .create(format!("从插件市场安装 {title} 前自动备份"))
        .map_err(|e| format!("备份插件配置失败：{e}"))?;
    dsh_marina::dsh_cli::add_packages(&runtime.executable, &paths.home, &[spec])
        .map(|_| ())
        .map_err(|e| format!("安装插件失败：{e}"))
}

/// Copy the plugin combination (profile files and local packages) of `source`
/// into the stopped instance `target`, then reinstall (§FR-3).  The target's
/// previous configuration is kept as a snapshot.
pub fn copy_plugins(
    state: &AppState,
    source: &str,
    target: &str,
) -> Result<Option<String>, String> {
    use dsh_marina::snapshots::PROFILE_FILES;
    require_stopped(state, source, "复制插件配置")?;
    require_stopped(state, target, "复制插件配置")?;
    let from = state
        .repository
        .paths(source)
        .map_err(|e| e.to_string())?
        .home
        .join("profiles/desktop");
    let target_paths = state.repository.paths(target).map_err(|e| e.to_string())?;
    if !from.is_dir() {
        return Err("源实例还没有插件配置（需要先启动一次）".to_owned());
    }
    let to = target_paths.home.join("profiles/desktop");
    if to.is_dir() {
        SnapshotStore::new(&target_paths.root)
            .create("复制插件组合前自动备份")
            .map_err(|e| format!("备份目标实例配置失败：{e}"))?;
    }
    let io = |e: std::io::Error| format!("复制插件配置失败：{e}");
    std::fs::create_dir_all(&to).map_err(io)?;
    for name in PROFILE_FILES {
        let (src, dst) = (from.join(name), to.join(name));
        if src.is_file() {
            std::fs::copy(&src, &dst).map_err(io)?;
        } else if dst.is_file() {
            std::fs::remove_file(&dst).map_err(io)?;
        }
    }
    let (src_packages, dst_packages) = (from.join("packages"), to.join("packages"));
    let is_tgz =
        |path: &std::path::Path| path.is_file() && path.extension().is_some_and(|e| e == "tgz");
    if dst_packages.is_dir() {
        for entry in std::fs::read_dir(&dst_packages).map_err(io)?.flatten() {
            if is_tgz(&entry.path()) && !src_packages.join(entry.file_name()).is_file() {
                std::fs::remove_file(entry.path()).map_err(io)?;
            }
        }
    }
    if src_packages.is_dir() {
        std::fs::create_dir_all(&dst_packages).map_err(io)?;
        for entry in std::fs::read_dir(&src_packages).map_err(io)?.flatten() {
            if is_tgz(&entry.path()) {
                std::fs::copy(entry.path(), dst_packages.join(entry.file_name())).map_err(io)?;
            }
        }
    }
    Ok(install_plugins(state, target).err())
}

/// Directory sizes are slow to compute, so refresh them at most once a minute.
fn refresh_disk_usage(state: &AppState, instance_ids: &[String]) {
    {
        let mut lifecycle = lock(state);
        if lifecycle
            .last_disk_scan
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        lifecycle.last_disk_scan = Some(Instant::now());
    }
    let mut disk = HashMap::new();
    for id in instance_ids {
        if let Ok(paths) = state.repository.paths(id) {
            disk.insert(id.clone(), dsh_marina::storage::directory_size(&paths.root));
        }
    }
    let runtime_disk = crate::maintenance::runtime_sizes(state)
        .into_iter()
        .collect();
    let mut lifecycle = lock(state);
    lifecycle.disk = disk;
    lifecycle.runtime_disk = runtime_disk;
}

/// Force the next tick to recompute disk usage (after clone, import, delete).
pub fn invalidate_disk_usage(state: &AppState) {
    lock(state).last_disk_scan = None;
}
