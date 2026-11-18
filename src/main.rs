//! GPUI-based DSH Marina launcher.

// 发布版是 GUI 程序：不弹出控制台窗口（调试版保留控制台，便于看日志）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod github;
mod github_pr;
mod lifecycle;
mod maintenance;
mod ui;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use gpui_tray::{Icon as TrayIconImage, Tray};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ui::runtime_label;

use dsh_marina::catalog;
use dsh_marina::copy;
use dsh_marina::download;
use dsh_marina::history::{LaunchHistoryStore, LaunchOutcome};
use dsh_marina::instances::{InstanceConfig, InstanceRepository, InstanceStatus};
use dsh_marina::links;
use dsh_marina::logs::{latest_log, tail};
use dsh_marina::packs;
use dsh_marina::platform::{self, APP_TITLE, TrayIconState};
use dsh_marina::plugins::list_plugins;
use dsh_marina::process::InstanceProcessManager;
use dsh_marina::relocate;
use dsh_marina::runtime::RuntimeSettingsStore;
use dsh_marina::runtimes::{EXTERNAL_RUNTIME_ID, LATEST_STABLE, RuntimeRegistry};
use dsh_marina::settings::SettingsStore;
use dsh_marina::snapshots::SnapshotStore;
use dsh_marina::storage::{self};

actions!(marina, [ShowWindow, QuitApp]);

/// How long a stopping instance gets to close its window before the job is terminated.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// `%USERPROFILE%\.dsh-marina`, where the data lives unless it was moved.
fn default_data_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join(".dsh-marina")
}

/// `DSH_MARINA_DATA` overrides everything; otherwise the location chosen in
/// settings (pointer file beside the default root) is used.
fn data_root_override() -> Option<PathBuf> {
    std::env::var_os("DSH_MARINA_DATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Get Marina's data root directory.
fn data_root() -> PathBuf {
    if let Some(custom) = data_root_override() {
        return custom;
    }
    let default = default_data_root();
    relocate::resolve(
        &default,
        &relocate::read_pointer(&relocate::default_pointer_file(&default)),
    )
}

/// Value of `--name <value>` on the command line.
fn argument_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

/// Ensure all required directories exist.
fn ensure_dirs(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root.join("instances"))?;
    fs::create_dir_all(root.join("runtimes"))?;
    fs::create_dir_all(root.join("download"))?;
    fs::create_dir_all(root.join("trash"))?;
    fs::create_dir_all(root.join("exports"))?;
    Ok(())
}

/// Application state shared across the UI.
struct AppState {
    data_root: PathBuf,
    repository: Arc<InstanceRepository>,
    registry: Arc<RuntimeRegistry>,
    runtime_store: Arc<RuntimeSettingsStore>,
    settings_store: Arc<SettingsStore>,
    processes: Arc<Mutex<InstanceProcessManager>>,
    lifecycle: lifecycle::SharedLifecycle,
}

impl AppState {
    fn new(data_root: &Path) -> Self {
        Self {
            data_root: data_root.to_path_buf(),
            repository: Arc::new(InstanceRepository::new(data_root)),
            registry: Arc::new(RuntimeRegistry::new(data_root)),
            runtime_store: Arc::new(RuntimeSettingsStore::new(data_root)),
            settings_store: Arc::new(SettingsStore::new(data_root)),
            processes: Arc::new(Mutex::new(InstanceProcessManager::default())),
            lifecycle: Default::default(),
        }
    }
}

/// App-wide state reachable from actions, the tray and window callbacks.
struct MarinaApp {
    state: Arc<AppState>,
    /// Raw HWND of the main window, used to hide/show it for the tray.
    hwnd: Option<isize>,
    /// Set while the exit flow (stop instances, then quit) is running.
    exiting: bool,
    window: Option<WeakEntity<MarinaWindow>>,
    /// Instance rows shown in the tray menu; the menu is rebuilt when they change.
    tray_menu: Vec<TrayEntry>,
}

#[derive(Clone, PartialEq)]
struct TrayEntry {
    id: String,
    name: String,
    status: InstanceStatus,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TrayOp {
    Open,
    Focus,
    Start,
    Stop,
    Restart,
    ViewCrash,
    Rollback,
    NewInstance,
    OpenLink,
    Settings,
    Quit,
}

/// A tray menu command; `id` is empty for the global entries.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = marina, no_json)]
struct TrayCommand {
    op: TrayOp,
    id: String,
}

impl Global for MarinaApp {}

struct MarinaTray {
    tray: Tray,
}

impl Global for MarinaTray {}

fn tray_icon(state: TrayIconState) -> Option<TrayIconImage> {
    const SIZE: u32 = 32;
    let rgba = platform::build_anchor_icon(SIZE, state);
    TrayIconImage::from_rgba(rgba, SIZE, SIZE).build().ok()
}

fn show_main_window(_: &ShowWindow, cx: &mut App) {
    if let Some(hwnd) = cx.try_global::<MarinaApp>().and_then(|app| app.hwnd) {
        platform::show_window(hwnd);
    } else {
        platform::activate_window(APP_TITLE);
    }
}

fn quit_app(_: &QuitApp, cx: &mut App) {
    request_quit(cx);
}

/// How running instances are ended when Marina quits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopMode {
    /// Ask each instance to exit, then end its Job after the grace period.
    Graceful,
    /// End every Job immediately.
    Force,
}

/// Work run after every instance stopped and before Marina quits.
type QuitHook = Box<dyn FnOnce(&AppState) -> Result<(), String> + Send>;

/// Exit flow (§FR-2): with running instances, ask first in the window
/// (取消 / 先停止实例再退出 / 强制退出), then stop everything and quit.
fn request_quit(cx: &mut App) {
    let app = cx.global::<MarinaApp>();
    if app.exiting {
        return;
    }
    let state = app.state.clone();
    let window = app.window.clone();
    let hwnd = app.hwnd;

    proceed_with_quit_check(state, window, hwnd, cx);
}

/// 继续退出流程，检查是否有运行中的实例需要确认
fn proceed_with_quit_check(
    state: Arc<AppState>,
    window: Option<WeakEntity<MarinaWindow>>,
    hwnd: Option<isize>,
    cx: &mut App,
) {
    let confirm = state
        .settings_store
        .load()
        .map(|s| s.confirm_on_exit)
        .unwrap_or(true);
    let running = state.lifecycle.lock().map(|l| l.running.len()).unwrap_or(0);
    if running == 0 || !confirm {
        perform_quit(StopMode::Graceful, None, cx);
        return;
    }
    if let Some(hwnd) = hwnd {
        platform::show_window(hwnd);
    }
    let shown = window
        .clone()
        .and_then(|w| w.upgrade())
        .map(|w| {
            w.update(cx, |this, cx| {
                this.dialog = Some(Dialog::Exit { running });
                cx.notify();
            })
        })
        .is_some();
    if !shown {
        // No window to ask in: fall back to the system message box.
        cx.spawn(async move |cx| {
            let proceed = cx
                .background_executor()
                .spawn(async move { platform::confirm_exit(running) })
                .await;
            if proceed {
                cx.update(|cx| perform_quit(StopMode::Graceful, None, cx));
            }
        })
        .detach();
    }
}

/// Stop every instance, run `hook`, then close the tray and quit.  A failing
/// hook cancels the quit and reports the error in the window.
fn perform_quit(mode: StopMode, hook: Option<QuitHook>, cx: &mut App) {
    let app = cx.global_mut::<MarinaApp>();
    if app.exiting {
        return;
    }
    app.exiting = true;
    let state = app.state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let running = state
                    .processes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .running_instances()
                    .unwrap_or_default();
                let grace = if mode == StopMode::Force {
                    Duration::ZERO
                } else {
                    STOP_GRACE
                };
                for id in running {
                    if let Err(e) = lifecycle::stop(&state, &id, grace) {
                        dsh_marina::applog::warn(&format!("退出时停止实例 {id} 失败：{e}"));
                    }
                }
                match hook {
                    Some(hook) => hook(&state),
                    None => Ok(()),
                }
            })
            .await;

        cx.update(|cx| match result {
            Ok(()) => {
                if let Some(tray) = cx.try_global::<MarinaTray>().map(|t| t.tray.clone())
                    && let Err(e) = tray.close(cx)
                {
                    dsh_marina::applog::warn(&format!("隐藏托盘失败：{e}"));
                }
                dsh_marina::applog::info("DSH Marina 正在退出");
                dsh_marina::applog::flush();
                cx.quit();
            }
            Err(e) => {
                let app = cx.global_mut::<MarinaApp>();
                app.exiting = false;
                if let Some(window) = app.window.clone().and_then(|w| w.upgrade()) {
                    window.update(cx, |this, cx| {
                        this.last_error = Some(e);
                        this.refresh_instances(cx);
                        cx.notify();
                    });
                }
            }
        });
    })
    .detach();
}
/// Queued `dsh://` links.  While Marina handles links it also takes the
/// protocol back from DSH, which re-registers itself on every launch.
fn poll_links(state: &AppState) -> Vec<String> {
    let settings = state.settings_store.load().unwrap_or_default();
    if settings.handle_links
        && let Ok(exe) = platform::current_executable()
    {
        let current = links::registry::current_command();
        if !current
            .as_deref()
            .is_some_and(|c| links::is_marina_command(c, &exe))
        {
            if let Some(command) = current
                && settings.previous_link_command.as_deref() != Some(command.as_str())
            {
                let mut updated = settings.clone();
                updated.previous_link_command = Some(command);
                let _ = state.settings_store.save(&updated);
            }
            if let Err(e) = links::registry::register(&exe) {
                dsh_marina::applog::warn(&format!("重新注册 dsh:// 协议失败：{e}"));
            }
        }
    }
    links::drain(state.repository.data_root())
}

/// Window close button: always hide to tray.
fn handle_window_close(cx: &mut App) -> bool {
    let Some(app) = cx.try_global::<MarinaApp>() else {
        return true;
    };
    let store = app.state.settings_store.clone();
    let hwnd = app.hwnd;
    let settings = store.load().unwrap_or_default();

    // 总是隐藏到托盘，不退出
    if let Some(hwnd) = hwnd {
        platform::hide_window(hwnd);
        if !settings.tray_hint_shown {
            let mut updated_settings = settings.clone();
            updated_settings.tray_hint_shown = true;
            if let Err(e) = store.save(&updated_settings) {
                dsh_marina::applog::warn(&format!("保存设置失败：{e}"));
            }
            // MessageBox blocks; keep it off the UI thread.
            cx.background_executor()
                .spawn(async { platform::show_tray_hint() })
                .detach();
        }
    }
    false
}

fn create_tray(cx: &mut App) -> anyhow::Result<()> {
    dsh_marina::applog::info("开始创建托盘图标…");

    let icon =
        tray_icon(TrayIconState::Idle).ok_or_else(|| anyhow::anyhow!("生成托盘图标图像失败"))?;

    dsh_marina::applog::info("托盘图标图像已生成，开始构建托盘与菜单…");

    let tray = Tray::builder()
        .icon(icon)
        .tooltip(APP_TITLE)
        .menu(build_tray_menu)
        .build(cx)?;

    dsh_marina::applog::info("托盘已构建，正在注册全局…");
    cx.set_global(MarinaTray { tray });
    dsh_marina::applog::info("托盘已就绪");
    Ok(())
}

fn command_item(
    label: impl Into<SharedString>,
    op: TrayOp,
    id: impl Into<String>,
    enabled: bool,
) -> MenuItem {
    MenuItem::action(label, TrayCommand { op, id: id.into() }).disabled(!enabled)
}

fn build_tray_menu(cx: &mut App) -> Vec<MenuItem> {
    let entries = cx
        .try_global::<MarinaApp>()
        .map(|app| app.tray_menu.clone())
        .unwrap_or_default();
    let mut menu = vec![
        command_item("打开 DSH Marina", TrayOp::Open, String::new(), true),
        MenuItem::separator(),
    ];
    for entry in entries {
        let running = matches!(
            entry.status,
            InstanceStatus::Running | InstanceStatus::AdoptedRunning
        );
        let busy = matches!(
            entry.status,
            InstanceStatus::Starting | InstanceStatus::Stopping
        );
        let mut items = vec![
            command_item("显示实例窗口", TrayOp::Focus, entry.id.clone(), running),
            command_item("启动", TrayOp::Start, entry.id.clone(), !running && !busy),
            command_item("停止", TrayOp::Stop, entry.id.clone(), running),
            command_item("重启", TrayOp::Restart, entry.id.clone(), running),
        ];
        if entry.status == InstanceStatus::Crashed {
            items.extend([
                MenuItem::separator(),
                command_item("查看崩溃信息", TrayOp::ViewCrash, entry.id.clone(), true),
                command_item(
                    "回滚到上次正常状态",
                    TrayOp::Rollback,
                    entry.id.clone(),
                    true,
                ),
            ]);
        }
        items.extend([
            MenuItem::separator(),
            command_item("在 Marina 中查看", TrayOp::Open, entry.id.clone(), true),
        ]);
        menu.push(MenuItem::submenu(
            Menu::new(tray_label(&entry)).items(items),
        ));
    }
    menu.extend([
        MenuItem::separator(),
        command_item("新建实例…", TrayOp::NewInstance, String::new(), true),
        command_item("打开 dsh:// 链接…", TrayOp::OpenLink, String::new(), true),
        command_item("设置…", TrayOp::Settings, String::new(), true),
        command_item("退出", TrayOp::Quit, String::new(), true),
    ]);
    menu
}

fn tray_label(entry: &TrayEntry) -> String {
    let status = match entry.status {
        InstanceStatus::Running => "运行中",
        InstanceStatus::AdoptedRunning => "运行中（已接管）",
        InstanceStatus::Starting => "启动中…",
        InstanceStatus::Stopping => "停止中…",
        InstanceStatus::Crashed => "启动失败",
        InstanceStatus::Stopped => "已停止",
    };
    format!("{}  ·  {status}", entry.name)
}

fn handle_tray_command(command: &TrayCommand, cx: &mut App) {
    if command.op == TrayOp::Quit {
        request_quit(cx);
        return;
    }
    if command.op == TrayOp::Open && command.id.is_empty() {
        show_main_window(&ShowWindow, cx);
        return;
    }
    let Some(app) = cx.try_global::<MarinaApp>() else {
        return;
    };
    let (hwnd, window) = (app.hwnd, app.window.clone().and_then(|w| w.upgrade()));
    let Some(window) = window else {
        return;
    };
    let shows_window = !matches!(
        command.op,
        TrayOp::Focus | TrayOp::Start | TrayOp::Stop | TrayOp::Restart
    );
    if shows_window && let Some(hwnd) = hwnd {
        platform::show_window(hwnd);
    }
    let command = command.clone();
    let main_window = cx.windows().into_iter().next();
    window.update(cx, |this, cx| match command.op {
        TrayOp::Focus => this.focus_instance(&command.id, cx),
        TrayOp::Start => this.start_instance(command.id, false, cx),
        TrayOp::Stop => this.request_stop(command.id, false, cx),
        TrayOp::Restart => this.request_stop(command.id, true, cx),
        TrayOp::Open | TrayOp::ViewCrash => this.select_instance(command.id, cx),
        TrayOp::Rollback => {
            this.select_instance(command.id.clone(), cx);
            this.open_rollback_dialog(command.id, cx);
        }
        TrayOp::Settings => {
            this.active_view = View::GlobalSettings;
            cx.notify();
        }
        TrayOp::NewInstance | TrayOp::OpenLink | TrayOp::Quit => {}
    });
    if matches!(command.op, TrayOp::NewInstance | TrayOp::OpenLink)
        && let Some(handle) = main_window
    {
        let open = move |this: &mut MarinaWindow,
                         window_ref: &mut Window,
                         cx: &mut Context<MarinaWindow>| {
            if command.op == TrayOp::NewInstance {
                this.open_new_dialog(window_ref, cx);
            } else {
                this.open_link_dialog(window_ref, cx);
            }
        };
        // The root view may be MarinaWindow itself; updating it again inside
        // `handle.update` would double-borrow it.
        if let Some(typed) = handle.downcast::<MarinaWindow>() {
            let _ = typed.update(cx, |this, window_ref, cx| open(this, window_ref, cx));
        } else {
            let _ = handle.update(cx, |_, window_ref, cx| {
                window.update(cx, |this, cx| open(this, window_ref, cx));
            });
        }
    }
}

fn raw_hwnd(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Initial content of a new instance (design §7.2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewMode {
    Blank,
    Clone,
    /// From an instance package (`.zip`) exported by Marina, e.g. on
    /// another computer.
    Import,
}

/// A DSH version offered in the new-instance dialog: installed, offered by
/// the download source, or both.
#[derive(Clone)]
struct VersionChoice {
    version: String,
    installed: bool,
    remote: Option<download::RemoteVersion>,
}

/// State of the "新建实例" dialog.
struct NewInstanceDialog {
    name: Entity<InputState>,
    /// Versions for a blank instance, newest first.
    versions: Vec<VersionChoice>,
    /// Chosen version; `None` follows the newest stable one.
    selected: Option<String>,
    /// Address of the version list (the download source in settings).
    feed_url: String,
    listing: bool,
    list_error: Option<String>,
    /// Why the list is incomplete (e.g. GitHub unreachable).
    list_note: Option<String>,
    /// Stable versions were asked for but the source has only prereleases.
    only_prerelease: bool,
    error: Option<String>,
    mode: NewMode,
    /// Copy sessions and credentials too.
    personal: bool,
    clone_source: Option<String>,
    /// Chosen instance package and what it contains.
    import_file: Option<(PathBuf, Result<packs::PackManifest, String>)>,
    busy: bool,
}

/// State of the "更换运行时" dialog.
struct SwitchRuntimeDialog {
    instance_id: String,
    /// Binding the instance uses right now, so re-selecting it disables submit.
    current: String,
    /// Versions, newest first; installed ones carry no remote entry.
    versions: Vec<VersionChoice>,
    selected: Option<String>,
    listing: bool,
    list_error: Option<String>,
    list_note: Option<String>,
    only_prerelease: bool,
    busy: bool,
    /// Plugin names the market index declares incompatible with `selected`.
    incompatible: Vec<String>,
}

/// Dialog variants for various operations.
enum Dialog {
    NewInstance(NewInstanceDialog),
    SwitchRuntime(SwitchRuntimeDialog),
    RestoreSnapshot {
        instance_id: String,
        snapshot_id: String,
    },
    DeleteSnapshot {
        instance_id: String,
        snapshot_id: String,
    },
    CreateSnapshot {
        instance_id: String,
        reason: Entity<InputState>,
    },
    RenameInstance {
        instance_id: String,
        name: Entity<InputState>,
    },
    /// FR-1: badge color, icon and notes.
    Appearance {
        instance_id: String,
        color: String,
        icon: Entity<InputState>,
        notes: Entity<InputState>,
        error: Option<String>,
    },
    DeleteInstance {
        instance_id: String,
        mode: storage::DeleteMode,
    },
    CloneInstance {
        instance_id: String,
        name: Entity<InputState>,
        personal: bool,
    },
    /// FR-12: the runtime crosses a major version since the last successful run.
    Compat {
        instance_id: String,
        previous: String,
        next: String,
    },
    UninstallPlugin {
        instance_id: String,
        package: String,
    },
    CopyPlugins {
        source_id: String,
        target: Option<String>,
    },
    /// 插件市场: pick the instance to install `plugins[index]` into.
    MarketInstall {
        index: usize,
        target: Option<String>,
    },
    /// Quit while instances are running (§FR-2).
    Exit {
        running: usize,
    },
    UninstallMarina {
        mode: maintenance::UninstallMode,
        stats: Option<storage::UninstallStats>,
    },
    /// 分享整合包对话框
    SharePack {
        /// 选中的实例ID
        instance: Option<String>,
        /// 整合包名称输入
        name: Entity<InputState>,
        /// 整合包描述输入
        description: Entity<InputState>,
        /// 导出到本地，或提交到社区
        mode: ShareMode,
        /// 选择的保存文件夹路径（仅本地模式）
        selected_folder: Option<PathBuf>,
        /// 错误信息
        error: Option<String>,
        /// 是否正在处理
        busy: bool,
    },
    /// 整合包社区设置：GitHub 账号与目录来源。
    CommunitySettings,
    /// 整合包详情：列出包含的插件。
    PackPlugins {
        name: String,
        author: String,
        description: String,
        dsh_version: String,
        plugins: Vec<String>,
        size: u64,
    },
    /// GitHub 登录对话框
    GitHubLogin {
        /// Device Flow 用户代码（空表示还未开始）
        user_code: String,
        /// 验证 URL
        verification_uri: String,
        /// 是否正在等待授权
        waiting: bool,
        /// 错误信息
        error: Option<String>,
        /// 是否正在初始化连接
        connecting: bool,
    },
    /// 分享到社区进度对话框
    ShareToCommunity {
        /// 整合包名称
        pack_name: String,
        /// 当前步骤
        current_step: usize,
        /// 总步骤数
        total_steps: usize,
        /// 步骤描述
        steps: Vec<(String, ShareStepStatus)>,
        /// 进度百分比 (0-100)
        progress: u8,
        /// 用户是否点了「取消」；步骤之间检查，当前请求结束后停下
        cancel: Arc<std::sync::atomic::AtomicBool>,
        /// PR URL (成功后)
        pr_url: Option<String>,
        /// 错误信息
        error: Option<String>,
    },
    UninstallExternal {
        delete_data: bool,
    },
    DeleteRuntime {
        runtime_id: String,
        version: String,
    },
    /// §7.8: the runtime is still in use; pick where its instances go first.
    RebindRuntime {
        runtime_id: String,
        version: String,
        /// Instance names that use it.
        users: Vec<String>,
        /// `(binding, label)` choices.
        targets: Vec<(String, String)>,
        selected: usize,
    },
    /// FR-5 浜屽垎鎺掓煡 in progress; `all` is every plugin enabled at the start.
    Bisect {
        instance_id: String,
        all: Vec<String>,
        bisect: dsh_marina::bisect::Bisect,
        busy: bool,
    },
    /// FR-9: over the version limit; `(runtime id, version)` of unused old ones.
    CleanupRuntimes {
        versions: Vec<(String, String)>,
        max: u32,
    },
    /// FR-7: the pack would contain tokens or local paths.
    ExportScan {
        instance_id: String,
        findings: Vec<dsh_marina::sensitive::SensitiveFinding>,
    },
    /// §7.5: stopping may lose unsaved work in the DSH window.
    StopInstance {
        instance_id: String,
        restart: bool,
        dont_ask: bool,
    },
    /// Move the data root to `target` and restart (design §8.1).
    MoveData {
        target: PathBuf,
    },
    /// Choose the instance that opens a `dsh://` link (design §14).  `input`
    /// is set when the user types the link; otherwise `url` came from Windows.
    OpenLink {
        url: String,
        input: Option<Entity<InputState>>,
        selected: Option<String>,
        remember: bool,
        error: Option<String>,
    },
}

#[derive(Clone, PartialEq)]
enum ShareStepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// 分享整合包的两种去处。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShareMode {
    /// 生成 .zip 和目录条目，放进用户选的文件夹。
    Local,
    /// 提交到社区仓库；通过自动校验后由维护者合并。
    Community,
}

/// 去登录 GitHub 前记下的分享表单，登录成功后原样放回去。
#[derive(Clone)]
struct ShareDraft {
    instance: Option<String>,
    mode: ShareMode,
    folder: Option<PathBuf>,
    name: String,
    description: String,
}

/// GitHub 认证信息
#[derive(Clone)]
struct GitHubAuth {
    /// GitHub access token
    token: String,
    /// GitHub 用户名
    username: String,
    /// 用户头像 URL
    #[allow(dead_code)]
    avatar_url: Option<String>,
}

/// "DSH 版本与下载" (§7.1, §FR-9) and the titlebar download list.
#[derive(Default)]
struct DownloadState {
    checking: bool,
    lookup: Option<download::FeedLookup>,
    error: Option<String>,
    installing: bool,
    /// DSH versions and 整合包 being downloaded.
    jobs: Vec<DownloadJob>,
    /// The titlebar download list is open.
    panel_open: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum JobPhase {
    Downloading,
    Installing,
}

/// 「关于」里的检查更新：Marina 自身的最新版本。
#[derive(Default)]
struct AppUpdateState {
    checking: bool,
    /// 服务端最新的正式版；已经查过但这里是 `None`，说明还没有发布过正式版。
    latest: Option<github::LatestRelease>,
    checked: bool,
    error: Option<String>,
}

/// One entry of the download list.
struct DownloadJob {
    /// `dsh:<version>` or `pack:<id>`.
    key: String,
    title: String,
    /// `(downloaded, total)`.
    progress: Arc<Mutex<(u64, u64)>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    phase: JobPhase,
    /// Recent `(time, downloaded)` samples for speed and ETA (§NFR-9).
    samples: std::cell::RefCell<std::collections::VecDeque<(std::time::Instant, u64)>>,
}

impl DownloadJob {
    fn amounts(&self) -> (u64, u64) {
        self.progress.lock().map(|p| *p).unwrap_or((0, 0))
    }
    /// Average bytes/second over the last ~5 seconds of samples.
    fn speed(&self, downloaded: u64) -> Option<f64> {
        let now = std::time::Instant::now();
        let mut samples = self.samples.borrow_mut();
        if samples.back().is_none_or(|(_, bytes)| *bytes != downloaded) {
            samples.push_back((now, downloaded));
        }
        while samples.len() > 2
            && samples
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 5.0)
        {
            samples.pop_front();
        }
        let (first_time, first_bytes) = *samples.front()?;
        let elapsed = now.duration_since(first_time).as_secs_f64();
        (elapsed >= 0.5 && downloaded > first_bytes)
            .then(|| (downloaded - first_bytes) as f64 / elapsed)
    }
}

/// Main window model with reactive state.
struct MarinaWindow {
    state: Arc<AppState>,
    instances: Vec<InstanceConfig>,
    /// Last known process status per instance; refreshed without blocking the UI.
    statuses: HashMap<String, InstanceStatus>,
    /// Tracked by the root element, so Escape reaches every dialog.
    focus: FocusHandle,
    /// Starting/Stopping overrides while a background start/stop runs.
    pending: HashMap<String, InstanceStatus>,
    /// Instances whose `cordis.patch.yml` is being rewritten right now.
    plugin_busy: std::collections::HashSet<String>,
    selected_instance: Option<String>,
    active_tab: DetailsTab,
    active_view: View,
    last_error: Option<String>,
    last_success: Option<String>,
    dialog: Option<Dialog>,
    tray_state: TrayIconState,
    /// Memory of each running instance's process tree, from the monitor.
    memory: HashMap<String, u64>,
    /// Disk usage of each instance directory, refreshed about once a minute.
    disk: HashMap<String, u64>,
    process_ids: HashMap<String, u32>,
    runtime_disk: HashMap<String, u64>,
    download: DownloadState,
    /// Marina 自身的更新检查结果（「关于」里）。
    app_update: AppUpdateState,
    /// A long copy (clone / import / data move) shown as a progress overlay.
    copy_job: Option<CopyJob>,
    /// Scroll position of the virtualized log panel.
    log_scroll: UniformListScrollHandle,
    /// Line count last shown, to follow new output only.
    log_seen: std::cell::Cell<usize>,
    packs: PacksState,
    market: MarketState,
    /// "自定义下载链接" field in settings.
    custom_link_input: Entity<InputState>,
    notices: NoticeCenter,
    /// GitHub 认证状态
    github_auth: Option<GitHubAuth>,
    /// 从分享对话框去登录 GitHub 时的表单，登录成功后恢复。
    share_draft: Option<ShareDraft>,
}

/// Lucide icons Marina uses beyond gpui-kit's default bundle, then the bundle.
struct MarinaAssets;

impl AssetSource for MarinaAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let extra: Option<&'static [u8]> = match path {
            "icons/download.svg" => Some(include_bytes!("../assets/icons/download.svg")),
            "icons/puzzle.svg" => Some(include_bytes!("../assets/icons/puzzle.svg")),
            _ => None,
        };
        match extra {
            Some(bytes) => Ok(Some(std::borrow::Cow::Borrowed(bytes))),
            None => assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        assets::Assets.list(path)
    }
}

/// Every toast also lands in the bell list; toasts hide after a few seconds.
#[derive(Default)]
struct NoticeCenter {
    /// `(error, text, time)`, newest last.
    history: Vec<(bool, String, chrono::DateTime<chrono::Local>)>,
    unread: usize,
    panel_open: bool,
    /// Toasts last recorded, to notice new ones.
    seen: (Option<String>, Option<String>),
    /// Bumped per toast so an older timer does not hide a newer one.
    generation: (u64, u64),
}

/// Progress overlay for a cancellable copy (design §FR-1 边界).
struct CopyJob {
    title: String,
    progress: Arc<copy::CopyProgress>,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    /// Dashboard: counts, quick actions and every instance at a glance.
    Home,
    /// 插件市场: the awesome-dsh-plugin community list.
    Market,
    /// Shared plugin packs (整合包): browse, download, reuse, share.
    Packs,
    InstanceDetails,
    GlobalSettings,
}

/// State of the 整合包 page.
#[derive(Default)]
struct PacksState {
    /// Last catalog fetch; `None` before the first one.
    catalog: Option<Result<Vec<dsh_marina::catalog::CatalogEntry>, String>>,
    loading: bool,
    url_input: Option<Entity<InputState>>,
    /// `(entry id, (done, total), cancel)` of the running download.
    downloading: Option<(
        String,
        Arc<Mutex<(u64, u64)>>,
        Arc<std::sync::atomic::AtomicBool>,
    )>,
    /// Pack being turned into an instance.
    importing: Option<String>,
    /// 已下载且校验通过的整合包 ID，列表渲染直接查这里。
    downloaded: std::collections::HashSet<String>,
    /// Search input field for filtering packs.
    search: Option<Entity<InputState>>,
    /// 全部 / 已下载。
    filter: PacksFilter,
    /// 登录是从分享对话框发起的，成功后回到分享表单；否则回到社区设置。
    login_from_share: bool,
}

/// 整合包列表的筛选。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PacksFilter {
    #[default]
    All,
    Downloaded,
}

/// 插件市场 page.
#[derive(Default)]
struct MarketState {
    index: Option<Arc<dsh_marina::market::MarketIndex>>,
    loading: bool,
    error: Option<String>,
    search: Option<Entity<InputState>>,
    _search_changed: Option<Subscription>,
    category: Option<String>,
    sort: dsh_marina::market::MarketSort,
    page: usize,
    /// Matches for the current filter, recomputed when it changes.
    hits: Arc<Vec<usize>>,
    /// Instance id → installed plugin package names.
    installed: HashMap<String, std::collections::HashSet<String>>,
    /// `(instance id, package)` being installed.
    installing: Vec<(String, String)>,
    /// Stars, downloads and capabilities, read after the index.
    stats: Option<Arc<dsh_marina::market::StatsTable>>,
    stats_loading: bool,
    /// No source answered and nothing was cached.
    stats_failed: bool,
    /// Plugin name → (visible width, content width) of its capability row,
    /// measured while painting; decides whether the row scrolls.
    chip_widths: std::rc::Rc<std::cell::RefCell<HashMap<String, (f32, f32)>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum DetailsTab {
    Overview,
    Plugins,
    Snapshots,
    Logs,
    Settings,
}

impl MarinaWindow {
    fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let custom_url = state
            .settings_store
            .load()
            .ok()
            .and_then(|s| {
                let options = maintenance::feed_options(&s);
                options.feed_url(options.preferred)
            })
            .unwrap_or_default();
        let custom_link_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://…latest.yml，或放安装包和 latest.yml 的目录地址")
                .default_value(custom_url)
        });
        let instances = state.repository.list().unwrap_or_else(|e| {
            dsh_marina::applog::warn(&format!("读取实例列表失败：{e}"));
            Vec::new()
        });

        // Adopt leftover DSH processes and clean the trash, then monitor every
        // 2 seconds; the loop stops once the window entity is dropped.
        let monitor_state = state.clone();
        cx.spawn(async move |this, cx| {
            {
                let state = monitor_state.clone();
                cx.background_executor()
                    .spawn(async move { lifecycle::startup(&state) })
                    .await;
            }
            loop {
                let Ok(ids) = this.update(cx, |this, _| {
                    this.instances
                        .iter()
                        .map(|i| i.id.clone())
                        .collect::<Vec<_>>()
                }) else {
                    break;
                };
                let state = monitor_state.clone();
                let incoming = cx
                    .background_executor()
                    .spawn(async move {
                        lifecycle::tick(&state, &ids);
                        poll_links(&state)
                    })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    for url in incoming {
                        this.receive_link(url, cx);
                    }
                    for (error, text) in lifecycle::take_notices(&this.state) {
                        if error {
                            this.last_error = Some(text);
                        } else {
                            this.last_success = Some(text);
                        }
                    }
                    this.refresh_instances(cx);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();

        let focus = cx.focus_handle();
        // 根元素持有键盘焦点，Escape 等按键才能沿元素树冒泡上来。
        window.focus(&focus, cx);
        let mut window = Self {
            state: state.clone(),
            instances,
            statuses: HashMap::new(),
            focus,
            pending: HashMap::new(),
            plugin_busy: std::collections::HashSet::new(),
            selected_instance: None,
            active_tab: DetailsTab::Overview,
            active_view: View::Home,
            last_error: None,
            last_success: None,
            dialog: None,
            tray_state: TrayIconState::Idle,
            memory: HashMap::new(),
            disk: HashMap::new(),
            process_ids: HashMap::new(),
            runtime_disk: HashMap::new(),
            download: DownloadState::default(),
            app_update: AppUpdateState::default(),
            copy_job: None,
            log_scroll: UniformListScrollHandle::new(),
            log_seen: std::cell::Cell::new(0),
            packs: PacksState::default(),
            market: MarketState::default(),
            custom_link_input,
            notices: NoticeCenter::default(),
            github_auth: github::load_token(&state.data_root).map(|(token, user)| GitHubAuth {
                token,
                username: user.login,
                avatar_url: Some(user.avatar_url),
            }),
            share_draft: None,
        };
        window.refresh_statuses();
        window.maybe_check_feed_daily(cx);
        window.bind_link_enter(cx);
        window
    }

    fn refresh_instances(&mut self, cx: &mut Context<Self>) {
        self.instances = self.state.repository.list().unwrap_or_else(|e| {
            dsh_marina::applog::warn(&format!("刷新实例列表失败：{e}"));
            Vec::new()
        });
        self.refresh_statuses();
        self.update_tray_icon(cx);
    }

    /// Statuses come from the background monitor's last tick, so this never
    /// touches the process lock (a stop may hold it for the grace period).
    fn refresh_statuses(&mut self) {
        let lifecycle = self
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.statuses = self
            .instances
            .iter()
            .map(|i| {
                let status = if lifecycle.rolling_back.contains(&i.id) {
                    InstanceStatus::Starting
                } else {
                    match lifecycle.running.get(&i.id) {
                        Some(lifecycle::ProcState::Running) => InstanceStatus::Running,
                        Some(lifecycle::ProcState::Adopted) => InstanceStatus::AdoptedRunning,
                        None if lifecycle.crashed.contains(&i.id) => InstanceStatus::Crashed,
                        None => InstanceStatus::Stopped,
                    }
                };
                (i.id.clone(), status)
            })
            .collect();
        self.memory = lifecycle.memory.clone();
        self.disk = lifecycle.disk.clone();
        self.process_ids = lifecycle.process_ids.clone();
        self.runtime_disk = lifecycle.runtime_disk.clone();
    }

    fn update_tray_icon(&mut self, cx: &mut Context<Self>) {
        let statuses: Vec<InstanceStatus> = self
            .instances
            .iter()
            .map(|i| self.get_instance_status(&i.id))
            .collect();
        let next = if statuses.contains(&InstanceStatus::Crashed) {
            TrayIconState::Crashed
        } else if statuses
            .iter()
            .any(|s| matches!(s, InstanceStatus::Running | InstanceStatus::AdoptedRunning))
        {
            TrayIconState::Running
        } else {
            TrayIconState::Idle
        };
        let entries: Vec<TrayEntry> = self
            .instances
            .iter()
            .map(|i| TrayEntry {
                id: i.id.clone(),
                name: i.name.clone(),
                status: self.get_instance_status(&i.id),
            })
            .collect();
        let menu_changed = cx
            .try_global::<MarinaApp>()
            .is_some_and(|app| app.tray_menu != entries);
        if menu_changed {
            cx.global_mut::<MarinaApp>().tray_menu = entries;
            if let Some(tray) = cx.try_global::<MarinaTray>().map(|tray| tray.tray.clone())
                && let Err(error) = tray.refresh_menu(cx)
            {
                dsh_marina::applog::warn(&format!("刷新托盘菜单失败：{error}"));
            }
        }
        if next == self.tray_state {
            return;
        }
        self.tray_state = next;
        if let (Some(tray), Some(icon)) = (
            cx.try_global::<MarinaTray>().map(|t| t.tray.clone()),
            tray_icon(next),
        ) && let Err(e) = tray.set_icon(icon, cx)
        {
            dsh_marina::applog::warn(&format!("更新托盘图标失败：{e}"));
        }
    }

    fn select_instance(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected_instance = Some(id);
        self.active_tab = DetailsTab::Overview;
        self.active_view = View::InstanceDetails;
        cx.notify();
    }

    /// Start an instance.  Unless `force` is set, a major version change since
    /// the last successful run asks for confirmation first (§FR-12).
    fn start_instance(&mut self, id: String, force: bool, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let id_clone = id.clone();
        self.pending.insert(id.clone(), InstanceStatus::Starting);
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !force
                        && let Some((previous, next)) = lifecycle::compat_warning(&state, &id_clone)
                    {
                        return Ok(Some((previous, next)));
                    }
                    lifecycle::launch(&state, &id_clone).map(|_| None)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.pending.remove(&id);
                match result {
                    Err(e) => this.last_error = Some(format!("启动实例失败：{e}")),
                    Ok(Some((previous, next))) => {
                        this.dialog = Some(Dialog::Compat {
                            instance_id: id.clone(),
                            previous,
                            next,
                        });
                    }
                    Ok(None) => {}
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// FR-12 "复制后再启动": clone, then start the copy.
    fn clone_and_start(&mut self, id: String, cx: &mut Context<Self>) {
        let name = format!("{} 副本", self.instance_name_of(&id));
        self.run_copy_job(
            "正在复制实例".to_owned(),
            move |state, progress| {
                state
                    .repository
                    .clone_instance_with_progress(&id, &name, false, progress)
                    .map_err(|e| e.to_string())
            },
            |this, result, cx| match result {
                Err(e) => this.last_error = Some(format!("复制实例失败：{e}")),
                Ok(config) => {
                    this.last_success = Some(format!("已复制为 {}，正在启动", config.name));
                    this.refresh_instances(cx);
                    this.select_instance(config.id.clone(), cx);
                    this.start_instance(config.id, true, cx);
                }
            },
            cx,
        );
    }

    /// Run a cancellable copy on a background thread behind the progress
    /// overlay.  `done` is skipped when the user cancelled; the copy has
    /// already removed its partial output by then.
    fn run_copy_job<T: Send + 'static>(
        &mut self,
        title: String,
        work: impl FnOnce(&Arc<AppState>, &copy::CopyProgress) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.copy_job.is_some() {
            self.last_error = Some("已有一个复制任务在进行中".to_owned());
            cx.notify();
            return;
        }
        let progress = copy::CopyProgress::new();
        self.copy_job = Some(CopyJob {
            title,
            progress: progress.clone(),
        });
        self.last_error = None;
        cx.notify();

        let state = self.state.clone();
        let worker_progress = progress.clone();
        let task = cx
            .background_executor()
            .spawn(async move { work(&state, &worker_progress) });

        // Repaint the bar while the copy runs.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    cx.notify();
                    this.copy_job.is_some()
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.copy_job = None;
                lifecycle::invalidate_disk_usage(&this.state);
                if progress.is_cancelled() && result.is_err() {
                    this.last_success = Some("已取消复制，未完成的文件已清理".to_owned());
                    this.refresh_instances(cx);
                } else {
                    done(this, result, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_copy_job(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.copy_job {
            job.progress.cancel();
            cx.notify();
        }
    }

    fn instance_name_of(&self, id: &str) -> String {
        self.instances
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Run blocking work on a background thread and report the result as a
    /// toast.  `pending` shows a transient status on the instance meanwhile.
    fn run_task(
        &mut self,
        pending: Option<(String, InstanceStatus)>,
        work: impl FnOnce(&Arc<AppState>) -> Result<String, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let pending_id = pending.as_ref().map(|(id, _)| id.clone());
        if let Some((id, status)) = pending {
            self.pending.insert(id, status);
        }
        self.last_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { work(&state) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(id) = pending_id {
                    this.pending.remove(&id);
                }
                match result {
                    Ok(message) if !message.is_empty() => this.last_success = Some(message),
                    Ok(_) => {}
                    Err(e) => this.last_error = Some(e),
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn restart_instance(&mut self, id: String, cx: &mut Context<Self>) {
        let task_id = id.clone();
        self.run_task(
            Some((id, InstanceStatus::Starting)),
            move |state| {
                lifecycle::restart(state, &task_id, STOP_GRACE)
                    .map(|_| String::new())
                    .map_err(|e| format!("重启实例失败：{e}"))
            },
            cx,
        );
    }

    /// Bring the instance's DSH window to the front.
    fn focus_instance(&mut self, id: &str, cx: &mut Context<Self>) {
        let focused = self
            .process_ids
            .get(id)
            .is_some_and(|pid| dsh_marina::process::focus_process_window(*pid));
        if !focused {
            self.last_error = Some("找不到实例窗口（可能已收到托盘里）".to_owned());
            cx.notify();
        }
    }

    fn enter_safe_mode(&mut self, id: String, cx: &mut Context<Self>) {
        self.run_task(
            None,
            move |state| {
                lifecycle::safe_mode(state, &id)
                    .map(|count| {
                        format!("已开启安全模式：禁用了 {count} 个插件，原配置已另存为快照")
                    })
                    .map_err(|e| format!("开启安全模式失败：{e}"))
            },
            cx,
        );
    }

    fn uninstall_plugin(&mut self, id: String, package: String, cx: &mut Context<Self>) {
        self.last_success = Some(format!("正在卸载 {package}…"));
        self.run_task(
            Some((id.clone(), InstanceStatus::Stopping)),
            move |state| {
                lifecycle::uninstall_plugin(state, &id, &package)
                    .map(|_| format!("已卸载 {package}"))
            },
            cx,
        );
    }

    fn copy_plugins(&mut self, source: String, target: String, cx: &mut Context<Self>) {
        let target_name = self.instance_name_of(&target);
        self.last_success = Some("正在复制插件配置并安装依赖…".to_owned());
        self.run_task(
            Some((target.clone(), InstanceStatus::Stopping)),
            move |state| match lifecycle::copy_plugins(state, &source, &target)? {
                None => Ok(format!("已把插件配置复制到 {target_name}。")),
                Some(warning) => Ok(format!(
                    "已把插件配置复制到 {target_name}，但依赖安装失败；DSH 下次启动时会再试：{warning}"
                )),
            },
            cx,
        );
    }

    /// Stop or restart, asking first unless the user turned the warning off.
    fn request_stop(&mut self, id: String, restart: bool, cx: &mut Context<Self>) {
        let confirm = self
            .state
            .settings_store
            .load()
            .map(|s| s.confirm_stop)
            .unwrap_or(true);
        if !confirm {
            self.confirm_stop(id, restart, false, cx);
            return;
        }
        if let Some(hwnd) = cx.try_global::<MarinaApp>().and_then(|a| a.hwnd) {
            platform::show_window(hwnd);
        }
        self.dialog = Some(Dialog::StopInstance {
            instance_id: id,
            restart,
            dont_ask: false,
        });
        cx.notify();
    }

    fn confirm_stop(&mut self, id: String, restart: bool, force: bool, cx: &mut Context<Self>) {
        if restart {
            self.restart_instance(id, cx);
        } else {
            self.stop_instance(id, if force { Duration::ZERO } else { STOP_GRACE }, cx);
        }
    }

    fn stop_instance(&mut self, id: String, grace: Duration, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let id_clone = id.clone();
        self.pending.insert(id.clone(), InstanceStatus::Stopping);
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lifecycle::stop(&state, &id_clone, grace) })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.pending.remove(&id);
                if let Err(e) = result {
                    this.last_error = Some(format!("停止实例失败：{e}"));
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn restore_snapshot(&mut self, instance_id: &str, snapshot_id: &str, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        lifecycle::restore_and_install(&state, &instance_id, &snapshot_id)
                    })
                    .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(None) => {
                        this.last_success = Some("已回滚，回滚前的配置已另存为安全快照".to_owned());
                    }
                    Ok(Some(warning)) => {
                        this.last_success = Some(format!(
                            "已回滚插件配置，但依赖重装失败，首次启动时 DSH 会自行安装：{warning}"
                        ));
                    }
                    Err(e) => this.last_error = Some(e),
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn mark_snapshot_last_good(
        &mut self,
        instance_id: &str,
        snapshot_id: &str,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paths = state
                        .repository
                        .paths(&instance_id)
                        .map_err(|e| e.to_string())?;
                    let store = SnapshotStore::new(&paths.root);
                    store
                        .mark_last_good(&snapshot_id)
                        .map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.last_error = Some(format!("标记快照为「上次正常」失败：{e}"));
                } else {
                    this.last_success = Some("已标记为「上次正常」快照".to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn create_snapshot(&mut self, instance_id: &str, reason: &str, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        let reason = reason.to_string();
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paths = state
                        .repository
                        .paths(&instance_id)
                        .map_err(|e| e.to_string())?;
                    let store = SnapshotStore::new(&paths.root);
                    store.create(&reason).map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.last_error = Some(format!("创建快照失败：{e}"));
                } else {
                    this.last_success = Some("快照已创建".to_string());
                    dsh_marina::applog::info("快照已创建");
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_snapshot(&mut self, instance_id: &str, snapshot_id: &str, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        let snapshot_id = snapshot_id.to_string();
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paths = state
                        .repository
                        .paths(&instance_id)
                        .map_err(|e| e.to_string())?;
                    let _store = SnapshotStore::new(&paths.root);
                    let snapshots_dir = paths.root.join("snapshots");
                    let snapshot_path = snapshots_dir.join(&snapshot_id);
                    if snapshot_path.exists() {
                        std::fs::remove_dir_all(&snapshot_path)
                            .map_err(|e| format!("删除快照目录失败：{e}"))?;
                    }
                    Ok::<(), String>(())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.last_error = Some(format!("删除快照失败：{e}"));
                } else {
                    this.last_success = Some("快照已删除".to_string());
                    dsh_marina::applog::info("快照已删除");
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rename_instance(&mut self, instance_id: &str, new_name: &str, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        let new_name = new_name.to_string();
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    state
                        .repository
                        .rename(&instance_id, &new_name)
                        .map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.last_error = Some(format!("重命名实例失败：{e}"));
                } else {
                    this.last_success = Some("实例已重命名".to_string());
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_instance(
        &mut self,
        instance_id: &str,
        mode: storage::DeleteMode,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let instance_id_str = instance_id.to_string();
        self.last_error = None;

        // Clear selection if deleting the selected instance
        if self.selected_instance.as_deref() == Some(instance_id) {
            self.selected_instance = None;
        }

        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paths = state
                        .repository
                        .paths(&instance_id_str)
                        .map_err(|e| e.to_string())?;
                    storage::delete_instance(&state.repository, &paths, mode)
                        .map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(e) => this.last_error = Some(format!("删除实例失败：{e}")),
                    Ok(outcome) => {
                        let mut message = "实例已删除".to_string();
                        if let Some(path) = outcome.exported_to {
                            message.push_str(&format!("；备份已导出到 {}", path.display()));
                        }
                        if let Some(path) = outcome.kept_at {
                            message.push_str(&format!("; data retained at {}", path.display()));
                        }
                        this.last_success = Some(message);
                    }
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn share_instance_to_folder(
        &mut self,
        instance_id: String,
        pack_name: String,
        pack_description: String,
        base_folder: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let pack_slug = catalog::slug(&pack_name); // 提前计算

        self.dialog = None; // 关闭对话框
        self.last_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    use std::fs;

                    // 创建以整合包名称命名的文件夹
                    let pack_folder = base_folder.join(&pack_slug);
                    fs::create_dir_all(&pack_folder).map_err(|e| format!("创建文件夹失败：{e}"))?;

                    // 生成 .zip 文件
                    let pack_file = pack_folder.join(format!("{pack_slug}.zip"));
                    dsh_marina::packs::export_instance_redacted(
                        &state.repository,
                        &instance_id,
                        &pack_file,
                    )
                    .map_err(|e| format!("导出整合包失败：{e}"))?;

                    // 生成 .catalog-entry.json 文件
                    let _config = state
                        .repository
                        .load(&instance_id)
                        .map_err(|e| format!("读取实例配置失败：{e}"))?;
                    let author =
                        std::env::var("USERNAME").unwrap_or_else(|_| "匿名用户".to_owned());

                    // 创建目录条目，使用用户填写的信息
                    let mut entry =
                        catalog::entry_for_file(&pack_file, &pack_slug, &author, &pack_description)
                            .map_err(|e| format!("生成目录条目失败：{e}"))?;

                    // 使用用户填写的名称和描述
                    entry.name = pack_name;
                    entry.description = pack_description;

                    let entry_file = pack_folder.join(format!("{pack_slug}.catalog-entry.json"));
                    fs::write(&entry_file, catalog::entry_json(&entry))
                        .map_err(|e| format!("保存目录条目失败：{e}"))?;

                    Ok::<_, String>((pack_folder, pack_slug.clone()))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((folder, slug)) => {
                        this.last_success = Some(format!(
                            "已导出 {slug}.zip 和目录条目到 {}",
                            folder.display()
                        ));
                    }
                    Err(e) => {
                        this.last_error = Some(format!("生成整合包失败：{e}"));
                    }
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn share_to_community(
        &mut self,
        instance_id: String,
        pack_name: String,
        pack_description: String,
        cx: &mut Context<Self>,
    ) {
        dsh_marina::applog::info(&format!(
            "[ShareToCommunity] 开始，实例: {}, 名称: {}",
            instance_id, pack_name
        ));

        let Some(auth) = self.github_auth.clone() else {
            dsh_marina::applog::warn("[ShareToCommunity] 错误：未登录 GitHub");
            self.last_error = Some("未登录 GitHub".to_string());
            self.dialog = None;
            cx.notify();
            return;
        };

        dsh_marina::applog::info(&format!(
            "[ShareToCommunity] GitHub 用户: {}",
            auth.username
        ));

        let state = self.state.clone();
        let pack_slug = catalog::slug(&pack_name);

        dsh_marina::applog::info(&format!("[ShareToCommunity] Slug: {}", pack_slug));

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.dialog = Some(Dialog::ShareToCommunity {
            pack_name: pack_name.clone(),
            current_step: 0,
            total_steps: 9,
            steps: vec![
                ("生成整合包文件".to_string(), ShareStepStatus::InProgress),
                (
                    "检查/创建存储仓库（公开）".to_string(),
                    ShareStepStatus::Pending,
                ),
                ("创建 GitHub Release".to_string(), ShareStepStatus::Pending),
                ("上传整合包到 Release".to_string(), ShareStepStatus::Pending),
                ("生成元数据文件".to_string(), ShareStepStatus::Pending),
                ("Fork 社区仓库".to_string(), ShareStepStatus::Pending),
                ("创建分支".to_string(), ShareStepStatus::Pending),
                ("上传元数据文件".to_string(), ShareStepStatus::Pending),
                ("创建 Pull Request".to_string(), ShareStepStatus::Pending),
            ],
            progress: 0,
            cancel: cancel.clone(),
            pr_url: None,
            error: None,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            // 把第 `index` 步标为完成并开始下一步，或者标为失败
            let advance = |this: &gpui_kit::WeakEntity<Self>, cx: &mut gpui_kit::AsyncApp, index: usize, result: Result<(), String>| {
                let ok = result.is_ok();
                this.update(cx, |this, cx| {
                    if let Some(Dialog::ShareToCommunity { ref mut steps, ref mut current_step, ref mut progress, ref mut error, .. }) = this.dialog {
                        match result {
                            Ok(()) => {
                                steps[index].1 = ShareStepStatus::Completed;
                                *current_step = index + 1;
                                *progress = ((index + 1) * 100 / steps.len()) as u8;
                                if let Some(next) = steps.get_mut(index + 1) {
                                    next.1 = ShareStepStatus::InProgress;
                                }
                            }
                            Err(e) => {
                                steps[index].1 = ShareStepStatus::Failed;
                                *error = Some(e);
                            }
                        }
                        cx.notify();
                    }
                })
                .ok();
                ok
            };

            // 用户点了「取消」：不再继续后面的步骤，当前请求结束后生效。
            let show_cancelled = |this: &gpui_kit::WeakEntity<Self>, cx: &mut gpui_kit::AsyncApp| {
                this.update(cx, |this, cx| {
                    if let Some(Dialog::ShareToCommunity { ref mut error, .. }) = this.dialog {
                        *error = Some("已取消分享".to_string());
                        cx.notify();
                    }
                })
                .ok();
            };

            // Step 1: 生成整合包文件（.zip），并读出清单信息
            let (entry_author, entry_name, entry_description) = (auth.username.clone(), pack_name.clone(), pack_description.clone());
            let slug = pack_slug.clone();
            let (zip_data, filename, mut entry, plugin_list, dsh_version, created_at) = match cx
                .background_executor()
                .spawn(async move {
                    let temp_dir = tempfile::tempdir().map_err(|e| format!("创建临时文件夹失败：{e}"))?;
                    let pack_file = temp_dir.path().join(format!("{slug}.zip"));
                    dsh_marina::packs::export_instance_redacted(&state.repository, &instance_id, &pack_file)
                        .map_err(|e| format!("导出整合包失败：{e}"))?;
                    let manifest = dsh_marina::packs::preview_pack(&pack_file)
                        .map_err(|e| format!("读取整合包清单失败：{e}"))?;
                    let mut entry = catalog::entry_for_file(&pack_file, &slug, &entry_author, &entry_description)
                        .map_err(|e| format!("生成目录条目失败：{e}"))?;
                    entry.name = entry_name;
                    let zip = std::fs::read(&pack_file).map_err(|e| format!("读取 zip 文件失败：{e}"))?;
                    let plugins = manifest
                        .plugins
                        .iter()
                        .map(|p| (p.name.clone(), p.version.clone(), p.enabled))
                        .collect::<Vec<_>>();
                    Ok::<_, String>((zip, format!("{slug}.zip"), entry, plugins, manifest.dsh_version.clone(), manifest.created_at.clone()))
                })
                .await
            {
                Ok(data) => {
                    advance(&this, cx, 0, Ok(()));
                    data
                }
                Err(e) => {
                    advance(&this, cx, 0, Err(e));
                    return;
                }
            };

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 2: 检查用户是否有 dsh-marina-shared-packs 仓库，没有则创建
            let (token, username) = (auth.token.clone(), auth.username.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    let exists = crate::github_pr::check_repo_exists(&token, &username, crate::github_pr::SHARED_PACKS_REPO)?;
                    if !exists {
                        crate::github_pr::create_repo(
                            &token,
                            crate::github_pr::SHARED_PACKS_REPO,
                            "DSH Marina 整合包分享仓库（自动创建）",
                        )?;
                        // 等待仓库初始化
                        std::thread::sleep(std::time::Duration::from_secs(3));
                    }
                    Ok::<_, String>(())
                })
                .await;
            if !advance(&this, cx, 1, result) {
                return;
            }

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 3: 创建 Release
            let (token, username, slug) = (auth.token.clone(), auth.username.clone(), pack_slug.clone());
            // 每次分享都用一个新 tag：同名整合包重复分享、或上次流程中断后重试，
            // 都不会撞上已存在的 Release（GitHub 对重复 tag 返回 422）。
            let tag = format!("pack-{}-v1.0.0-{}", slug, chrono::Utc::now().format("%Y%m%d%H%M%S"));
            let (release_name, release_body) = (pack_name.clone(), format!("整合包：{}\n\n{}", pack_name, pack_description));
            let release_id = match cx
                .background_executor()
                .spawn(async move {
                    crate::github_pr::create_release(
                        &token,
                        &username,
                        crate::github_pr::SHARED_PACKS_REPO,
                        &tag,
                        &release_name,
                        &release_body,
                    )
                })
                .await
            {
                Ok(id) => {
                    advance(&this, cx, 2, Ok(()));
                    id
                }
                Err(e) => {
                    advance(&this, cx, 2, Err(e));
                    return;
                }
            };

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 4: 上传 zip 到 Release
            let (token, username, filename_c) = (auth.token.clone(), auth.username.clone(), filename.clone());
            // 直接移交缓冲区：zip 可能有几百 MB，再复制一份会让内存翻倍。
            let download_url = match cx
                .background_executor()
                .spawn(async move {
                    crate::github_pr::upload_release_asset(
                        &token,
                        &username,
                        crate::github_pr::SHARED_PACKS_REPO,
                        release_id,
                        &filename_c,
                        zip_data,
                    )
                })
                .await
            {
                Ok(url) => {
                    advance(&this, cx, 3, Ok(()));
                    url
                }
                Err(e) => {
                    advance(&this, cx, 3, Err(e));
                    return;
                }
            };

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 5: 生成社区仓库需要的元数据（pack.json + README.md + index 条目）
            entry.url = download_url.clone();
            entry.name = pack_name.clone();
            entry.description = pack_description.clone();
            entry.author = auth.username.clone();

            // pack.json 的 description 至少 10 个字符，requirements.min_runtime 必须是 x.y.z
            let pack_description_full = if pack_description.chars().count() >= 10 {
                pack_description.clone()
            } else {
                format!("{pack_name}：{pack_description}（由 DSH Marina 分享）")
            };
            // schema 限制 description 最多 500 个字符
            let pack_description_full: String = pack_description_full.chars().take(500).collect();
            let is_semver = |value: &str| {
                let parts = value.split('.').collect::<Vec<_>>();
                parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
            };
            let min_runtime = if is_semver(&dsh_version) { dsh_version.clone() } else { "0.0.0".to_owned() };
            let pack_plugins: Vec<serde_json::Value> = if plugin_list.is_empty() {
                vec![serde_json::json!({ "name": "dsh-marina", "version": "*", "enabled": true })]
            } else {
                plugin_list
                    .iter()
                    .map(|(name, version, enabled)| {
                        serde_json::json!({
                            "name": name,
                            "version": if version.is_empty() { "*" } else { version.as_str() },
                            "enabled": enabled,
                        })
                    })
                    .collect()
            };
            let shared_repo_url = format!("https://github.com/{}/{}", auth.username, crate::github_pr::SHARED_PACKS_REPO);
            // schema 要求 metadata.created_at 为 YYYY-MM-DD；旧整合包可能没有该字段
            let created_date = {
                let candidate: String = created_at.chars().take(10).collect();
                let bytes = candidate.as_bytes();
                let valid = bytes.len() == 10
                    && bytes[4] == b'-'
                    && bytes[7] == b'-'
                    && candidate
                        .chars()
                        .enumerate()
                        .all(|(index, ch)| matches!(index, 4 | 7) || ch.is_ascii_digit());
                if valid {
                    candidate
                } else {
                    chrono::Utc::now().format("%Y-%m-%d").to_string()
                }
            };
            // schema 限制 name / author 最多 100 个字符
            let pack_name_meta: String = pack_name.chars().take(100).collect();
            let author_meta: String = auth.username.chars().take(100).collect();
            let pack_json = serde_json::json!({
                "schema_version": 1,
                "name": pack_name_meta,
                "version": "1.0.0",
                "author": author_meta,
                "description": pack_description_full,
                "requirements": { "min_runtime": min_runtime },
                "plugins": pack_plugins,
                "metadata": {
                    "repository": shared_repo_url,
                    "homepage": download_url,
                    "download_url": download_url.clone(),
                    "size": entry.size,
                    "sha256": entry.sha256.clone(),
                    "license": "MIT",
                    "created_at": created_date,
                }
            });
            let pack_json_bytes = match serde_json::to_vec_pretty(&pack_json) {
                Ok(bytes) => bytes,
                Err(e) => {
                    advance(&this, cx, 4, Err(format!("生成 pack.json 失败：{e}")));
                    return;
                }
            };
            let plugin_summary = if entry.plugins.is_empty() {
                "（无启用的插件）".to_owned()
            } else {
                entry.plugins.iter().map(|name| format!("- {name}")).collect::<Vec<_>>().join("\n")
            };
            let readme = format!(
                "# {pack_name}\n\n{pack_description}\n\n- **作者：** @{username}\n- **版本：** 1.0.0\n- **运行时：** {min_runtime}\n\n## 包含插件\n\n{plugin_summary}\n\n## 下载\n\n[{filename}]({download_url})\n\n---\n由 [DSH Marina](https://github.com/QuaonY/dsh-marina) 自动生成。\n",
                username = auth.username,
            );
            advance(&this, cx, 4, Ok(()));

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 6: Fork 社区仓库
            let (token, username) = (auth.token.clone(), auth.username.clone());
            let owner = match cx
                .background_executor()
                .spawn(async move { crate::github_pr::prepare_fork(&token, &username) })
                .await
            {
                Ok(owner) => {
                    advance(&this, cx, 5, Ok(()));
                    owner
                }
                Err(e) => {
                    advance(&this, cx, 5, Err(e));
                    return;
                }
            };

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 7: 创建分支（带时间戳，避免与之前的提交冲突）
            let branch = format!("share/{}-{}", pack_slug, chrono::Utc::now().format("%Y%m%d%H%M%S"));
            let (token, owner_c, branch_c) = (auth.token.clone(), owner.clone(), branch.clone());
            let result = cx
                .background_executor()
                .spawn(async move { crate::github_pr::create_branch(&token, &owner_c, &branch_c) })
                .await;
            if !advance(&this, cx, 6, result) {
                return;
            }

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // Step 8: 上传 pack.json、README.md，并更新社区目录 index.json
            let (token, owner_c, branch_c) = (auth.token.clone(), owner.clone(), branch.clone());
            let pack_dir = format!("packs/{pack_slug}");
            let pack_json_path = format!("{pack_dir}/pack.json");
            let readme_path = format!("{pack_dir}/README.md");
            let entry_for_index = entry.clone();
            let commit_msg = format!("Add pack: {pack_name}");
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::github_pr::upload_file(&token, &owner_c, &branch_c, &pack_json_path, &pack_json_bytes, &commit_msg)?;
                    crate::github_pr::upload_file(&token, &owner_c, &branch_c, &readme_path, readme.as_bytes(), &commit_msg)?;
                    crate::github_pr::update_catalog_index(&token, &owner_c, &branch_c, &entry_for_index)
                })
                .await;
            if !advance(&this, cx, 7, result) {
                return;
            }

            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                show_cancelled(&this, cx);
                return;
            }
            // 创建 Pull Request
            let title = format!("Add pack: {pack_name}");
            let body = format!(
                "## 新增整合包\n\n**名称：** {pack_name}\n**描述：** {pack_description}\n\n---\n由 DSH Marina 自动提交"
            );
            let token = auth.token.clone();
            let result = cx
                .background_executor()
                .spawn(async move { crate::github_pr::create_pull_request(&token, &owner, &branch, &title, &body) })
                .await;
            match result {
                Ok(url) => {
                    this.update(cx, |this, cx| {
                        if let Some(Dialog::ShareToCommunity { ref mut pr_url, ref mut progress, .. }) = this.dialog {
                            *pr_url = Some(url);
                            *progress = 100;
                            cx.notify();
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        if let Some(Dialog::ShareToCommunity { ref mut steps, ref mut error, .. }) = this.dialog {
                            steps[8].1 = ShareStepStatus::Failed;
                            *error = Some(e);
                            cx.notify();
                        }
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn export_instance(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let id = instance_id.to_string();
        self.last_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let scan_id = id.clone();
            let findings = cx
                .background_executor()
                .spawn(async move { packs::scan_export(&state.repository, &scan_id) })
                .await;
            let _ = this.update(cx, |this, cx| match findings {
                Ok(findings) if findings.is_empty() => this.save_export(id, false, cx),
                Ok(findings) => {
                    this.dialog = Some(Dialog::ExportScan {
                        instance_id: id,
                        findings,
                    });
                    cx.notify();
                }
                Err(e) => {
                    this.last_error = Some(format!("导出实例失败：{e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn save_export(&mut self, instance_id: String, redact: bool, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let name = state
                        .repository
                        .load(&instance_id)
                        .map(|c| c.name)
                        .unwrap_or_else(|_| instance_id.clone());
                    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M");
                    let file_name = storage::sanitize_file_name(&format!("{name}-{timestamp}.zip"));
                    let Some(destination) = rfd::FileDialog::new()
                        .set_title("导出整合包")
                        .set_directory(storage::desktop_directory())
                        .set_file_name(file_name)
                        .add_filter("Marina 整合包", &["zip"])
                        .save_file()
                    else {
                        return Ok(None);
                    };
                    if redact {
                        packs::export_instance_redacted(
                            &state.repository,
                            &instance_id,
                            &destination,
                        )
                    } else {
                        packs::export_instance(&state.repository, &instance_id, &destination)
                    }
                    .map_err(|e| e.to_string())?;
                    Ok::<Option<PathBuf>, String>(Some(destination))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(e) => {
                        this.last_error = Some(format!("导出实例失败：{e}"));
                    }
                    Ok(Some(path)) => {
                        let note = if redact { " (redacted)" } else { "" };
                        this.last_success = Some(format!("实例已导出到：{}{note}", path.display()));
                    }
                    Ok(None) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn clone_instance(
        &mut self,
        instance_id: &str,
        new_name: &str,
        personal: bool,
        cx: &mut Context<Self>,
    ) {
        let instance_id = instance_id.to_string();
        let new_name = new_name.to_string();
        self.run_copy_job(
            format!("正在克隆 {}", self.instance_name_of(&instance_id)),
            move |state, progress| {
                state
                    .repository
                    .clone_instance_with_progress(&instance_id, &new_name, personal, progress)
                    .map_err(|e| e.to_string())
            },
            |this, result, cx| match result {
                Err(e) => this.last_error = Some(format!("克隆实例失败：{e}")),
                Ok(config) => {
                    this.last_success = Some(format!("已复制为 {}", config.name));
                    this.refresh_instances(cx);
                    this.select_instance(config.id, cx);
                }
            },
            cx,
        );
    }

    fn get_instance_status(&self, id: &str) -> InstanceStatus {
        self.pending
            .get(id)
            .or_else(|| self.statuses.get(id))
            .cloned()
            .unwrap_or(InstanceStatus::Stopped)
    }

    /// Enable or disable plugins through `cordis.patch.yml` on a background thread.
    fn set_plugins_enabled(
        &mut self,
        instance_id: &str,
        names: Vec<String>,
        enabled: bool,
        success: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        // A second click would rewrite `cordis.patch.yml` while the first
        // write is still running: keep one write per instance at a time.
        if !self.plugin_busy.insert(instance_id.clone()) {
            return;
        }
        if let Err(error) = lifecycle::require_stopped(&self.state, &instance_id, "切换插件状态")
        {
            self.last_error = Some(error);
            cx.notify();
            return;
        }
        self.last_error = None;
        cx.notify();
        let busy_key = instance_id.clone();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paths = state
                        .repository
                        .paths(&instance_id)
                        .map_err(|e| e.to_string())?;
                    let changed = if enabled {
                        dsh_marina::plugins::enable_plugins(&paths.home, &names)
                    } else {
                        dsh_marina::plugins::disable_plugins(&paths.home, &names)
                    };
                    changed.map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.plugin_busy.remove(&busy_key);
                match result {
                    Err(e) => this.last_error = Some(format!("切换插件状态失败：{e}")),
                    Ok(_) => {
                        if let Some(message) = success {
                            this.last_success = Some(message.to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Finish a quick, synchronous instance config change.
    fn apply_instance_update(
        &mut self,
        result: Result<InstanceConfig, dsh_marina::instances::RepositoryError>,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = result {
            self.last_error = Some(format!("保存实例设置失败：{e}"));
        }
        self.refresh_instances(cx);
        cx.notify();
    }

    /// Apply a settings change and save it; returns whether the save worked.
    fn update_settings(
        &mut self,
        apply: impl FnOnce(&mut dsh_marina::settings::MarinaSettings),
        cx: &mut Context<Self>,
    ) -> bool {
        let store = &self.state.settings_store;
        let result = store
            .load()
            .map_err(|e| e.to_string())
            .and_then(|mut settings| {
                apply(&mut settings);
                store.save(&settings).map_err(|e| e.to_string())
            });
        let saved = match result {
            Ok(()) => true,
            Err(e) => {
                self.last_error = Some(format!("保存设置失败：{e}"));
                false
            }
        };
        cx.notify();
        saved
    }

    fn open_create_snapshot_dialog(
        &mut self,
        instance_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reason = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如：更新插件前")
                .default_value("手动快照")
        });
        reason.update(cx, |input, cx| input.focus(window, cx));
        self.bind_enter(&reason, cx);
        self.dialog = Some(Dialog::CreateSnapshot {
            instance_id,
            reason,
        });
        cx.notify();
    }

    fn open_rename_dialog(
        &mut self,
        instance_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .instances
            .iter()
            .find(|i| i.id == instance_id)
            .map(|i| i.name.clone())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("实例名称")
                .default_value(current)
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        self.bind_enter(&name, cx);
        self.dialog = Some(Dialog::RenameInstance { instance_id, name });
        cx.notify();
    }

    fn open_appearance_dialog(
        &mut self,
        instance_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(instance) = self.instances.iter().find(|i| i.id == instance_id).cloned() else {
            return;
        };
        let icon = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如：🚀（留空则显示名称首字）")
                .default_value(instance.icon.clone())
        });
        let notes = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("这个实例用来做什么？")
                .default_value(instance.notes.clone())
        });
        self.dialog = Some(Dialog::Appearance {
            instance_id,
            color: instance.color,
            icon,
            notes,
            error: None,
        });
        cx.notify();
    }

    fn open_share_pack_dialog(
        &mut self,
        mode: ShareMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.instances.is_empty() {
            return;
        }

        // 默认选中当前实例，没有就选第一个
        let instance = self
            .selected_instance
            .clone()
            .filter(|id| self.instances.iter().any(|i| &i.id == id))
            .or_else(|| self.instances.first().map(|i| i.id.clone()));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("例如：AI开发全家桶"));
        let description =
            cx.new(|cx| InputState::new(window, cx).placeholder("简要介绍这个整合包的用途和特点"));

        name.update(cx, |input, cx| input.focus(window, cx));

        self.bind_enter(&name, cx);
        self.bind_enter(&description, cx);
        self.dialog = Some(Dialog::SharePack {
            instance,
            name,
            description,
            mode,
            selected_folder: None,
            error: None,
            busy: false,
        });
        cx.notify();
    }

    /// 登录回来，把去登录前填的表单放回新开的分享对话框。
    fn restore_share_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.share_draft.take() else {
            return;
        };
        let instances = &self.instances;
        let Some(Dialog::SharePack {
            instance,
            name,
            description,
            mode,
            selected_folder,
            ..
        }) = self.dialog.as_mut()
        else {
            return;
        };
        if let Some(id) = draft.instance
            && instances.iter().any(|i| i.id == id)
        {
            *instance = Some(id);
        }
        *mode = draft.mode;
        *selected_folder = draft.folder;
        name.update(cx, |input, cx| input.set_value(draft.name, window, cx));
        description.update(cx, |input, cx| {
            input.set_value(draft.description, window, cx)
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn open_github_login_dialog(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 从分享对话框过来时记住已填的内容，登录成功后放回去
        self.packs.login_from_share = matches!(self.dialog, Some(Dialog::SharePack { .. }));
        self.share_draft = self.dialog.as_ref().and_then(|dialog| match dialog {
            Dialog::SharePack {
                instance,
                name,
                description,
                mode,
                selected_folder,
                ..
            } => Some(ShareDraft {
                instance: instance.clone(),
                mode: *mode,
                folder: selected_folder.clone(),
                name: name.read(cx).value().to_string(),
                description: description.read(cx).value().to_string(),
            }),
            _ => None,
        });
        // 第一步：显示说明，让用户主动点击"连接"
        self.dialog = Some(Dialog::GitHubLogin {
            user_code: String::new(),
            verification_uri: String::new(),
            waiting: false,
            error: None,
            connecting: false,
        });
        cx.notify();
    }

    fn start_github_device_flow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 显示"正在连接..."
        if let Some(Dialog::GitHubLogin {
            ref mut connecting, ..
        }) = self.dialog
        {
            *connecting = true;
        }
        cx.notify();

        let state = self.state.clone();
        cx.spawn_in(window, async move |this, cx| {
            let set_error = |this: &WeakEntity<Self>, cx: &mut AsyncWindowContext, message: String| {
                this.update(cx, |this, cx| {
                    if let Some(Dialog::GitHubLogin { ref mut error, ref mut connecting, .. }) = this.dialog {
                        *error = Some(message);
                        *connecting = false;
                        cx.notify();
                    }
                })
                .ok();
            };

            let response = match cx.background_executor().spawn(async move { github::start_device_flow() }).await {
                Ok(response) => response,
                Err(e) => {
                    set_error(&this, cx, e);
                    return;
                }
            };

            let user_code = response.user_code.clone();
            let opened = this
                .update(cx, |this, cx| {
                    // 用户在连接过程中关掉了对话框
                    if !matches!(this.dialog, Some(Dialog::GitHubLogin { .. })) {
                        return false;
                    }
                    this.dialog = Some(Dialog::GitHubLogin {
                        user_code: response.user_code.clone(),
                        verification_uri: response.verification_uri.clone(),
                        waiting: true,
                        error: None,
                        connecting: false,
                    });
                    cx.write_to_clipboard(ClipboardItem::new_string(response.user_code.clone()));
                    cx.open_url(&response.verification_uri);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !opened {
                return;
            }

            let mut interval = Duration::from_secs(response.interval.max(5));
            loop {
                cx.background_executor().timer(interval).await;

                // 对话框被关闭（或换成了新的登录码）就停止轮询
                let still_open = this
                    .update(cx, |this, _| {
                        matches!(&this.dialog, Some(Dialog::GitHubLogin { user_code: code, .. }) if *code == user_code)
                    })
                    .unwrap_or(false);
                if !still_open {
                    return;
                }

                let device_code = response.device_code.clone();
                let data_root = state.data_root.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let token = github::poll_device_token(&device_code)?;
                        let user = github::get_user_info(&token.access_token).map_err(github::PollError::Other)?;
                        let saved = github::save_token(&data_root, &token.access_token, &user);
                        Ok::<_, github::PollError>((token.access_token, user, saved))
                    })
                    .await;

                match result {
                    Ok((token, user, saved)) => {
                        this.update_in(cx, |this, window, cx| {
                            this.github_auth = Some(GitHubAuth {
                                token,
                                username: user.login.clone(),
                                avatar_url: Some(user.avatar_url.clone()),
                            });
                            this.last_success = Some(match saved {
                                Ok(()) => format!("已连接 GitHub 账号：{}", user.login),
                                Err(e) => format!("已连接 GitHub 账号：{}（{e}，下次启动需要重新登录）", user.login),
                            });
                            // 登录完成：回到发起时的界面
                            if this.packs.login_from_share {
                                this.open_share_pack_dialog(ShareMode::Community, window, cx);
                                this.restore_share_draft(window, cx);
                            } else {
                                this.dialog = Some(Dialog::CommunitySettings);
                                cx.notify();
                            }
                        })
                        .ok();
                        return;
                    }
                    Err(github::PollError::Pending) => {}
                    Err(github::PollError::SlowDown) => interval += Duration::from_secs(5),
                    Err(github::PollError::Expired) => {
                        set_error(&this, cx, "授权码已过期，请关闭后重新登录".to_owned());
                        return;
                    }
                    Err(github::PollError::Denied) => {
                        set_error(&this, cx, "授权被拒绝".to_owned());
                        return;
                    }
                    Err(github::PollError::Network(e)) | Err(github::PollError::Other(e)) => {
                        set_error(&this, cx, format!("认证失败：{e}"));
                        return;
                    }
                }
            }
        })
        .detach();
    }

    fn submit_appearance(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::Appearance {
            instance_id,
            color,
            icon,
            notes,
            error,
        }) = self.dialog.as_mut()
        else {
            return;
        };
        let icon_text = icon.read(cx).value().trim().to_owned();
        let notes_text = notes.read(cx).value().trim().to_owned();
        match self
            .state
            .repository
            .set_appearance(instance_id, color, &icon_text, &notes_text)
        {
            Ok(_) => {
                self.dialog = None;
                self.refresh_instances(cx);
            }
            Err(e) => *error = Some(e.to_string()),
        }
        cx.notify();
    }

    fn submit_share_pack(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::SharePack {
            instance,
            name,
            description,
            mode,
            selected_folder,
            error,
            busy,
        }) = self.dialog.as_mut()
        else {
            return;
        };

        if *busy {
            return;
        }

        let mode = *mode;

        let Some(instance_id) = instance.clone() else {
            *error = Some("请选择一个实例".to_owned());
            cx.notify();
            return;
        };

        let pack_name = name.read(cx).value().trim().to_owned();
        let pack_description = description.read(cx).value().trim().to_owned();

        if pack_name.is_empty() {
            *error = Some("请填写整合包名称".to_owned());
            cx.notify();
            return;
        }

        if pack_description.is_empty() {
            *error = Some("请填写整合包描述".to_owned());
            cx.notify();
            return;
        }

        match mode {
            ShareMode::Community => {
                if self.github_auth.is_none() {
                    *error = Some("请先连接 GitHub 账号".to_owned());
                    cx.notify();
                    return;
                }
                *busy = true;
                *error = None;
                cx.notify();
                self.share_to_community(instance_id, pack_name, pack_description, cx);
            }
            ShareMode::Local => {
                let Some(base_folder) = selected_folder.clone() else {
                    *error = Some("请选择保存位置".to_owned());
                    cx.notify();
                    return;
                };

                *busy = true;
                *error = None;
                cx.notify();
                self.share_instance_to_folder(
                    instance_id,
                    pack_name,
                    pack_description,
                    base_folder,
                    cx,
                );
            }
        }
    }

    fn select_share_folder(&mut self, cx: &mut Context<Self>) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("选择保存位置")
            .pick_folder();

        cx.spawn(async move |this, cx| {
            let folder = dialog.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Dialog::SharePack {
                    selected_folder,
                    error,
                    ..
                }) = this.dialog.as_mut()
                    && let Some(folder) = folder
                {
                    *selected_folder = Some(folder.path().to_path_buf());
                    *error = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_clone_dialog(
        &mut self,
        instance_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self
            .instances
            .iter()
            .find(|i| i.id == instance_id)
            .map(|i| i.name.clone())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("新的实例名称")
                .default_value(format!("{source} 副本"))
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        self.bind_enter(&name, cx);
        self.dialog = Some(Dialog::CloneInstance {
            instance_id,
            name,
            personal: false,
        });
        cx.notify();
    }

    /// Ask to roll back to the last-good snapshot.
    fn open_rollback_dialog(&mut self, id: String, cx: &mut Context<Self>) {
        let last_good = self
            .state
            .repository
            .paths(&id)
            .ok()
            .and_then(|paths| SnapshotStore::new(&paths.root).last_good().ok().flatten());
        match last_good {
            Some(snapshot) => {
                self.dialog = Some(Dialog::RestoreSnapshot {
                    instance_id: id,
                    snapshot_id: snapshot.id,
                });
            }
            None => self.last_error = Some("这个实例没有可恢复的「上次正常」快照".to_owned()),
        }
        cx.notify();
    }

    /// Record new toasts in the bell list and hide them after a while.
    fn track_notices(&mut self, cx: &mut Context<Self>) {
        const MAX_HISTORY: usize = 200;
        for error in [true, false] {
            let current = if error {
                self.last_error.clone()
            } else {
                self.last_success.clone()
            };
            let seen = if error {
                &mut self.notices.seen.0
            } else {
                &mut self.notices.seen.1
            };
            if *seen == current {
                continue;
            }
            *seen = current.clone();
            let Some(text) = current else { continue };
            self.notices
                .history
                .push((error, text.clone(), chrono::Local::now()));
            if self.notices.history.len() > MAX_HISTORY {
                self.notices.history.remove(0);
            }
            if !self.notices.panel_open {
                self.notices.unread += 1;
            }
            let generation = if error {
                &mut self.notices.generation.0
            } else {
                &mut self.notices.generation.1
            };
            *generation += 1;
            let mine = *generation;
            let delay = Duration::from_secs(if error { 8 } else { 4 });
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    let current = if error {
                        this.notices.generation.0
                    } else {
                        this.notices.generation.1
                    };
                    if current != mine {
                        return;
                    }
                    let slot = if error {
                        &mut this.last_error
                    } else {
                        &mut this.last_success
                    };
                    if slot.as_deref() == Some(text.as_str()) {
                        *slot = None;
                        if error {
                            this.notices.seen.0 = None
                        } else {
                            this.notices.seen.1 = None
                        }
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    /// Add a download to the list; `None` when the same one already runs.
    fn push_job(
        &mut self,
        key: String,
        title: String,
        total: u64,
        cx: &mut Context<Self>,
    ) -> Option<(Arc<Mutex<(u64, u64)>>, Arc<std::sync::atomic::AtomicBool>)> {
        if self.download.jobs.iter().any(|job| job.key == key) {
            return None;
        }
        let progress = Arc::new(Mutex::new((0u64, total)));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first = self.download.jobs.is_empty();
        self.download.jobs.push(DownloadJob {
            key,
            title,
            progress: progress.clone(),
            cancel: cancel.clone(),
            phase: JobPhase::Downloading,
            samples: Default::default(),
        });
        if first {
            // Repaint the progress ring and list while anything downloads.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    let Ok(active) = this.update(cx, |this, cx| {
                        for job in &mut this.download.jobs {
                            let (done, total) = job.amounts();
                            if total > 0 && done >= total {
                                job.phase = JobPhase::Installing;
                            }
                        }
                        cx.notify();
                        !this.download.jobs.is_empty()
                    }) else {
                        break;
                    };
                    if !active {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
        Some((progress, cancel))
    }

    fn finish_job(&mut self, key: &str, cx: &mut Context<Self>) {
        self.download.jobs.retain(|job| job.key != key);
        if self.download.jobs.is_empty() {
            self.download.panel_open = false;
        }
        cx.notify();
    }

    fn cancel_job(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(job) = self.download.jobs.iter().find(|job| job.key == key) {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Download and install one DSH version in the background.
    fn download_runtime(&mut self, version: download::RemoteVersion, cx: &mut Context<Self>) {
        let key = format!("dsh:{}", version.version);
        let total = version.feed.as_ref().map(|f| f.size).unwrap_or(0);
        let Some((progress, cancel)) =
            self.push_job(key.clone(), format!("DSH {}", version.version), total, cx)
        else {
            return;
        };
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let worker_cancel = cancel.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let settings = state.settings_store.load().unwrap_or_default();
                    let feed =
                        download::resolve_version(&version, &maintenance::feed_options(&settings))
                            .map_err(|e| e.to_string())?;
                    if let Ok(mut slot) = progress.lock() {
                        slot.1 = feed.size;
                    }
                    let mut report = |p: download::DownloadProgress| {
                        if let Ok(mut slot) = progress.lock() {
                            *slot = (p.downloaded, p.total.max(feed.size));
                        }
                    };
                    download::download_and_install(
                        &feed,
                        &state.registry,
                        &worker_cancel,
                        &mut report,
                    )
                    .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_job(&key, cx);
                match result {
                    Ok(runtime) => {
                        this.last_success = Some(format!("DSH {} 已安装", runtime.version));
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.mark_version_installed(&runtime.version);
                        this.suggest_runtime_cleanup(cx);
                    }
                    Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                        this.last_success = Some("已取消下载；下次可以继续".to_owned())
                    }
                    Err(e) => this.last_error = Some(format!("下载或安装 DSH 失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn mark_version_installed(&mut self, version: &str) {
        if let Some(Dialog::NewInstance(dialog)) = self.dialog.as_mut() {
            for choice in &mut dialog.versions {
                if choice.version == version {
                    choice.installed = true;
                }
            }
        }
    }

    /// Look up the newest DSH release from the configured sources.
    fn check_feed(&mut self, silent: bool, cx: &mut Context<Self>) {
        if self.download.checking {
            return;
        }
        self.download.checking = true;
        self.download.error = None;
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut settings = state.settings_store.load().unwrap_or_default();
                    let options = maintenance::feed_options(&settings);
                    let result =
                        download::list_versions(&options).and_then(|(source, versions, _)| {
                            let newest = versions
                                .first()
                                .ok_or(download::DownloadError::MissingField("version"))?;
                            Ok(download::FeedLookup {
                                feed: download::resolve_version(newest, &options)?,
                                source,
                                failures: Vec::new(),
                            })
                        });
                    settings.last_feed_check = Some(chrono::Utc::now().to_rfc3339());
                    let _ = state.settings_store.save(&settings);
                    let installed = state
                        .registry
                        .scan()
                        .map(|all| all.into_iter().map(|r| r.version).collect::<Vec<_>>())
                        .unwrap_or_default();
                    result
                        .map(|lookup| (lookup, installed))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.download.checking = false;
                match result {
                    Ok((lookup, installed)) => {
                        let newer = !installed.iter().any(|v| v == &lookup.feed.version)
                            && installed.first().is_none_or(|newest| {
                                dsh_marina::runtimes::compare_versions(&lookup.feed.version, newest)
                                    .is_gt()
                            });
                        if silent && newer {
                            this.last_success = Some(format!(
                                "DSH {} 有新版本；可在「设置 > DSH 版本管理」中下载",
                                lookup.feed.version
                            ));
                        }
                        this.download.lookup = Some(lookup);
                    }
                    Err(e) if !silent => this.download.error = Some(format!("检查更新失败：{e}")),
                    Err(e) => dsh_marina::applog::warn(&format!("每日下载源检查失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「关于」里的检查更新：查询 Marina 自己的最新正式版。
    fn check_app_update(&mut self, cx: &mut Context<Self>) {
        if self.app_update.checking {
            return;
        }
        self.app_update.checking = true;
        self.app_update.error = None;
        cx.notify();
        let token = self.github_auth.as_ref().map(|auth| auth.token.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { github::latest_release(token.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.app_update.checking = false;
                this.app_update.checked = true;
                match result {
                    Ok(Some(release)) => {
                        dsh_marina::applog::info(&format!(
                            "[Update] 最新发布: {}",
                            release.tag_name
                        ));
                        this.app_update.latest = Some(release);
                        this.app_update.error = None;
                    }
                    Ok(None) => {
                        dsh_marina::applog::info("[Update] 仓库还没有正式版本");
                        this.app_update.latest = None;
                        this.app_update.error = None;
                    }
                    Err(e) => {
                        dsh_marina::applog::warn(&format!("[Update] 检查更新失败：{e}"));
                        this.app_update.latest = None;
                        this.app_update.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Daily automatic update check (§FR-9).
    fn maybe_check_feed_daily(&mut self, cx: &mut Context<Self>) {
        let last = self
            .state
            .settings_store
            .load()
            .ok()
            .and_then(|s| s.last_feed_check);
        let due = last
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
            .is_none_or(|at| {
                chrono::Utc::now().signed_duration_since(at) > chrono::Duration::hours(24)
            });
        if due {
            self.check_feed(true, cx);
        }
    }

    fn start_download(&mut self, cx: &mut Context<Self>) {
        let Some(lookup) = self.download.lookup.as_ref() else {
            return;
        };
        let feed = lookup.feed.clone();
        self.download.error = None;
        self.download_runtime(
            download::RemoteVersion {
                version: feed.version.clone(),
                prerelease: feed.version.contains('-'),
                release_date: feed.release_date.clone(),
                feed_url: String::new(),
                feed: Some(feed),
            },
            cx,
        );
    }
    fn install_offline(&mut self, installer: PathBuf, cx: &mut Context<Self>) {
        self.download.installing = true;
        self.last_success = Some("正在安装离线包…".to_owned());
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    download::install_offline(&installer, &state.registry)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.download.installing = false;
                match result {
                    Ok(runtime) => {
                        this.last_success = Some(format!("已从离线包安装 DSH {}", runtime.version));
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.suggest_runtime_cleanup(cx);
                    }
                    Err(e) => this.last_error = Some(format!("离线安装失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Write the license list next to the log and open it.
    fn show_licenses(&mut self, cx: &mut Context<Self>) {
        let file = std::env::temp_dir().join("DSH-Marina-THIRD-PARTY-LICENSES.txt");
        match fs::write(&file, ui::THIRD_PARTY_LICENSES) {
            Ok(()) => platform::open_path(&file),
            Err(e) => {
                self.last_error = Some(format!("打开许可清单失败：{e}"));
                cx.notify();
            }
        }
    }

    /// FR-5 bisect: disable half of the plugins per round until one is left.
    fn start_bisect(&mut self, id: String, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let begin_id = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { lifecycle::bisect_begin(&state, &begin_id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(all) => match dsh_marina::bisect::Bisect::start(all.clone()) {
                        Some(bisect) => {
                            this.dialog = Some(Dialog::Bisect {
                                instance_id: id.clone(),
                                all,
                                bisect,
                                busy: false,
                            });
                            this.run_bisect_round(cx);
                        }
                        None => this.last_error = Some("没有启用的插件可用于二分排查".to_owned()),
                    },
                    Err(e) => this.last_error = Some(format!("启动二分排查失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_bisect_round(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::Bisect {
            instance_id,
            bisect,
            busy,
            ..
        }) = self.dialog.as_mut()
        else {
            return;
        };
        *busy = true;
        let (id, candidates, disabled) = (
            instance_id.clone(),
            bisect.candidates.clone(),
            bisect.disabled.clone(),
        );
        let state = self.state.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lifecycle::bisect_round(&state, &id, &candidates, &disabled) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Dialog::Bisect { busy, .. }) = this.dialog.as_mut() {
                    *busy = false;
                }
                if let Err(e) = result {
                    this.last_error = Some(format!(
                        "启动本轮二分排查失败（如果仍然失败可以直接反馈）: {e}"
                    ));
                }
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn report_bisect(&mut self, still_fails: bool, cx: &mut Context<Self>) {
        use dsh_marina::bisect::Step;
        let Some(Dialog::Bisect {
            instance_id,
            all,
            bisect,
            ..
        }) = self.dialog.as_mut()
        else {
            return;
        };
        let (id, all) = (instance_id.clone(), all.clone());
        let culprit = match bisect.report(still_fails) {
            Step::Test => {
                self.run_bisect_round(cx);
                return;
            }
            Step::Found(name) => Some(name),
            Step::NotPlugins => None,
        };
        self.finish_bisect(id, all, culprit, cx);
    }

    fn finish_bisect(
        &mut self,
        id: String,
        all: Vec<String>,
        culprit: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.dialog = None;
        let message = match &culprit {
            Some(name) => format!("二分排查完成：找到有问题的插件 {name}，已禁用它并恢复其余插件"),
            None => "二分排查完成：禁用插件后问题仍在，已恢复全部插件".to_owned(),
        };
        self.run_task(
            None,
            move |state| {
                lifecycle::bisect_finish(state, &id, &all, culprit.as_deref()).map(|()| message)
            },
            cx,
        );
    }

    fn cancel_bisect(&mut self, cx: &mut Context<Self>) {
        if let Some(Dialog::Bisect {
            instance_id, all, ..
        }) = self.dialog.as_ref()
        {
            let (id, all) = (instance_id.clone(), all.clone());
            self.dialog = None;
            self.run_task(
                None,
                move |state| {
                    lifecycle::bisect_finish(state, &id, &all, None)
                        .map(|()| "已取消二分排查，插件已恢复".to_owned())
                },
                cx,
            );
        }
    }

    /// FR-3: pick local `.tgz` plugin packages and install them offline.
    fn install_plugin_packages(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(files) = rfd::FileDialog::new()
            .set_title("选择插件包（.tgz）")
            .add_filter("插件包", &["tgz"])
            .pick_files()
        else {
            return;
        };
        let count = files.len();
        self.last_success = Some(format!("正在安装 {count} 个插件包…"));
        self.run_task(
            Some((id.clone(), InstanceStatus::Starting)),
            move |state| {
                lifecycle::add_plugin_packages(state, &id, &files)
                    .map(|()| format!("已安装 {count} 个插件包"))
            },
            cx,
        );
    }

    /// FR-9: more managed versions than `max_runtimes` → offer to remove the
    /// oldest ones no instance uses.
    fn suggest_runtime_cleanup(&mut self, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let max = self
            .state
            .settings_store
            .load()
            .map(|s| s.max_runtimes)
            .unwrap_or(3);
        let managed = self.state.registry.scan().unwrap_or_default();
        let all = self
            .state
            .registry
            .all(&self.state.runtime_store.load().unwrap_or_default())
            .unwrap_or_default();
        let used = dsh_marina::runtimes::used_runtime_ids(&self.instances, &all);
        let candidates = dsh_marina::runtimes::cleanup_candidates(&managed, &used, max);
        if !candidates.is_empty() {
            let versions = candidates.into_iter().map(|r| (r.id, r.version)).collect();
            self.dialog = Some(Dialog::CleanupRuntimes { versions, max });
            cx.notify();
        }
    }

    fn cleanup_runtimes(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        self.run_task(
            None,
            move |state| {
                let mut removed = 0;
                for id in &ids {
                    maintenance::remove_runtime(state, id)?;
                    removed += 1;
                }
                lifecycle::invalidate_disk_usage(state);
                Ok(format!("已移除 {removed} 个旧版本（已放入回收站）"))
            },
            cx,
        );
    }

    fn delete_runtime(&mut self, runtime_id: String, cx: &mut Context<Self>) {
        self.run_task(
            None,
            move |state| {
                let message = maintenance::remove_runtime(state, &runtime_id)?;
                lifecycle::invalidate_disk_usage(state);
                Ok(message)
            },
            cx,
        );
    }

    fn rebind_runtime(&mut self, runtime_id: String, binding: String, cx: &mut Context<Self>) {
        self.run_task(
            None,
            move |state| {
                let message = maintenance::rebind_and_remove_runtime(state, &runtime_id, &binding)?;
                lifecycle::invalidate_disk_usage(state);
                Ok(message)
            },
            cx,
        );
    }

    fn uninstall_external(&mut self, delete_data: bool, cx: &mut Context<Self>) {
        self.last_success = Some("正在卸载外部 DSH…".to_owned());
        self.run_task(
            None,
            move |state| maintenance::uninstall_external(state, delete_data),
            cx,
        );
    }

    /// 标记托盘提示已显示
    /// "更改数据位置…": pick a folder, validate it, then confirm (design §8.1).
    fn pick_data_location(&mut self, cx: &mut Context<Self>) {
        if data_root_override().is_some() {
            self.last_error = Some("数据位置由 DSH_MARINA_DATA 指定，不能在这里更改".to_owned());
            cx.notify();
            return;
        }
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let current = state.repository.data_root().to_path_buf();
                    let Some(picked) = rfd::FileDialog::new()
                        .set_title("选择新的数据位置")
                        .pick_folder()
                    else {
                        return Ok(None);
                    };
                    relocate::plan_target(&current, &picked)
                        .map(Some)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Some(target)) => this.dialog = Some(Dialog::MoveData { target }),
                    Ok(None) => {}
                    Err(e) => this.last_error = Some(format!("无法把数据移到这个位置：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy the data root to `target`, point Marina at it and restart.
    fn move_data(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        self.dialog = None;
        let running = self
            .statuses
            .values()
            .any(|s| !matches!(s, InstanceStatus::Stopped | InstanceStatus::Crashed));
        if running {
            self.last_error = Some("请先停止所有实例再移动数据".to_owned());
            cx.notify();
            return;
        }
        let copy_target = target.clone();
        self.run_copy_job(
            "正在移动数据目录…".to_owned(),
            move |state, progress| {
                let busy = state
                    .processes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .running_instances()
                    .unwrap_or_default();
                if !busy.is_empty() {
                    return Err("请先停止所有实例再移动数据".to_owned());
                }
                relocate::copy_data(
                    state.repository.data_root(),
                    &copy_target,
                    progress,
                    platform::free_space,
                )
                .map_err(|e| e.to_string())
            },
            move |this, result, cx| match result {
                Err(e) => this.last_error = Some(format!("移动数据失败：{e}")),
                Ok(_) => {
                    this.last_success = Some("数据已复制，正在重启 Marina…".to_owned());
                    let pointer_file = relocate::default_pointer_file(&default_data_root());
                    perform_quit(
                        StopMode::Graceful,
                        Some(Box::new(move |state: &AppState| {
                            let previous = relocate::read_pointer(&pointer_file);
                            let pointer = relocate::Pointer {
                                data_root: Some(target.clone()),
                                remove_previous: Some(state.repository.data_root().to_path_buf()),
                            };
                            relocate::write_pointer(&pointer_file, &pointer)
                                .map_err(|e| e.to_string())?;
                            platform::spawn_restart().inspect_err(|_| {
                                let _ = relocate::write_pointer(&pointer_file, &previous);
                                let _ = fs::remove_dir_all(&target);
                            })
                        })),
                        cx,
                    );
                }
            },
            cx,
        );
    }

    /// A `dsh://` link arrived from Windows: open it with the default
    /// instance, or ask which one.
    fn receive_link(&mut self, url: String, cx: &mut Context<Self>) {
        if let Some(hwnd) = cx.try_global::<MarinaApp>().and_then(|a| a.hwnd) {
            platform::show_window(hwnd);
        }
        let default = self
            .state
            .settings_store
            .load()
            .ok()
            .and_then(|s| s.default_link_instance)
            .filter(|id| self.instances.iter().any(|i| &i.id == id));
        if let Some(id) = default {
            self.open_link(id, url, cx);
            return;
        }
        let selected = self
            .selected_instance
            .clone()
            .or_else(|| self.instances.first().map(|i| i.id.clone()));
        self.dialog = Some(Dialog::OpenLink {
            url,
            input: None,
            selected,
            remember: false,
            error: None,
        });
        cx.notify();
    }

    /// "打开 dsh:// 链接…": type a link and pick the instance.
    fn open_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("dsh://"));
        input.update(cx, |input, cx| input.focus(window, cx));
        let selected = self
            .selected_instance
            .clone()
            .or_else(|| self.instances.first().map(|i| i.id.clone()));
        self.dialog = Some(Dialog::OpenLink {
            url: String::new(),
            input: Some(input),
            selected,
            remember: false,
            error: None,
        });
        cx.notify();
    }

    fn submit_open_link(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::OpenLink {
            url,
            input,
            selected,
            remember,
            error,
        }) = self.dialog.as_mut()
        else {
            return;
        };
        let text = match input {
            Some(input) => input.read(cx).value().trim().to_owned(),
            None => url.clone(),
        };
        let checked = links::validate(&text).and_then(|url| match selected {
            Some(id) => Ok((id.clone(), url)),
            None => Err("请选择一个实例".to_owned()),
        });
        let (id, url) = match checked {
            Ok(value) => value,
            Err(e) => {
                *error = Some(e);
                cx.notify();
                return;
            }
        };
        let remember = *remember;
        self.dialog = None;
        if remember {
            let default = id.clone();
            self.update_settings(move |s| s.default_link_instance = Some(default), cx);
        }
        self.open_link(id, url, cx);
    }

    fn open_link(&mut self, id: String, url: String, cx: &mut Context<Self>) {
        let name = self.instance_name_of(&id);
        let running = matches!(
            self.get_instance_status(&id),
            InstanceStatus::Running | InstanceStatus::AdoptedRunning
        );
        let pending = (!running).then(|| (id.clone(), InstanceStatus::Starting));
        self.run_task(
            pending,
            move |state| {
                lifecycle::open_link(state, &id, &url)
                    .map(|forwarded| {
                        if forwarded {
                            format!("已把链接转发给 {name}")
                        } else {
                            format!("正在用 {name} 打开链接")
                        }
                    })
                    .map_err(|e| format!("打开链接失败：{e}"))
            },
            cx,
        );
    }

    /// Take over `dsh://` (saving the previous handler) or give it back.
    fn set_handle_links(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let settings = self.state.settings_store.load().unwrap_or_default();
        let result = platform::current_executable().and_then(|exe| {
            if enabled {
                let previous = links::registry::current_command()
                    .filter(|c| !links::is_marina_command(c, &exe))
                    .or(settings.previous_link_command.clone());
                links::registry::register(&exe).map(|()| previous)
            } else {
                links::registry::restore(settings.previous_link_command.as_deref()).map(|()| None)
            }
        });
        match result {
            Ok(previous) => {
                self.update_settings(
                    move |s| {
                        s.handle_links = enabled;
                        s.previous_link_command = previous;
                    },
                    cx,
                );
            }
            Err(e) => {
                self.last_error = Some(format!("配置 dsh:// 链接处理失败：{e}"));
                cx.notify();
            }
        }
    }

    fn open_uninstall_marina(&mut self, cx: &mut Context<Self>) {
        self.dialog = Some(Dialog::UninstallMarina {
            mode: maintenance::UninstallMode::ExportAll,
            stats: None,
        });
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let stats =
                cx.background_executor()
                    .spawn(async move {
                        storage::uninstall_stats(&state.repository, &state.registry).ok()
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Dialog::UninstallMarina { stats: slot, .. }) = this.dialog.as_mut() {
                    *slot = stats;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Stop every instance, save data as chosen, schedule removal and quit.
    fn confirm_uninstall_marina(
        &mut self,
        mode: maintenance::UninstallMode,
        cx: &mut Context<Self>,
    ) {
        self.dialog = None;
        self.last_error = None;
        self.last_success = Some("正在停止所有实例并卸载 DSH Marina…".to_owned());
        cx.notify();
        perform_quit(
            StopMode::Graceful,
            Some(Box::new(move |state: &AppState| {
                maintenance::uninstall_marina(state, mode).map(|saved| {
                    if let Some(saved) = saved {
                        platform::open_path(&saved);
                    }
                })
            })),
            cx,
        );
    }

    fn open_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut installed: Vec<String> = self
            .state
            .registry
            .scan()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.is_external())
            .map(|r| r.version)
            .collect();
        installed.sort_by(|a, b| dsh_marina::runtimes::compare_versions(b, a));
        let settings = self.state.settings_store.load().unwrap_or_default();
        let options = maintenance::feed_options(&settings);
        let feed_url = options.feed_url(options.preferred).unwrap_or_default();

        let name =
            cx.new(|cx| InputState::new(window, cx).placeholder("给实例起个名字，例如「工作」"));
        name.update(cx, |input, cx| input.focus(window, cx));
        self.bind_enter(&name, cx);
        self.dialog = Some(Dialog::NewInstance(NewInstanceDialog {
            name,
            selected: installed.first().cloned(),
            versions: installed
                .into_iter()
                .map(|version| VersionChoice {
                    version,
                    installed: true,
                    remote: None,
                })
                .collect(),
            feed_url,
            listing: false,
            list_error: None,
            list_note: None,
            only_prerelease: false,
            error: None,
            mode: NewMode::Blank,
            personal: false,
            clone_source: self
                .selected_instance
                .clone()
                .or_else(|| self.instances.first().map(|i| i.id.clone())),
            import_file: None,
            busy: false,
        }));
        self.list_versions(cx);
        cx.notify();
    }

    /// "更换运行时…": switch an existing instance to another DSH version in
    /// place.  Data, plugins and sessions never move; only the binding does.
    fn open_switch_runtime_dialog(&mut self, instance_id: String, cx: &mut Context<Self>) {
        let Ok(instance) = self.state.repository.load(&instance_id) else {
            return;
        };
        let mut installed: Vec<String> = self
            .state
            .registry
            .scan()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.is_external())
            .map(|r| r.version)
            .collect();
        installed.sort_by(|a, b| dsh_marina::runtimes::compare_versions(b, a));
        let selected = if instance.runtime.dsh_version == EXTERNAL_RUNTIME_ID {
            // The external DSH has no row of its own; nothing is preselected.
            None
        } else {
            Some(instance.runtime.dsh_version.clone())
        };
        self.dialog = Some(Dialog::SwitchRuntime(SwitchRuntimeDialog {
            instance_id,
            current: instance.runtime.dsh_version.clone(),
            versions: installed
                .into_iter()
                .map(|version| VersionChoice {
                    version,
                    installed: true,
                    remote: None,
                })
                .collect(),
            selected,
            listing: false,
            list_error: None,
            list_note: None,
            only_prerelease: false,
            busy: false,
            incompatible: Vec::new(),
        }));
        self.list_switch_versions(cx);
        cx.notify();
    }

    /// Read every version the download source offers into the switch dialog.
    fn list_switch_versions(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::SwitchRuntime(dialog)) = self.dialog.as_mut() else {
            return;
        };
        if dialog.listing {
            return;
        }
        dialog.listing = true;
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let settings = state.settings_store.load().unwrap_or_default();
                    download::list_versions(&maintenance::feed_options(&settings))
                        .map(|listed| (listed, download::take_list_note()))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(Dialog::SwitchRuntime(dialog)) = this.dialog.as_mut() else {
                    return;
                };
                dialog.listing = false;
                match result {
                    Ok(((_source, remote, only_pre), note)) => {
                        dialog.only_prerelease = only_pre;
                        dialog.list_note = note;
                        for version in remote {
                            match dialog
                                .versions
                                .iter_mut()
                                .find(|c| c.version == version.version)
                            {
                                Some(choice) => choice.remote = Some(version),
                                None => dialog.versions.push(VersionChoice {
                                    version: version.version.clone(),
                                    installed: false,
                                    remote: Some(version),
                                }),
                            }
                        }
                        dialog.versions.sort_by(|a, b| {
                            dsh_marina::runtimes::compare_versions(&b.version, &a.version)
                        });
                        if dialog.selected.is_none() {
                            dialog.selected = dialog.versions.first().map(|c| c.version.clone());
                        }
                        let id = dialog.instance_id.clone();
                        let selected = dialog.selected.clone();
                        let incompatible = this.incompatible_plugins(&id, selected.as_deref());
                        if let Some(Dialog::SwitchRuntime(dialog)) = this.dialog.as_mut() {
                            dialog.incompatible = incompatible;
                        }
                    }
                    Err(e) => dialog.list_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Installed plugins the market index declares incompatible with `version`
    /// (an empty version or a missing index means "nothing to warn about").
    fn incompatible_plugins(&self, instance_id: &str, version: Option<&str>) -> Vec<String> {
        let Some(version) = version.filter(|v| *v != LATEST_STABLE && *v != EXTERNAL_RUNTIME_ID)
        else {
            return Vec::new();
        };
        let Some(index) = self.market.index.as_ref() else {
            return Vec::new();
        };
        let Ok(paths) = self.state.repository.paths(instance_id) else {
            return Vec::new();
        };
        let Ok(installed) = dsh_marina::plugins::list_plugins(&paths.home) else {
            return Vec::new();
        };
        let mut names: Vec<String> = installed
            .iter()
            .filter(|plugin| {
                index
                    .plugins
                    .iter()
                    .any(|entry| entry.name == plugin.name && !entry.supports(version))
            })
            .map(|plugin| plugin.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Apply the chosen runtime to an existing instance: snapshot first, then
    /// rebind; download the runtime afterwards when it is not installed yet.
    fn submit_switch_runtime(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::SwitchRuntime(dialog)) = self.dialog.as_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let Some(target) = dialog.selected.clone() else {
            return;
        };
        if target == dialog.current {
            return;
        }
        let id = dialog.instance_id.clone();
        dialog.busy = true;
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result: Result<(InstanceConfig, Option<download::RemoteVersion>), String> = cx
                .background_executor()
                .spawn(async move {
                    lifecycle::require_stopped(&state, &id, "更换运行时")?;
                    let config = state.repository.load(&id).map_err(|e| e.to_string())?;
                    if config.runtime.dsh_version == target {
                        return Ok((config, None));
                    }
                    let paths = state.repository.paths(&id).map_err(|e| e.to_string())?;
                    let _snapshot = dsh_marina::snapshots::SnapshotStore::new(&paths.root)
                        .create(format!("切换到 {} 之前的快照", runtime_label(&target)))
                        .map_err(|e| format!("切换前的快照创建失败：{e}"))?;
                    let config = state
                        .repository
                        .switch_runtime_binding(&id, &target)
                        .map_err(|e| e.to_string())?;
                    let installed = state
                        .registry
                        .scan()
                        .unwrap_or_default()
                        .iter()
                        .any(|r| r.version == target);
                    let remote = if installed
                        || target == LATEST_STABLE
                        || target == EXTERNAL_RUNTIME_ID
                    {
                        None
                    } else {
                        let settings = state.settings_store.load().unwrap_or_default();
                        download::list_versions(&maintenance::feed_options(&settings))
                            .ok()
                            .and_then(|(_, remote, _)| {
                                remote.into_iter().find(|v| v.version == target)
                            })
                    };
                    Ok((config, remote))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((config, remote)) => {
                        this.dialog = None;
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.refresh_instances(cx);
                        let message = format!(
                            "实例「{}」已切换到 {}；数据、插件和会话保持原样，切换前的配置已存为快照",
                            config.name,
                            runtime_label(&config.runtime.dsh_version)
                        );
                        this.last_success = Some(match &remote {
                            Some(v) => format!("{message}。正在下载 DSH {}，下载完成后即可启动", v.version),
                            None => message,
                        });
                        if let Some(version) = remote {
                            this.download_runtime(version, cx);
                        }
                    }
                    Err(e) => {
                        if let Some(Dialog::SwitchRuntime(dialog)) = this.dialog.as_mut() {
                            dialog.busy = false;
                        }
                        this.last_error = Some(format!("更换运行时失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// One click back to the runtime the instance used before its last switch.
    fn revert_runtime(&mut self, id: String, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    lifecycle::require_stopped(&state, &id, "换回上一个运行时")?;
                    let config = state.repository.load(&id).map_err(|e| e.to_string())?;
                    // The old runtime may have been deleted since the switch;
                    // reverting to it would leave the instance unable to start.
                    if let Some(previous) = config.runtime.previous_binding.as_deref() {
                        let special = previous == LATEST_STABLE || previous == EXTERNAL_RUNTIME_ID;
                        let installed = state
                            .registry
                            .all(&state.runtime_store.load().unwrap_or_default())
                            .unwrap_or_default()
                            .iter()
                            .any(|r| r.version == previous || r.id == previous);
                        if !special && !installed {
                            return Err(format!(
                                "上一个运行时 {} 已经不在了。请用「更换运行时…」选一个还在的版本",
                                runtime_label(previous)
                            ));
                        }
                    }
                    state
                        .repository
                        .revert_runtime_binding(&id)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(config) => {
                        this.refresh_instances(cx);
                        this.last_success = Some(format!(
                            "实例「{}」已换回 {}",
                            config.name,
                            runtime_label(&config.runtime.dsh_version)
                        ));
                    }
                    Err(e) => this.last_error = Some(format!("换回上一个运行时失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pick an instance package for the "导入整合包" mode and read its manifest.
    fn pick_import_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name_input = match self.dialog.as_ref() {
            Some(Dialog::NewInstance(dialog)) => dialog.name.clone(),
            _ => return,
        };
        let task = cx.background_executor().spawn(async move {
            let path = rfd::FileDialog::new()
                .set_title("选择整合包")
                .set_directory(storage::desktop_directory())
                .add_filter("Marina 整合包", &["zip"])
                .pick_file()?;
            let manifest = packs::preview_pack(&path).map_err(|e| e.to_string());
            Some((path, manifest))
        });
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let Some((path, manifest)) = task.await else {
                return;
            };
            let _ = cx.update_window(window_handle, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    let Some(Dialog::NewInstance(dialog)) = this.dialog.as_mut() else {
                        return;
                    };
                    // Name the instance after the package unless one was typed.
                    if let Ok(manifest) = &manifest
                        && name_input.read(cx).value().trim().is_empty()
                    {
                        let name = manifest.name.clone();
                        name_input.update(cx, |input, cx| input.set_value(name, window, cx));
                    }
                    dialog.import_file = Some((path, manifest));
                    dialog.error = None;
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Import a new instance from a package exported by Marina. The
    /// package's DSH version is used when installed, downloaded when the
    /// source offers it, else the instance follows the newest stable one.
    /// Plugins are then installed from the package's lock file.
    fn import_instance_package(&mut self, path: PathBuf, name: String, cx: &mut Context<Self>) {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.last_error = None;
        self.last_success = Some(format!("正在导入 {label}…"));
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = packs::import_pack(&state.repository, &path, Some(&name)).map_err(|e| e.to_string())?;
                    let wanted = config.runtime.dsh_version.clone();
                    let pinned = !wanted.is_empty() && wanted != LATEST_STABLE && wanted != EXTERNAL_RUNTIME_ID;
                    let installed = state.registry.scan().unwrap_or_default().iter().any(|r| r.version == wanted);
                    if !pinned || installed {
                        let warning = lifecycle::install_plugins(&state, &config.id).err();
                        return Ok::<_, String>((config, None, warning));
                    }
                    // Not installed here: download it if the source has it.
                    let settings = state.settings_store.load().unwrap_or_default();
                    let remote = download::list_versions(&maintenance::feed_options(&settings))
                        .ok()
                        .and_then(|(_, versions, _)| versions.into_iter().find(|v| v.version == wanted));
                    match remote {
                        Some(remote) => Ok((config, Some(Ok(remote)), None)),
                        None => {
                            let config = state.repository.set_runtime_binding(&config.id, LATEST_STABLE).map_err(|e| e.to_string())?;
                            Ok((config, Some(Err(wanted)), None))
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((config, runtime, warning)) => {
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.refresh_instances(cx);
                        this.last_success = Some(match &runtime {
                            None => format!("已导入实例 {}", config.name),
                            Some(Ok(remote)) => format!(
                                "已导入实例 {}；正在下载它需要的 DSH {}。插件会在首次启动时安装",
                                config.name, remote.version
                            ),
                            Some(Err(version)) => format!(
                                "已导入实例 {}。它的整合包使用 DSH {version}，但下载源里没有这个版本；改用最新的稳定版",
                                config.name
                            ),
                        });
                        if let Some(warning) = warning {
                            this.last_error = Some(format!("插件依赖没有完全安装；DSH 首次启动时会自行安装：{warning}"));
                        }
                        let outside = lifecycle::outside_paths(&this.state, &config.id);
                        if !outside.is_empty() {
                            this.last_error = Some(format!(
                                "实例 {} 的插件配置包含实例目录之外的路径：{}",
                                config.name,
                                outside.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                            ));
                        }
                        if let Some(Ok(remote)) = runtime {
                            this.download_runtime(remote, cx);
                        }
                        this.select_instance(config.id, cx);
                    }
                    Err(e) => this.last_error = Some(format!("导入整合包失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Read every version the download source offers into the dialog.
    fn list_versions(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::NewInstance(dialog)) = self.dialog.as_mut() else {
            return;
        };
        if dialog.listing {
            return;
        }
        dialog.listing = true;
        dialog.list_error = None;
        dialog.list_note = None;
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let settings = state.settings_store.load().unwrap_or_default();
                    download::list_versions(&maintenance::feed_options(&settings))
                        .map(|listed| (listed, download::take_list_note()))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(Dialog::NewInstance(dialog)) = this.dialog.as_mut() else {
                    return;
                };
                dialog.listing = false;
                match result {
                    Ok(((source, remote, only_prerelease), note)) => {
                        dialog.only_prerelease = only_prerelease;
                        dialog.list_note = note;
                        if let Some(url) = remote.first().map(|v| v.feed_url.clone())
                            && source != download::DownloadSource::Official
                            && dialog.feed_url.is_empty()
                        {
                            dialog.feed_url = url;
                        }
                        for version in remote {
                            match dialog
                                .versions
                                .iter_mut()
                                .find(|c| c.version == version.version)
                            {
                                Some(choice) => choice.remote = Some(version),
                                None => dialog.versions.push(VersionChoice {
                                    version: version.version.clone(),
                                    installed: false,
                                    remote: Some(version),
                                }),
                            }
                        }
                        dialog.versions.sort_by(|a, b| {
                            dsh_marina::runtimes::compare_versions(&b.version, &a.version)
                        });
                        if dialog.selected.is_none() {
                            dialog.selected = dialog
                                .versions
                                .iter()
                                .find(|c| c.remote.as_ref().is_none_or(|r| !r.prerelease))
                                .or(dialog.versions.first())
                                .map(|c| c.version.clone());
                        }
                    }
                    Err(e) => dialog.list_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 让输入框按回车等同于点对话框的主按钮。
    fn bind_enter(&self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        cx.subscribe(input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_active_dialog(cx);
            }
        })
        .detach();
    }

    /// 设置里的下载链接输入框：回车即保存。
    fn bind_link_enter(&self, cx: &mut Context<Self>) {
        let input = self.custom_link_input.clone();
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_download_link(cx);
            }
        })
        .detach();
    }

    fn submit_new_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::NewInstance(dialog)) = self.dialog.as_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let name = dialog.name.read(cx).value().trim().to_owned();
        let error = match dialog.mode {
            _ if name.is_empty() => Some("请输入实例名称"),
            NewMode::Clone if dialog.clone_source.is_none() => Some("请选择要克隆的实例"),
            NewMode::Import if !matches!(dialog.import_file, Some((_, Ok(_)))) => {
                Some("请选择有效的整合包")
            }
            _ => None,
        };
        if let Some(error) = error {
            dialog.error = Some(error.to_owned());
            cx.notify();
            return;
        }
        let (mode, personal) = (dialog.mode, dialog.personal);
        let clone_source = dialog.clone_source.clone();

        // A copy takes everything from its source, the DSH version included,
        // and can take minutes; run it behind the progress overlay.
        if mode == NewMode::Clone {
            self.dialog = None;
            self.run_copy_job(
                "正在克隆实例…".to_owned(),
                move |state, progress| {
                    state
                        .repository
                        .clone_instance_with_progress(clone_source.as_deref().unwrap_or_default(), name, personal, progress)
                        .map_err(|e| e.to_string())
                },
                |this, result, cx| match result {
                    Ok(config) => {
                        this.refresh_instances(cx);
                        this.last_success = Some(format!("已创建实例 {}", config.name));
                        let outside = lifecycle::outside_paths(&this.state, &config.id);
                        if !outside.is_empty() {
                            this.last_error = Some(format!(
                                "实例 {} 的插件配置包含实例目录之外的路径；这些插件仍可能写入原来的位置：{}",
                                config.name,
                                outside.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                            ));
                        }
                        this.select_instance(config.id, cx);
                    }
                    Err(e) => this.last_error = Some(format!("创建实例失败：{e}")),
                },
                cx,
            );
            return;
        }

        if mode == NewMode::Import {
            let Some((path, Ok(_))) = dialog.import_file.clone() else {
                return;
            };
            self.dialog = None;
            self.import_instance_package(path, name, cx);
            return;
        }

        // Blank: bind the chosen version and download it when missing.
        let choice = dialog
            .selected
            .as_ref()
            .and_then(|v| dialog.versions.iter().find(|c| &c.version == v))
            .cloned();
        let binding = choice
            .as_ref()
            .map(|c| c.version.clone())
            .unwrap_or_else(|| LATEST_STABLE.to_owned());
        let download = choice.filter(|c| !c.installed).and_then(|c| c.remote);
        dialog.busy = true;
        dialog.error = None;
        cx.notify();

        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let repository = &state.repository;
                    let config = repository.create_blank(name).map_err(|e| e.to_string())?;
                    if binding == config.runtime.dsh_version {
                        Ok(config)
                    } else {
                        repository
                            .set_runtime_binding(&config.id, binding)
                            .map_err(|e| e.to_string())
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(config) => {
                        this.dialog = None;
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.refresh_instances(cx);
                        this.last_success = Some(match &download {
                            Some(v) => format!(
                                "已创建实例 {}；正在下载 DSH {}。下载完成后即可启动",
                                config.name, v.version
                            ),
                            None => format!("已创建实例 {}", config.name),
                        });
                        if let Some(version) = download {
                            this.download_runtime(version, cx);
                        }
                        this.select_instance(config.id, cx);
                    }
                    Err(e) => {
                        if let Some(Dialog::NewInstance(dialog)) = this.dialog.as_mut() {
                            dialog.busy = false;
                            dialog.error = Some(format!("创建实例失败：{e}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn main() {
    // Initialize data directories
    let data_root = data_root();
    println!("数据目录：{}", data_root.display());

    if let Err(e) = ensure_dirs(&data_root) {
        eprintln!("创建数据目录失败：{e}");
        std::process::exit(1);
    }
    dsh_marina::applog::init(&data_root);

    // A second launch just brings the running Marina to the front.  After a
    // data move the new process waits for the old one to release the lock.
    // A `dsh://` link (Marina registered as the handler) is queued for the
    // running Marina, or opened by this one when it is the first.
    let open_url = argument_value("--open-url").and_then(|url| links::validate(&url).ok());
    if let Some(url) = &open_url
        && let Err(e) = links::enqueue(&data_root, url)
    {
        dsh_marina::applog::warn(&format!("排队 dsh:// 链接失败：{e}"));
    }
    let start_hidden = open_url.is_none() && std::env::args().any(|a| a == "--background");
    let wait_for_restart = argument_value("--wait-for").is_some();
    let mut single_instance = platform::acquire_single_instance();
    if wait_for_restart {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while single_instance.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(300));
            single_instance = platform::acquire_single_instance();
        }
    }
    let Some(_single_instance) = single_instance else {
        platform::activate_window(APP_TITLE);
        return;
    };

    // Load backend state
    let state = Arc::new(AppState::new(&data_root));
    dsh_marina::i18n::init(&data_root);
    dsh_marina::applog::info(&format!(
        "DSH Marina {} 已启动，数据目录 {}",
        env!("CARGO_PKG_VERSION"),
        data_root.display()
    ));

    // Delete the old root left behind by a data move (design §8.1).
    if data_root_override().is_none() {
        let pointer_file = relocate::default_pointer_file(&default_data_root());
        let removal_state = state.clone();
        std::thread::spawn(move || {
            let notice = match relocate::finish_pending_removal(
                &pointer_file,
                removal_state.repository.data_root(),
            ) {
                Some(Ok(previous)) => (
                    false,
                    format!("数据已移到新位置；原目录 {} 已删除", previous.display()),
                ),
                Some(Err(e)) => (
                    true,
                    format!("数据已移到新位置，但原目录无法删除，请手动删除：{e}"),
                ),
                None => return,
            };
            removal_state
                .lifecycle
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notices
                .push(notice);
        });
    }

    application()
        .with_assets(MarinaAssets)
        // The window hides to the tray, so only the exit flow quits.
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            init(cx);
            Theme::change(ThemeMode::Light, None, cx);

            cx.set_global(MarinaApp {
                state: state.clone(),
                hwnd: None,
                exiting: false,
                window: None,
                tray_menu: Vec::new(),
            });
            cx.on_action(show_main_window)
                .on_action(quit_app)
                .on_action(handle_tray_command);

            let state_clone = state.clone();
            let tray_error_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let tray_error_clone = tray_error_holder.clone();

            match open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1240.0), px(820.0)),
                        cx,
                    ))),
                    // The window has no OS caption: `render_titlebar` draws the
                    // whole bar, so no system hover tooltip or shadow appears.
                    titlebar: Some(TitlebarOptions {
                        title: Some("DSH Marina".into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let hwnd = raw_hwnd(window);
                    cx.global_mut::<MarinaApp>().hwnd = hwnd;
                    // Auto-start passes --background: stay in the tray.
                    if start_hidden && let Some(hwnd) = hwnd {
                        platform::hide_window(hwnd);
                    }
                    window.on_window_should_close(cx, |_, cx| handle_window_close(cx));

                    let view = cx.new(|cx| {
                        let mut view = MarinaWindow::new(state_clone, window, cx);
                        let tray_err = tray_error_clone
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        if let Some(error) = tray_err {
                            view.last_error = Some(format!("系统托盘不可用：{error}"));
                        }
                        view
                    });
                    cx.global_mut::<MarinaApp>().window = Some(view.downgrade());

                    // Defer tray creation to the next event loop cycle to ensure
                    // the window message loop is fully operational
                    let tray_err_holder = tray_error_holder.clone();

                    // 总是创建托盘图标
                    cx.defer(move |cx| {
                        dsh_marina::applog::info("窗口事件循环就绪，开始创建系统托盘…");
                        if let Err(e) = create_tray(cx) {
                            let error_msg = format!("{e:#}");
                            dsh_marina::applog::error(&format!("创建托盘图标失败：{error_msg}"));
                            *tray_err_holder
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error_msg);
                        } else {
                            dsh_marina::applog::info("托盘图标创建成功");
                        }
                    });

                    view
                },
            ) {
                Ok(_) => {}
                Err(e) => {
                    dsh_marina::applog::error(&format!("打开窗口失败：{e:?}"));
                    cx.quit();
                }
            }
        });
}
