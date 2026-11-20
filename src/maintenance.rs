//! Runtime management and uninstall flows (design §FR-9, §FR-10, §7.8).
//! Everything here blocks and runs on background threads.

use std::path::PathBuf;

use dsh_marina::download::{DownloadSource, FeedOptions};
use dsh_marina::packs::{self, PackError};
use dsh_marina::runtimes::{self, RuntimeSource};
use dsh_marina::settings::MarinaSettings;
use dsh_marina::storage;

use crate::AppState;

/// What to do with Marina's data when uninstalling Marina itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UninstallMode {
    /// Export every instance to a backup folder on the desktop, then delete.
    ExportAll,
    /// Move the whole data directory to the desktop.
    MoveData,
    /// Delete everything.
    DeleteAll,
}

pub fn feed_options(settings: &MarinaSettings) -> FeedOptions {
    // Only the two official sources and the user's own link are offered; an
    // older "GitHub 镜像" choice falls back to the official source.
    let preferred = match settings.download_source {
        DownloadSource::Mirror => DownloadSource::Official,
        source => source,
    };
    FeedOptions {
        preferred,
        stable: settings.stable_only,
        mirror_url: String::new(),
        custom_url: settings.custom_url.clone(),
    }
}

/// Delete a managed runtime that no instance uses.  The directory goes to the
/// system recycle bin so a mistake can be undone.
pub fn remove_runtime(state: &AppState, runtime_id: &str) -> Result<String, String> {
    let runtimes = state.registry.scan().map_err(|e| e.to_string())?;
    let runtime = runtimes
        .iter()
        .find(|r| r.id == runtime_id && r.source == RuntimeSource::Managed)
        .ok_or_else(|| "找不到这个 DSH 版本".to_owned())?;
    let instances = state.repository.list().map_err(|e| e.to_string())?;
    let newest = runtimes.first().map(|r| r.version.as_str());
    let used = runtimes::usage_count(&instances, runtime, newest);
    if used > 0 {
        return Err(format!(
            "还有 {used} 个实例在使用 DSH {}，不能删除",
            runtime.version
        ));
    }
    trash_runtime(state, runtime)
}

fn trash_runtime(state: &AppState, runtime: &runtimes::RuntimeInfo) -> Result<String, String> {
    let directory = state.registry.runtimes_dir().join(&runtime.id);
    if directory.parent() != Some(state.registry.runtimes_dir().as_path()) || !directory.is_dir() {
        return Err("DSH 版本目录无效".to_owned());
    }
    trash::delete(&directory).map_err(|e| format!("删除 DSH {} 失败：{e}", runtime.version))?;
    Ok(format!("已删除 DSH {}（已移到回收站）", runtime.version))
}

/// §7.8 批量解绑: move every instance pinned to `runtime_id` to `binding`
/// (a version, `latest-stable` or the original DSH), then delete the runtime.
pub fn rebind_and_remove_runtime(
    state: &AppState,
    runtime_id: &str,
    binding: &str,
) -> Result<String, String> {
    let runtimes = state.registry.scan().map_err(|e| e.to_string())?;
    let runtime = runtimes
        .iter()
        .find(|r| r.id == runtime_id && r.source == RuntimeSource::Managed)
        .ok_or_else(|| "找不到这个 DSH 版本".to_owned())?
        .clone();
    if binding == runtime.version {
        return Err("请选择另一个版本".to_owned());
    }
    let running = state
        .processes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .running_instances()
        .unwrap_or_default();
    let instances = state.repository.list().map_err(|e| e.to_string())?;
    let newest = runtimes.first().map(|r| r.version.as_str());
    let affected: Vec<_> = instances
        .iter()
        .filter(|i| runtimes::usage_count(std::slice::from_ref(*i), &runtime, newest) > 0)
        .collect();
    if let Some(busy) = affected.iter().find(|i| running.contains(&i.id)) {
        return Err(format!("「{}」正在运行，请先停止再解绑", busy.name));
    }
    let mut moved = 0;
    for instance in &affected {
        if instance.runtime.dsh_version == runtime.version {
            state
                .repository
                .set_runtime_binding(&instance.id, binding)
                .map_err(|e| format!("解绑「{}」失败：{e}", instance.name))?;
            moved += 1;
        }
    }
    // Instances following "最新稳定版" move to the next newest by themselves,
    // as long as one is left (or the original DSH can take over).
    let followers = affected
        .iter()
        .any(|i| i.runtime.dsh_version != runtime.version);
    let others = runtimes
        .iter()
        .any(|r| r.id != runtime.id && r.source == RuntimeSource::Managed);
    let external = state
        .registry
        .all(&state.runtime_store.load().unwrap_or_default())
        .unwrap_or_default()
        .iter()
        .any(|r| r.is_external());
    if followers && !others && !external {
        return Err("还有实例跟随「最新稳定版」，删除后就没有可用的 DSH 了".to_owned());
    }
    let removed = trash_runtime(state, &runtime)?;
    Ok(format!(
        "已把 {moved} 个实例解绑，改用 {}；{removed}",
        crate_runtime_label(binding)
    ))
}

fn crate_runtime_label(binding: &str) -> String {
    match binding {
        runtimes::LATEST_STABLE => "最新稳定版".to_owned(),
        runtimes::EXTERNAL_RUNTIME_ID => "原版 DSH".to_owned(),
        version => format!("DSH {version}"),
    }
}

/// Disk usage of each managed runtime, keyed by runtime ID.
pub fn runtime_sizes(state: &AppState) -> Vec<(String, u64)> {
    state
        .registry
        .scan()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some((
                r.id.clone(),
                storage::directory_size(r.executable.parent()?),
            ))
        })
        .collect()
}

/// Final step of uninstalling Marina, run after every instance has stopped.
/// Schedules removal of the executable (and the data unless it was moved)
/// after Marina exits.  Returns where data was saved, if anywhere.
pub fn uninstall_marina(state: &AppState, mode: UninstallMode) -> Result<Option<PathBuf>, String> {
    let data_root = state.repository.data_root().to_path_buf();
    let saved = match mode {
        UninstallMode::ExportAll => Some(export_all(state)?),
        UninstallMode::MoveData => Some(
            storage::move_data_to_desktop(&data_root).map_err(|e| format!("移动数据失败：{e}"))?,
        ),
        UninstallMode::DeleteAll => None,
    };
    if let Err(e) = dsh_marina::platform::set_auto_start(false) {
        dsh_marina::applog::warn(&format!("移除开机自启失败：{e}"));
    }
    #[cfg(windows)]
    if let Ok(settings) = state.settings_store.load()
        && settings.handle_links
        && let Err(e) =
            dsh_marina::links::registry::restore(settings.previous_link_command.as_deref())
    {
        dsh_marina::applog::warn(&format!("恢复 dsh:// 协议失败：{e}"));
    }
    let executable = dsh_marina::platform::current_executable()?;
    #[cfg(windows)]
    {
        // An NSIS-installed copy owns shortcuts and the "应用和功能" entry, so let
        // its uninstaller remove the program while we only handle the data.
        let installer_uninstaller = executable.with_file_name("uninstall.exe");
        if installer_uninstaller.is_file() {
            storage::spawn_installer_uninstall(&installer_uninstaller)
                .map_err(|e| format!("无法启动卸载程序：{e}"))?;
            return Ok(saved);
        }
        storage::spawn_uninstall_script(&executable, &data_root, mode != UninstallMode::MoveData)
            .map_err(|e| format!("无法安排卸载：{e}"))?;
    }
    #[cfg(not(windows))]
    {
        // macOS: move the .app bundle (or the bare executable) to the Trash.
        // The data directory was already handled above; the running binary can
        // be trashed while Marina is still alive and exits normally afterwards.
        let target = executable
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| executable.clone());
        let remove_data = mode != UninstallMode::MoveData;
        trash::delete(&target).map_err(|e| format!("无法移到废纸篓：{e}"))?;
        if remove_data {
            let _ = trash::delete(&data_root);
        }
    }
    Ok(saved)
}

/// Export every instance as a pack into `Desktop\DSH-Marina-Backup-<date>`.
/// Instances that were never started have nothing to export and are skipped.
fn export_all(state: &AppState) -> Result<PathBuf, String> {
    let folder = storage::desktop_directory().join(format!(
        "DSH-Marina-Backup-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::create_dir_all(&folder).map_err(|e| format!("无法创建备份目录：{e}"))?;
    for instance in state.repository.list().map_err(|e| e.to_string())? {
        let file = folder.join(storage::sanitize_file_name(&format!(
            "{}-{}.marina-pack.zip",
            instance.name, instance.id
        )));
        match packs::export_instance(&state.repository, &instance.id, &file) {
            Ok(_) | Err(PackError::ProfileNotInitialized(_)) => {}
            Err(e) => return Err(format!("导出「{}」失败，已取消卸载：{e}", instance.name)),
        }
    }
    Ok(folder)
}

/// Uninstall the user's original DSH.  Refuses while a Marina instance that
/// uses it is running.
pub fn uninstall_external(state: &AppState, delete_data: bool) -> Result<String, String> {
    let settings = state.runtime_store.load().unwrap_or_default();
    let external = state
        .registry
        .external(settings.dsh_executable.as_deref())
        .ok_or_else(|| "没有检测到原版 DSH".to_owned())?;
    let running: Vec<String> = state
        .lifecycle
        .lock()
        .map(|l| l.running.keys().cloned().collect())
        .unwrap_or_default();
    for id in running {
        if let Ok(instance) = state.repository.load(&id)
            && let Ok(runtime) = state
                .registry
                .resolve(&instance.runtime.dsh_version, &settings)
            && runtime.is_external()
        {
            return Err(format!(
                "实例「{}」正在使用原版 DSH 运行，请先停止",
                instance.name
            ));
        }
    }
    dsh_marina::platform::uninstall_external(&external.executable, delete_data)?;
    // §7.7: forget the original in external.toml as well.
    if let Err(e) = dsh_marina::external::remove_external(state.repository.data_root()) {
        dsh_marina::applog::warn(&format!("移除 external.toml 失败：{e}"));
    }
    Ok(if delete_data {
        "已卸载原版 DSH 并删除它的数据"
    } else {
        "已卸载原版 DSH，数据保留在 ~/.dsh"
    }
    .to_owned())
}
