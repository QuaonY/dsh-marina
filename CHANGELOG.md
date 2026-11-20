# Changelog / 更新日志

All notable changes to DSH Marina are recorded here, newest first. Each release appends one
section; the release pipeline extracts the matching section as the Release body and update notes.

每个版本的变更都记录在此，最新在前。每次发版追加一节；发布流水线自动抽取对应版本章节作为
Release 说明与更新弹窗内容。

## 0.1.4

This release makes deleted-instance data recoverable, steadies the frame rate, and refreshes the look.

- Restorable trash: deleting with "keep data" now keeps a recoverable copy for 30 days. A new "Trash" entry at the bottom of the sidebar (with a count badge) opens a dedicated page where kept instances can be restored or permanently deleted. Restore rebuilds the instance with its home data and configuration.
- Steadier frame rate: per-frame and periodic overhead was trimmed, markedly reducing stutter and large frame-rate swings across all screens.
- The sidebar memory gauge now shows real whole-system memory usage (used / total) instead of only the running-instance sum, so it no longer reads a misleading 0 when idle; a sub-line shows how much of that is your instances.
- Refined light and dark palettes for better card depth and text contrast, improving overall display quality.

中文

本版让「保留数据后删除」的实例可以恢复，显著稳定帧率，并微调了整体观感。

- 可恢复的回收站：选择「保留数据后删除」的实例会保留 30 天可恢复副本。侧边栏底部新增「回收站」入口（带数量徽标），打开独立页面可恢复或彻底删除。恢复会重建实例并放回 home 数据与配置。
- 帧率更稳：并精简每帧与周期性开销，各界面的卡顿与帧率大幅波动明显减少。
- 侧边栏内存栏改为显示整机真实内存占用（已用 / 总量），不再只统计运行中实例、空闲时误显 0；下方小字标出其中实例占用。
- 微调浅色 / 深色配色，提升卡片层次与文字对比度，优化显示效果。

## 0.1.3

This release adds a home dashboard, batch controls and download pause/resume.

- Home dashboard: per-instance color bar, status dot, memory sparkline and live usage for running instances.
- Home search box (filter by name) plus "Start all / Stop all" batch actions.
- Global shortcuts: Ctrl/Cmd+N new instance, Ctrl/Cmd+, open settings.
- Downloads support pause / resume: pausing keeps the partial file for resuming; only cancel cleans up.
- Home empty-state guidance (brand mark + new-instance button).
- Network clients reused per timeout; monitor polling adapts (idle 6s / active 2s).
- Theme palette split into its own module.
- Cancelling an unfinished download no longer leaves a broken instance in the list.

中文

本版新增首页仪表盘、批量操作与下载暂停/继续。

- 首页仪表盘：实例色条、状态点、运行中实例的内存迷你曲线与实时占用。
- 首页搜索框（按名称过滤）与「全部启动 / 全部停止」批量操作。
- 全局快捷键：Ctrl/Cmd+N 新建实例，Ctrl/Cmd+, 打开设置。
- 下载支持「暂停 / 继续」：暂停保留已下载部分可续传，取消才清理未完成文件与待装实例。
- 首页空状态引导（品牌标识 + 新建实例按钮）。
- 网络客户端按超时复用；监控轮询自适应降频（空闲 6s / 活跃 2s）。
- 主题色板拆分为独立模块，便于维护。
- 取消未完成的下载不再在实例列表留下损坏实例。

## 0.1.2

This release turns "check for updates" into a real automatic updater and ships an NSIS installer.

- Automatic update check on every launch; a red dot on the bell when a new version exists.
- Update dialog shows current → new version, the release notes and a GitHub link.
- "Download and update" shows progress, verifies the published SHA-256, then offers "Restart and update" or "Later".
- With instances running, "Restart and update" warns first; "Update on exit" defers installation until Marina quits.
- Settings > General gains an "auto check for updates" toggle.
- NSIS installer: choose install dir, desktop shortcut and Start Menu; per-user installs need no admin rights.

中文

本版把「检查更新」改成真正的自动更新，并提供 NSIS 安装包。

- 每次启动自动检查更新；发现新版本时消息铃铛亮红点。
- 更新弹窗显示当前版本 → 新版本、更新内容与 GitHub 链接。
- 「下载并更新」显示进度，下载完成后做 SHA-256 校验，再提供「重启并更新」和「稍后更新」。
- 有实例运行时点「重启并更新」会先提示；可选「退出时再更新」，退出 Marina 时再安装。
- 「设置 > 通用」新增「自动检查更新」开关。
- NSIS 安装包：可选安装目录、桌面快捷方式与开始菜单；装到用户目录无需管理员权限。