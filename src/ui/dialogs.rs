//! Dialogs and toasts.

use super::*;
use crate::ShareStepStatus;
use dsh_marina::settings::DEFAULT_PACK_CATALOG_URL;
use dsh_marina::storage::DeleteMode;
use std::path::PathBuf;

fn dialog_shell(title: &str, title_color: u32, width: f32, body: Div, footer: Div) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(OVERLAY))
        .occlude()
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(width))
                .max_h(relative(0.92))
                .rounded(px(18.))
                .bg(rgb(BG_CARD))
                .border_1()
                .border_color(rgb(BORDER_SUBTLE))
                .shadow_lg()
                .overflow_hidden()
                .child(
                    div()
                        .px(px(28.))
                        .pt(px(24.))
                        .pb(px(12.))
                        .text_size(px(19.))
                        .font_weight(FontWeight::EXTRA_BOLD)
                        .text_color(rgb(title_color))
                        .child(dsh_marina::i18n::tr(title)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scrollbar() // 添加滚动条
                        .child(body.px(px(28.)).py(px(12.))),
                )
                .child(
                    footer
                        .flex()
                        .flex_shrink_0()
                        .justify_end()
                        .gap(px(10.))
                        .px(px(28.))
                        .pt(px(18.))
                        .pb(px(24.)),
                ),
        )
}

fn field_label(text: &str) -> Div {
    div()
        .mb(px(8.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(TEXT_2))
        .child(text.to_owned())
}

fn body() -> Div {
    div().flex().flex_col().gap(px(14.))
}

fn paragraph(text: impl Into<SharedString>) -> Div {
    div().text_color(rgb(TEXT_2)).child(text.into())
}

/// Radio-style option card.
fn option_card(title: &str, desc: &str, selected: bool) -> Hoverable {
    Hoverable::new(
        div()
            .flex()
            .items_start()
            .gap(px(13.))
            .px(px(16.))
            .py(px(14.))
            .rounded(px(11.))
            .border_1()
            .when(selected, |el| {
                el.border_color(rgb(PRIMARY)).bg(rgba(PRIMARY_TINT_08))
            })
            .when(!selected, |el| {
                el.border_color(rgb(BORDER)).bg(rgb(BG_ELEVATED))
            })
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .mt(px(2.))
                    .size(px(18.))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(if selected { PRIMARY } else { BORDER }))
                    .when(selected, |el| {
                        el.child(div().size(px(8.)).rounded_full().bg(rgb(PRIMARY)))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(3.))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(dsh_marina::i18n::tr(title)),
                    )
                    .when(!desc.is_empty(), |el| {
                        el.child(hint(desc.to_owned()).text_size(px(12.5)))
                    }),
            ),
        move |s| {
            if selected { s } else { s.bg(rgb(BG_CARD)) }
        },
    )
}

fn error_text(text: String) -> Div {
    div().text_color(rgb(DANGER)).child(text)
}

impl MarinaWindow {
    fn cancel_button(&self, cx: &mut Context<Self>) -> Div {
        clickable(
            btn("取消", Tone::Default, false),
            true,
            cx.listener(|this, _, _, cx| {
                this.dialog = None;
                this.share_draft = None;
                cx.notify();
            }),
        )
    }

    fn instance_name(&self, id: &str) -> String {
        self.instances
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Progress overlay for a running copy; only "取消" closes it early.
    fn render_copy_job(&self, job: &CopyJob, cx: &mut Context<Self>) -> Div {
        let (done, total) = job.progress.snapshot();
        let cancelling = job.progress.is_cancelled();
        let measured = job.progress.is_measured();
        let ratio = if measured && total > 0 {
            (done as f32 / total as f32).clamp(0., 1.)
        } else {
            0.
        };
        let status = if cancelling {
            "正在取消并清理已复制的内容…".to_owned()
        } else if !measured {
            format!("正在统计文件大小… {}", storage::format_bytes(total))
        } else {
            format!(
                "{} / {}（{:.0}%）",
                storage::format_bytes(done),
                storage::format_bytes(total),
                ratio * 100.
            )
        };
        dialog_shell(
            &job.title,
            TEXT,
            520.,
            body()
                .child(paragraph(status))
                .child(
                    div()
                        .h(px(8.))
                        .w_full()
                        .rounded(px(4.))
                        .bg(rgb(BG_ELEVATED))
                        .child(
                            div()
                                .h_full()
                                .rounded(px(4.))
                                .bg(rgb(PRIMARY))
                                .w(relative(ratio)),
                        ),
                )
                .child(hint("取消后会删除已复制的部分，原实例不受影响。")),
            div().child(clickable(
                btn(
                    if cancelling {
                        "正在取消…"
                    } else {
                        "取消"
                    },
                    Tone::Default,
                    false,
                ),
                !cancelling,
                cx.listener(|this, _, _, cx| this.cancel_copy_job(cx)),
            )),
        )
    }

    pub(super) fn render_dialog(&self, cx: &mut Context<Self>) -> Option<Div> {
        if let Some(job) = &self.copy_job {
            return Some(self.render_copy_job(job, cx));
        }
        let dialog = match self.dialog.as_ref()? {
            Dialog::NewInstance(dialog) => self.render_new_dialog(dialog, cx),
            Dialog::CreateSnapshot { reason, .. } => {
                let reason = reason.clone();
                let submit = clickable(
                    btn("创建", Tone::Primary, false),
                    true,
                    cx.listener(|this, _, _, cx| this.submit_snapshot_dialog(cx)),
                );
                dialog_shell(
                    "创建快照",
                    TEXT,
                    520.,
                    body()
                        .child(paragraph("手动保存一份当前的插件配置，用于以后回滚。"))
                        .child(div().child(field_label("快照原因")).child(Input::new(&reason))),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::RestoreSnapshot { instance_id, snapshot_id } => {
                let instance_id = instance_id.clone();
                let snapshot_id = snapshot_id.clone();
                let text = format!(
                    "即将把「{}」的插件配置回滚到快照 {}。回滚前的配置会自动另存为一份安全快照。",
                    self.instance_name(&instance_id),
                    snapshot_id
                );
                let submit = clickable(
                    btn("回滚", Tone::Primary, false),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = None;
                        this.restore_snapshot(&instance_id, &snapshot_id, cx);
                    }),
                );
                dialog_shell(
                    "回滚到快照",
                    TEXT,
                    520.,
                    body().child(paragraph(text)),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::DeleteSnapshot { instance_id, snapshot_id } => {
                let instance_id = instance_id.clone();
                let snapshot_id = snapshot_id.clone();
                let text = format!("确定要删除快照 {snapshot_id} 吗？此操作无法撤销。");
                let submit = clickable(
                    btn("删除", Tone::Danger, false),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = None;
                        this.delete_snapshot(&instance_id, &snapshot_id, cx);
                    }),
                );
                dialog_shell(
                    "删除快照",
                    DANGER,
                    520.,
                    body().child(paragraph(text)),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::RenameInstance { name, .. } => {
                let name = name.clone();
                let submit = clickable(
                    btn("保存", Tone::Primary, false),
                    true,
                    cx.listener(|this, _, _, cx| this.submit_rename_dialog(cx)),
                );
                dialog_shell(
                    "重命名实例",
                    TEXT,
                    520.,
                    body()
                        .child(paragraph("名称只影响启动器里的显示，不会改动实例目录。"))
                        .child(div().child(field_label("实例名称")).child(Input::new(&name))),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::Appearance { color, icon, notes, error, .. } => {
                let current = color.clone();
                let swatches = div().flex().gap(px(10.)).children(super::BADGE_COLORS.iter().map(|value| {
                    let value = value.to_string();
                    let element_id = format!("swatch-{value}");
                    let selected = value.eq_ignore_ascii_case(&current);
                    clickable(
                        div()
                            .size(px(30.))
                            .rounded(px(8.))
                            .bg(rgb(super::hex_color(&value)))
                            .border_2()
                            .border_color(if selected { rgb(TEXT) } else { rgb(BG_CARD) }),
                        !selected,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(Dialog::Appearance { color, .. }) = this.dialog.as_mut() {
                                *color = value.clone();
                                cx.notify();
                            }
                        }),
                    )
                    .id(SharedString::from(element_id))
                }));
                dialog_shell(
                    "图标、颜色与备注",
                    TEXT,
                    540.,
                    body()
                        .child(div().child(field_label("颜色")).child(swatches))
                        .child(div().child(field_label("图标（1 个字或 emoji）")).child(Input::new(icon)))
                        .child(div().child(field_label("备注")).child(Input::new(notes)))
                        .when_some(error.clone(), |el, e| el.child(error_text(e))),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("保存", Tone::Primary, false),
                        true,
                        cx.listener(|this, _, _, cx| this.submit_appearance(cx)),
                    )),
                )
            }
            Dialog::CloneInstance { instance_id, name, personal } => {
                let instance_id = instance_id.clone();
                let name = name.clone();
                // Sessions and credentials of a running instance are being
                // written, so copying them can capture half a file.
                let running = is_running(&self.get_instance_status(&instance_id));
                let personal = *personal && !running;
                let source = self.instance_name(&instance_id);
                let submit = clickable(
                    btn("复制", Tone::Primary, false),
                    true,
                    cx.listener(|this, _, _, cx| this.submit_clone_dialog(cx)),
                );
                dialog_shell(
                    "复制为新实例",
                    TEXT,
                    520.,
                    body()
                        .child(paragraph(format!("复制「{source}」的插件组合和配置到一个新实例。")))
                        .child(div().child(field_label("新实例名称")).child(Input::new(&name)))
                        .child(div().flex().flex_col().gap(px(12.)).children([
                            clickable(
                                option_card("只复制插件和配置（推荐）", "不复制会话和登录凭据", !personal),
                                true,
                                cx.listener(|this, _, _, cx| this.set_clone_personal(false, cx)),
                            ),
                            clickable(
                                option_card(
                                    "同时复制个人数据",
                                    if running {
                                        "实例正在运行，停止后才能复制"
                                    } else {
                                        "会话、登录凭据也一并复制"
                                    },
                                    personal,
                                ),
                                !running,
                                cx.listener(|this, _, _, cx| this.set_clone_personal(true, cx)),
                            ),
                        ]))
                        .when(running, |el| {
                            el.child(hint(format!(
                                "「{source}」正在运行，会话和登录凭据可能正在写入，停止后才能一并复制。"
                            )))
                        }),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::Compat { instance_id, previous, next } => {
                let (clone_id, force_id) = (instance_id.clone(), instance_id.clone());
                let text = format!(
                    "「{}」上次在 DSH {previous} 上正常运行，这次要用 DSH {next} 启动。大版本变化可能让插件或配置不兼容，建议先复制一份再试。",
                    self.instance_name(instance_id)
                );
                dialog_shell(
                    "DSH 版本变化较大",
                    TEXT,
                    560.,
                    body().child(paragraph(text)),
                    div()
                        .child(self.cancel_button(cx))
                        .child(clickable(
                            btn("继续启动", Tone::Default, false),
                            true,
                            cx.listener(move |this, _, _, cx| {
                                this.dialog = None;
                                this.start_instance(force_id.clone(), true, cx);
                            }),
                        ))
                        .child(clickable(
                            btn("复制后再启动", Tone::Primary, false),
                            true,
                            cx.listener(move |this, _, _, cx| {
                                this.dialog = None;
                                this.clone_and_start(clone_id.clone(), cx);
                            }),
                        )),
                )
            }
            Dialog::UninstallPlugin { instance_id, package } => {
                let (instance_id, package) = (instance_id.clone(), package.clone());
                let text = format!(
                    "即将从「{}」卸载 {package}。卸载前会自动保存一份快照，可以随时回滚。",
                    self.instance_name(&instance_id)
                );
                let submit = clickable(
                    btn("卸载", Tone::Danger, false),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = None;
                        this.uninstall_plugin(instance_id.clone(), package.clone(), cx);
                    }),
                );
                dialog_shell(
                    "卸载插件",
                    DANGER,
                    520.,
                    body().child(paragraph(text)),
                    div().child(self.cancel_button(cx)).child(submit),
                )
            }
            Dialog::CopyPlugins { source_id, target } => {
                self.render_copy_plugins_dialog(source_id.clone(), target.clone(), cx)
            }
            Dialog::MarketInstall { index, target } => self.render_market_install_dialog(*index, target.clone(), cx)?,
            Dialog::Exit { running } => dialog_shell(
                "退出 DSH Marina",
                TEXT,
                560.,
                body().child(paragraph(format!(
                    "还有 {running} 个实例正在运行，退出启动器会结束它们。「先停止实例再退出」会依次请求正常退出；「强制退出」立即结束所有进程，可能丢失未保存的会话数据。"
                ))),
                div()
                    .child(self.cancel_button(cx))
                    .child(clickable(
                        btn("强制退出", Tone::Danger, false),
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            this.last_success = Some("正在结束所有实例…".to_owned());
                            cx.notify();
                            crate::perform_quit(crate::StopMode::Force, None, cx);
                        }),
                    ))
                    .child(clickable(
                        btn("先停止实例再退出", Tone::Primary, false),
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            this.last_success = Some("正在停止实例，完成后退出…".to_owned());
                            cx.notify();
                            crate::perform_quit(crate::StopMode::Graceful, None, cx);
                        }),
                    )),
            ),
            Dialog::UninstallMarina { mode, stats } => self.render_uninstall_marina(*mode, stats.as_ref(), cx),
            Dialog::UninstallExternal { delete_data } => {
                let delete_data = *delete_data;
                let options = [
                    (false, "只卸载程序（推荐）", "保留 ~/.dsh 和 %APPDATA% 里的数据，以后重装还能用"),
                    (true, "卸载并删除数据", "同时删除原版的插件、会话和登录凭据，无法恢复"),
                ]
                .into_iter()
                .map(|(value, title, desc)| {
                    clickable(
                        option_card(title, desc, value == delete_data),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(Dialog::UninstallExternal { delete_data }) = this.dialog.as_mut() {
                                *delete_data = value;
                                cx.notify();
                            }
                        }),
                    )
                });
                dialog_shell(
                    "卸载原版 DSH",
                    DANGER,
                    580.,
                    body()
                        .child(error_text(
                            "这是你的原版 DSH（安装在 %LOCALAPPDATA%\\Programs\\DeepSeek Harness），删除后系统里将不再有 DSH，需要重新安装才能在外面打开。请先完全退出原版 DSH。"
                                .to_owned(),
                        ))
                        .child(div().flex().flex_col().gap(px(12.)).children(options)),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("确认卸载", Tone::Danger, false),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = None;
                            this.uninstall_external(delete_data, cx);
                        }),
                    )),
                )
            }
            Dialog::RebindRuntime { runtime_id, version, users, targets, selected } => {
                let runtime_id = runtime_id.clone();
                let binding = targets.get(*selected).map(|(b, _)| b.clone());
                let options = targets.iter().enumerate().map(|(index, (_, label))| {
                    clickable(
                        option_card(label, "", index == *selected),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(Dialog::RebindRuntime { selected, .. }) = this.dialog.as_mut() {
                                *selected = index;
                                cx.notify();
                            }
                        }),
                    )
                });
                dialog_shell(
                    &format!("改绑后删除 DSH {version}"),
                    DANGER,
                    560.,
                    body()
                        .child(paragraph(format!(
                            "这些实例还在用 DSH {version}：{}。先把它们改到下面选的版本，再把 DSH {version} 移到回收站。跟随「最新稳定版」的实例会自动换到下一个最新版本。",
                            users.join("、")
                        )))
                        .child(div().flex().flex_col().gap(px(8.)).children(options)),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("改绑并删除", Tone::Danger, false),
                        binding.is_some(),
                        cx.listener(move |this, _, _, cx| {
                            if let Some(binding) = binding.clone() {
                                this.dialog = None;
                                this.rebind_runtime(runtime_id.clone(), binding, cx);
                            }
                        }),
                    )),
                )
            }
            Dialog::DeleteRuntime { runtime_id, version } => {
                let runtime_id = runtime_id.clone();
                dialog_shell(
                    "删除 DSH 版本",
                    DANGER,
                    520.,
                    body().child(paragraph(format!(
                        "即将删除 DSH {version}。没有实例在使用它，文件会移到系统回收站。"
                    ))),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("删除", Tone::Danger, false),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = None;
                            this.delete_runtime(runtime_id.clone(), cx);
                        }),
                    )),
                )
            }
            Dialog::MoveData { target } => {
                let target = target.clone();
                let current = self.state.repository.data_root().to_path_buf();
                let size = storage::format_bytes(storage::directory_size(&current));
                dialog_shell(
                    "移动数据目录",
                    TEXT,
                    580.,
                    body()
                        .child(paragraph(format!(
                            "把全部实例、DSH 版本和设置（约 {size}）复制到新位置，校验无误后 Marina 会自动重启并删除旧目录。复制期间可以取消。"
                        )))
                        .child(hint("当前位置"))
                        .child(path_box(&current))
                        .child(hint("新位置"))
                        .child(path_box(&target))
                        .child(hint("需要先停止所有实例。原版 DSH 的数据不受影响。")),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("开始移动", Tone::Primary, false),
                        true,
                        cx.listener(move |this, _, _, cx| this.move_data(target.clone(), cx)),
                    )),
                )
            }
            Dialog::OpenLink { url, input, selected, remember, error } => {
                let picker = div().flex().flex_col().gap(px(8.)).children(self.instances.iter().map(|instance| {
                    let id = instance.id.clone();
                    let status = self.get_instance_status(&instance.id);
                    let desc = if is_running(&status) { "运行中，链接会交给这个窗口" } else { "已停止，会带着链接启动" };
                    clickable(
                        option_card(&instance.name, desc, selected.as_deref() == Some(instance.id.as_str())),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(Dialog::OpenLink { selected, .. }) = this.dialog.as_mut() {
                                *selected = Some(id.clone());
                                cx.notify();
                            }
                        }),
                    )
                }));
                let remember_row = div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(clickable(
                        switch(*remember),
                        true,
                        cx.listener(|this, _, _, cx| {
                            if let Some(Dialog::OpenLink { remember, .. }) = this.dialog.as_mut() {
                                *remember = !*remember;
                                cx.notify();
                            }
                        }),
                    ))
                    .child(hint("以后直接用这个实例打开，不再询问"));
                let link = match input {
                    Some(input) => div().child(field_label("链接")).child(Input::new(input)),
                    None => div().child(field_label("链接")).child(
                        div()
                            .p(px(10.))
                            .rounded(px(8.))
                            .bg(rgb(BG_ELEVATED))
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(url.clone()),
                    ),
                };
                dialog_shell(
                    "用哪个实例打开链接",
                    TEXT,
                    560.,
                    body()
                        .child(link)
                        .when(self.instances.is_empty(), |el| el.child(paragraph("还没有实例，请先新建一个。")))
                        .child(picker)
                        .child(remember_row)
                        .when_some(error.clone(), |el, e| el.child(error_text(e))),
                    div().child(self.cancel_button(cx)).child(clickable(
                        btn("打开", Tone::Primary, false),
                        selected.is_some(),
                        cx.listener(|this, _, _, cx| this.submit_open_link(cx)),
                    )),
                )
            }
            Dialog::Bisect { instance_id, bisect, busy, .. } => {
                let name = self.instance_name_of(instance_id);
                let busy = *busy;
                dialog_shell(
                    &format!("二分排查「{name}」· 第 {} 轮", bisect.round),
                    TEXT,
                    580.,
                    body()
                        .child(paragraph(format!(
                            "还剩 {} 个可疑插件。这一轮临时禁用了 {} 个：{}。",
                            bisect.candidates.len(),
                            bisect.disabled.len(),
                            bisect.disabled.join("、")
                        )))
                        .child(paragraph(if busy {
                            "正在重启实例，请稍候…".to_owned()
                        } else {
                            "实例已重新启动。请在 DSH 窗口里试一下原来出问题的操作，然后告诉 Marina 结果。".to_owned()
                        }))
                        .child(hint("排查前已自动创建快照；退出排查会恢复全部插件。")),
                    div()
                        .child(clickable(
                            btn("退出排查", Tone::Default, false),
                            !busy,
                            cx.listener(|this, _, _, cx| this.cancel_bisect(cx)),
                        ))
                        .child(clickable(
                            btn("仍然失败", Tone::Danger, false),
                            !busy,
                            cx.listener(|this, _, _, cx| this.report_bisect(true, cx)),
                        ))
                        .child(clickable(
                            btn("现在正常了", Tone::Primary, false),
                            !busy,
                            cx.listener(|this, _, _, cx| this.report_bisect(false, cx)),
                        )),
                )
            }
            Dialog::CleanupRuntimes { versions, max } => {
                let ids: Vec<String> = versions.iter().map(|(id, _)| id.clone()).collect();
                let names: Vec<String> = versions.iter().map(|(_, v)| format!("DSH {v}")).collect();
                dialog_shell(
                    "清理旧版本",
                    TEXT,
                    520.,
                    body().child(paragraph(format!(
                        "已托管的 DSH 版本超过了设置的上限（{max} 个）。下面这些旧版本没有实例在用，可以移到回收站：{}。",
                        names.join("、")
                    ))),
                    div().child(clickable(
                        btn("以后再说", Tone::Default, false),
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        }),
                    ))
                    .child(clickable(
                        btn("清理", Tone::Primary, false),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = None;
                            this.cleanup_runtimes(ids.clone(), cx);
                        }),
                    )),
                )
            }
            Dialog::ExportScan { instance_id, findings } => {
                let (redacted_id, raw_id) = (instance_id.clone(), instance_id.clone());
                let rows = findings.iter().take(12).map(|finding| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .py(px(6.))
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(chip(finding.kind.label(), ChipTone::Warn))
                                .child(hint(format!("{}:{}", finding.file, finding.line))),
                        )
                        .child(hint(finding.excerpt.clone()).font_family(MONO).truncate())
                });
                let more = findings.len().saturating_sub(12);
                dialog_shell(
                    "导出前发现敏感内容",
                    TEXT,
                    600.,
                    body()
                        .child(paragraph(format!(
                            "整合包里有 {} 处可能是口令或本机路径的内容。推荐脱敏导出：口令换成 ***，用户目录换成 %USERPROFILE%。导入的人需要自己重新填写口令。",
                            findings.len()
                        )))
                        .child(div().flex().flex_col().children(rows))
                        .when(more > 0, |el| el.child(hint(format!("还有 {more} 处未列出")))),
                    div()
                        .child(self.cancel_button(cx))
                        .child(clickable(
                            btn("原样导出", Tone::Default, false),
                            true,
                            cx.listener(move |this, _, _, cx| {
                                this.dialog = None;
                                this.save_export(raw_id.clone(), false, cx);
                            }),
                        ))
                        .child(clickable(
                            btn("脱敏导出（推荐）", Tone::Primary, false),
                            true,
                            cx.listener(move |this, _, _, cx| {
                                this.dialog = None;
                                this.save_export(redacted_id.clone(), true, cx);
                            }),
                        )),
                )
            }
            Dialog::StopInstance { instance_id, restart, dont_ask } => {
                let (id, restart, dont_ask) = (instance_id.clone(), *restart, *dont_ask);
                let name = self.instance_name_of(&id);
                let action = if restart { "重启" } else { "停止" };
                let remember = move |this: &mut Self, cx: &mut Context<Self>| {
                    this.dialog = None;
                    if dont_ask {
                        this.update_settings(|s| s.confirm_stop = false, cx);
                    }
                };
                let force_id = id.clone();
                let dont_ask_row = div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(clickable(
                        switch(dont_ask),
                        true,
                        cx.listener(|this, _, _, cx| {
                            if let Some(Dialog::StopInstance { dont_ask, .. }) = this.dialog.as_mut() {
                                *dont_ask = !*dont_ask;
                                cx.notify();
                            }
                        }),
                    ))
                    .child(hint("以后不再提醒（可在设置里重新打开）"));
                let mut footer = div().child(self.cancel_button(cx));
                if !restart {
                    footer = footer.child(clickable(
                        btn("立即强制结束", Tone::Danger, false),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            remember(this, cx);
                            this.confirm_stop(force_id.clone(), false, true, cx);
                        }),
                    ));
                }
                footer = footer.child(clickable(
                    btn(action, Tone::Primary, false),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        remember(this, cx);
                        this.confirm_stop(id.clone(), restart, false, cx);
                    }),
                ));
                dialog_shell(
                    &format!("{action}「{name}」"),
                    TEXT,
                    540.,
                    body()
                        .child(paragraph(format!(
                            "Marina 会先请 DSH 正常关闭；{} 秒内没有退出就强制结束整个进程树，可能丢失未保存的内容（正在输入的消息、未完成的任务）。",
                            crate::STOP_GRACE.as_secs()
                        )))
                        .child(dont_ask_row),
                    footer,
                )
            }
            Dialog::SharePack { instance, name, description, mode, selected_folder, error, busy } => {
                self.render_share_pack_dialog(
                    instance.clone(),
                    name.clone(),
                    description.clone(),
                    *mode,
                    selected_folder.clone(),
                    error.clone(),
                    *busy,
                    cx,
                )
            }
            Dialog::CommunitySettings => self.render_community_settings_dialog(cx),
            Dialog::PackPlugins { name, author, description, dsh_version, plugins, size } => {
                self.render_pack_plugins_dialog(
                    name.clone(),
                    author.clone(),
                    description.clone(),
                    dsh_version.clone(),
                    plugins.clone(),
                    *size,
                    cx,
                )
            }
            Dialog::GitHubLogin { user_code, verification_uri, waiting, connecting, error } => {
                self.render_github_login_dialog(user_code, verification_uri, *waiting, *connecting, error.clone(), cx)
            }
            Dialog::ShareToCommunity { pack_name, current_step, total_steps, steps, progress, cancel, pr_url, error } => {
                self.render_share_to_community_dialog(pack_name, *current_step, *total_steps, steps, *progress, cancel.clone(), pr_url.clone(), error.clone(), cx)
            }
            Dialog::DeleteInstance { instance_id, mode } => {
                self.render_delete_dialog(instance_id.clone(), *mode, cx)
            }
        };
        Some(dialog)
    }

    fn render_uninstall_marina(
        &self,
        mode: crate::maintenance::UninstallMode,
        stats: Option<&storage::UninstallStats>,
        cx: &mut Context<Self>,
    ) -> Div {
        use crate::maintenance::UninstallMode;
        let summary = match stats {
            Some(stats) => format!(
                "将删除 {} 个实例、{} 个 DSH 版本，共 {}。原版 DSH 不受影响。",
                stats.instances,
                stats.runtimes,
                storage::format_bytes(stats.bytes)
            ),
            None => "正在统计数据大小…".to_owned(),
        };
        let options = [
            (
                UninstallMode::ExportAll,
                "导出全部实例后卸载（推荐）",
                "每个实例导出为整合包，放到桌面 DSH-Marina-Backup-<日期> 文件夹",
            ),
            (
                UninstallMode::MoveData,
                "把数据移到桌面后卸载",
                "整个数据目录原样移到桌面，以后可以手动放回",
            ),
            (
                UninstallMode::DeleteAll,
                "全部删除",
                "实例、DSH 版本和设置全部删除，无法恢复",
            ),
        ]
        .into_iter()
        .map(|(option, title, desc)| {
            clickable(
                option_card(title, desc, option == mode),
                true,
                cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::UninstallMarina { mode, .. }) = this.dialog.as_mut() {
                        *mode = option;
                        cx.notify();
                    }
                }),
            )
        });
        let running = self
            .instances
            .iter()
            .filter(|i| is_running(&self.get_instance_status(&i.id)))
            .count();
        dialog_shell(
            "卸载 DSH Marina",
            DANGER,
            600.,
            body()
                .child(paragraph(summary))
                .when(running > 0, |el| {
                    el.child(paragraph(format!(
                        "{running} 个正在运行的实例会先被正常停止。"
                    )))
                })
                .child(div().flex().flex_col().gap(px(12.)).children(options))
                .child(hint(
                    "卸载会删除 Marina 程序本身并移除开机自启动，然后退出。",
                )),
            div().child(self.cancel_button(cx)).child(clickable(
                btn("卸载并退出", Tone::Danger, false),
                true,
                cx.listener(move |this, _, _, cx| this.confirm_uninstall_marina(mode, cx)),
            )),
        )
    }

    fn set_clone_personal(&mut self, value: bool, cx: &mut Context<Self>) {
        if let Some(Dialog::CloneInstance { personal, .. }) = self.dialog.as_mut() {
            *personal = value;
            cx.notify();
        }
    }

    /// 回车提交：对话框的主输入框按 Enter 等同于点主按钮。
    pub(crate) fn submit_active_dialog(&mut self, cx: &mut Context<Self>) {
        match self.dialog {
            Some(Dialog::CreateSnapshot { .. }) => self.submit_snapshot_dialog(cx),
            Some(Dialog::RenameInstance { .. }) => self.submit_rename_dialog(cx),
            Some(Dialog::CloneInstance { .. }) => self.submit_clone_dialog(cx),
            Some(Dialog::Appearance { .. }) => self.submit_appearance(cx),
            Some(Dialog::NewInstance(_)) => self.submit_new_dialog(cx),
            Some(Dialog::SharePack { .. }) => self.submit_share_pack(cx),
            _ => {}
        }
    }

    pub(crate) fn submit_snapshot_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::CreateSnapshot {
            instance_id,
            reason,
        }) = self.dialog.as_ref()
        else {
            return;
        };
        let (instance_id, reason) = (instance_id.clone(), reason.clone());
        let text = reason.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.dialog = None;
        self.create_snapshot(&instance_id, &text, cx);
    }

    pub(crate) fn submit_rename_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::RenameInstance { instance_id, name }) = self.dialog.as_ref() else {
            return;
        };
        let (instance_id, name) = (instance_id.clone(), name.clone());
        let text = name.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.dialog = None;
        self.rename_instance(&instance_id, &text, cx);
    }

    pub(crate) fn submit_clone_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::CloneInstance {
            instance_id,
            name,
            personal,
        }) = self.dialog.as_ref()
        else {
            return;
        };
        let (instance_id, name, personal) = (instance_id.clone(), name.clone(), *personal);
        let text = name.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let personal = personal && !is_running(&self.get_instance_status(&instance_id));
        self.dialog = None;
        self.clone_instance(&instance_id, &text, personal, cx);
    }

    fn render_market_install_dialog(
        &self,
        index: usize,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let plugin = self.market_plugin(index)?.clone();
        let options = self.instances.iter().map(|instance| {
            let id = instance.id.clone();
            let status = self.get_instance_status(&instance.id);
            let stopped = !is_running(&status) && !is_busy(&status);
            let installed = self
                .market
                .installed
                .get(&instance.id)
                .is_some_and(|n| n.contains(&plugin.name));
            let version = self.instance_dsh_version(instance);
            let compatible = version.as_deref().is_none_or(|v| plugin.supports(v));
            let mut desc = match &version {
                Some(v) => format!("DSH {v}"),
                None => "DSH 版本未安装".to_owned(),
            };
            if installed {
                desc.push_str(" · 已装过，再装会更新到这个版本");
            }
            if !compatible {
                desc.push_str(&format!(" · 插件要求 DSH {}，可能不兼容", plugin.dsh_range));
            }
            if !stopped {
                desc = "正在运行，需先停止".to_owned();
            }
            clickable(
                option_card(
                    &instance.name,
                    &desc,
                    target.as_deref() == Some(id.as_str()),
                ),
                stopped,
                cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::MarketInstall { target, .. }) = this.dialog.as_mut() {
                        *target = Some(id.clone());
                        cx.notify();
                    }
                }),
            )
        });
        let chosen = target.clone();
        let submit = clickable(
            btn("安装", Tone::Primary, false),
            chosen.is_some(),
            cx.listener(move |this, _, _, cx| {
                if let Some(target) = chosen.clone() {
                    this.dialog = None;
                    this.install_market_plugin(index, target, cx);
                }
            }),
        );
        let list = if self.instances.is_empty() {
            div().child(paragraph("还没有实例，先新建一个。"))
        } else {
            div().child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .max_h(px(320.))
                    .children(options)
                    .overflow_y_scrollbar(),
            )
        };
        // What the community list's static scan found in the plugin's code.
        let scan = plugin
            .stats
            .as_ref()
            .filter(|s| !s.capabilities.is_empty() || !s.red_lines.is_empty())
            .map(|stats| {
                let mut capabilities = stats.capabilities.clone();
                capabilities.sort_by_key(|c| !dsh_marina::market::is_sensitive_capability(c));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(field_label("代码会用到"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .children(capabilities.iter().map(|c| {
                                chip(
                                    dsh_marina::market::capability_label(c).to_owned(),
                                    if dsh_marina::market::is_sensitive_capability(c) {
                                        ChipTone::Warn
                                    } else {
                                        ChipTone::Off
                                    },
                                )
                            })),
                    )
                    .children(stats.red_lines.iter().take(4).map(|line| {
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(DANGER))
                            .child(format!("⚠ {}", dsh_marina::market::red_line_label(line)))
                    }))
                    .child(
                        hint("以上来自社区列表对插件代码的自动扫描，仅供参考，不代表插件有恶意。")
                            .text_size(px(11.)),
                    )
            });
        Some(dialog_shell(
            &format!("安装 {}", plugin.short_title()),
            TEXT,
            560.,
            body()
                .child(paragraph(format!(
                    "{} v{}（npm 包 {}）会装进你选的实例，只影响那一个实例。装之前会自动存一份快照，启动失败可以一键回滚。",
                    plugin.short_title(),
                    plugin.version,
                    plugin.name
                )))
                .children(scan)
                .child(div().child(field_label("装到哪个实例")).child(div().child(list)))
                .child(hint("实例需要先启动过一次（让 DSH 完成初始化），并处于停止状态。安装要从 npm 下载，可能需要一两分钟。")),
            div().child(self.cancel_button(cx)).child(submit),
        ))
    }

    fn render_copy_plugins_dialog(
        &self,
        source_id: String,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let source = self.instance_name(&source_id);
        // The source profile is read while the app may be rewriting it.
        let source_running = is_running(&self.get_instance_status(&source_id));
        let candidates: Vec<&InstanceConfig> = self
            .instances
            .iter()
            .filter(|i| i.id != source_id)
            .collect();
        let options = candidates.iter().map(|instance| {
            let id = instance.id.clone();
            let status = self.get_instance_status(&instance.id);
            let stopped = !is_running(&status) && !is_busy(&status);
            let desc = if stopped {
                "目标实例的现有插件配置会先存为快照"
            } else {
                "正在运行，需先停止"
            };
            clickable(
                option_card(&instance.name, desc, target.as_deref() == Some(id.as_str())),
                stopped,
                cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::CopyPlugins { target, .. }) = this.dialog.as_mut() {
                        *target = Some(id.clone());
                        cx.notify();
                    }
                }),
            )
        });
        let chosen = target.clone();
        let submit = clickable(
            btn("复制", Tone::Primary, false),
            chosen.is_some() && !source_running,
            cx.listener(move |this, _, _, cx| {
                if let Some(target) = chosen.clone() {
                    this.dialog = None;
                    this.copy_plugins(source_id.clone(), target, cx);
                }
            }),
        );
        let list = if candidates.is_empty() {
            div().child(paragraph("还没有其他实例。"))
        } else {
            div().flex().flex_col().gap(px(12.)).children(options)
        };
        dialog_shell(
            "复制插件组合到…",
            TEXT,
            560.,
            body()
                .child(paragraph(format!(
                    "把「{source}」的插件列表、启用状态和本地插件包复制到另一个实例，然后重新安装依赖。"
                )))
                .when(source_running, |el| {
                    el.child(hint(format!(
                        "「{source}」正在运行，停止后才能复制插件配置。"
                    )))
                })
                .child(div().child(field_label("目标实例")).child(list)),
            div().child(self.cancel_button(cx)).child(submit),
        )
    }

    fn render_new_dialog(&self, dialog: &NewInstanceDialog, cx: &mut Context<Self>) -> Div {
        fn edit(
            this: &mut MarinaWindow,
            cx: &mut Context<MarinaWindow>,
            f: impl FnOnce(&mut NewInstanceDialog),
        ) {
            if let Some(Dialog::NewInstance(dialog)) = this.dialog.as_mut() {
                f(dialog);
                dialog.error = None;
                cx.notify();
            }
        }

        let modes = [
            (
                NewMode::Blank,
                "空白实例",
                "全新的 DSH，没有插件，自己选版本",
                true,
            ),
            (
                NewMode::Clone,
                "复制已有实例",
                "插件、配置和 DSH 版本原样复制",
                !self.instances.is_empty(),
            ),
            (
                NewMode::Import,
                "导入整合包",
                "用别的电脑导出的 .zip 创建",
                true,
            ),
        ];
        let mode_row = div().flex().gap(px(12.)).children(modes.into_iter().map(
            |(mode, title, desc, enabled)| {
                div().flex_1().min_w(px(0.)).child(clickable(
                    option_card(title, desc, dialog.mode == mode),
                    enabled && !dialog.busy,
                    cx.listener(move |this, _, _, cx| edit(this, cx, |d| d.mode = mode)),
                ))
            },
        ));

        let section = |title: &str, right: Option<Div>| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .mb(px(8.))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(TEXT_2))
                        .child(tr(title)),
                )
                .when_some(right, |el, right| el.child(right))
        };

        let detail = match dialog.mode {
            NewMode::Import => {
                let picker = clickable(
                    btn(
                        if dialog.import_file.is_some() {
                            "换一个文件…"
                        } else {
                            "选择整合包…"
                        },
                        Tone::Default,
                        true,
                    ),
                    !dialog.busy,
                    cx.listener(|this, _, window, cx| this.pick_import_file(window, cx)),
                );
                let summary = match &dialog.import_file {
                    None => div().child(hint(
                        "在另一台电脑上用「实例 → 设置 → 导出实例…」得到的 .zip 文件。",
                    )),
                    Some((path, Err(e))) => div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(px(12.))
                                .truncate()
                                .child(path.display().to_string()),
                        )
                        .child(error_text(format!("不是有效的整合包：{e}"))),
                    Some((path, Ok(manifest))) => {
                        let enabled = manifest.plugins.iter().filter(|p| p.enabled).count();
                        let installed = !manifest.dsh_version.is_empty()
                            && self
                                .state
                                .registry
                                .scan()
                                .unwrap_or_default()
                                .iter()
                                .any(|r| r.version == manifest.dsh_version);
                        let version_note = if manifest.dsh_version.is_empty() {
                            "跟随最新稳定版".to_owned()
                        } else if installed {
                            format!("DSH {}（本机已有）", manifest.dsh_version)
                        } else {
                            format!("DSH {}（本机没有，导入后自动下载）", manifest.dsh_version)
                        };
                        let names: Vec<String> =
                            manifest.plugins.iter().map(|p| p.name.clone()).collect();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(12.))
                            .rounded(px(10.))
                            .bg(rgb(BG_ELEVATED))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(manifest.name.clone()),
                            )
                            .child(hint(format!(
                                "{} 个插件（{} 个启用）· {} · 导出于 {}",
                                manifest.plugins.len(),
                                enabled,
                                version_note,
                                fmt_rfc3339(&manifest.created_at)
                            )))
                            .when(!names.is_empty(), |el| {
                                el.child(hint(names.join("、")).font_family(MONO).line_clamp(2))
                            })
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.))
                                    .text_color(rgb(TEXT_3))
                                    .truncate()
                                    .child(path.display().to_string()),
                            )
                    }
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().child(section("整合包", Some(div().child(picker)))).child(summary))
                    .child(hint("整合包只含插件配置和本地插件包，不含会话和登录信息。导入后会按包里的清单安装插件，需要联网。"))
            }
            NewMode::Clone => {
                let sources = self.instances.iter().map(|instance| {
                    let id = instance.id.clone();
                    let selected = dialog.clone_source.as_deref() == Some(instance.id.as_str());
                    clickable(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .pl(px(6.))
                            .pr(px(12.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(rgb(if selected { PRIMARY } else { BORDER_SUBTLE }))
                            .bg(rgb(if selected { PRIMARY_MUTED } else { BG_CARD }))
                            .child(badge(
                                &badge_text(instance),
                                hex_color(&instance.color),
                                22.,
                            ))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(if selected { PRIMARY } else { TEXT }))
                                    .child(instance.name.clone()),
                            ),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            let id = id.clone();
                            edit(this, cx, move |d| d.clone_source = Some(id));
                        }),
                    )
                });
                let personal = dialog.personal;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(
                        div()
                            .child(section("复制哪个实例", None))
                            .child(div().flex().flex_wrap().gap(px(8.)).children(sources)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .p(px(12.))
                            .rounded(px(10.))
                            .bg(rgb(BG_ELEVATED))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("同时复制个人数据"),
                                    )
                                    .child(hint("会话和登录凭据；默认只复制插件、配置和 DSH 版本")),
                            )
                            .child(clickable(
                                switch(personal),
                                true,
                                cx.listener(move |this, _, _, cx| {
                                    edit(this, cx, |d| d.personal = !d.personal)
                                }),
                            )),
                    )
            }
            NewMode::Blank => {
                // 分离本地已有和在线版本
                let (local_versions, remote_versions): (Vec<_>, Vec<_>) =
                    dialog.versions.iter().partition(|choice| choice.installed);

                let render_version_row = |choice: &&VersionChoice, show_radio: bool| {
                    let selected = dialog.selected.as_deref() == Some(choice.version.as_str());
                    let prerelease = choice
                        .remote
                        .as_ref()
                        .map_or(choice.version.contains('-'), |r| r.prerelease);
                    let date = choice
                        .remote
                        .as_ref()
                        .map(|r| r.release_date.chars().take(10).collect::<String>())
                        .unwrap_or_default();
                    let size = choice
                        .remote
                        .as_ref()
                        .and_then(|r| r.feed.as_ref())
                        .map(|f| storage::format_bytes(f.size));
                    let downloading = self
                        .download
                        .jobs
                        .iter()
                        .any(|j| j.key == format!("dsh:{}", choice.version));
                    let version = choice.version.clone();
                    clickable(
                        Hoverable::new(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .when(selected, |el| el.bg(rgb(PRIMARY_MUTED)))
                                .when(show_radio, |el| {
                                    el.child(
                                        div()
                                            .flex()
                                            .flex_shrink_0()
                                            .size(px(16.))
                                            .items_center()
                                            .justify_center()
                                            .rounded_full()
                                            .border_2()
                                            .border_color(rgb(if selected {
                                                PRIMARY
                                            } else {
                                                BORDER
                                            }))
                                            .when(selected, |el| {
                                                el.child(
                                                    div()
                                                        .size(px(6.))
                                                        .rounded_full()
                                                        .bg(rgb(PRIMARY)),
                                                )
                                            }),
                                    )
                                })
                                .child(
                                    div()
                                        .font_family(MONO)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(if selected { PRIMARY } else { TEXT }))
                                        .child(choice.version.clone()),
                                )
                                .child(if prerelease {
                                    chip("预发布", ChipTone::Warn)
                                } else {
                                    chip("稳定版", ChipTone::Good)
                                })
                                .child(div().flex_1())
                                .when(!date.is_empty(), |el| el.child(hint(date)))
                                .child(if choice.installed {
                                    chip("本地可用", ChipTone::Good)
                                } else if downloading {
                                    chip("下载中", ChipTone::Info)
                                } else {
                                    chip(
                                        size.map_or("需下载".to_owned(), |s| {
                                            format!("需下载 · {s}")
                                        }),
                                        ChipTone::Off,
                                    )
                                }),
                            move |s| if selected { s } else { s.bg(rgb(BG_ROW_HOVER)) },
                        ),
                        !dialog.busy,
                        cx.listener(move |this, _, _, cx| {
                            let version = version.clone();
                            edit(this, cx, move |d| d.selected = Some(version));
                        }),
                    )
                };

                let local_section = if !local_versions.is_empty() {
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div().px(px(8.)).py(px(4.)).child(
                                    div()
                                        .text_size(px(11.5))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(SUCCESS))
                                        .child("✓ 本地已有运行时（无需下载）"),
                                ),
                            )
                            .children(
                                local_versions
                                    .iter()
                                    .map(|choice| render_version_row(choice, true)),
                            ),
                    )
                } else {
                    None
                };

                let remote_section = if !remote_versions.is_empty() {
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .when(!local_versions.is_empty(), |el| {
                                el.child(
                                    div()
                                        .h(px(1.))
                                        .mx(px(12.))
                                        .my(px(8.))
                                        .bg(rgb(BORDER_SUBTLE)),
                                )
                            })
                            .child(
                                div().px(px(8.)).py(px(4.)).child(
                                    div()
                                        .text_size(px(11.5))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(TEXT_2))
                                        .child("在线版本（需要下载）"),
                                ),
                            )
                            .children(
                                remote_versions
                                    .iter()
                                    .map(|choice| render_version_row(choice, true)),
                            ),
                    )
                } else {
                    None
                };

                let status = if dialog.listing {
                    Some(hint("正在读取可用版本…"))
                } else {
                    dialog
                        .list_error
                        .as_ref()
                        .map(|e| {
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(DANGER))
                                .child(format!("读取版本列表失败：{e}"))
                        })
                        .or_else(|| {
                            dialog.list_note.as_ref().map(|n| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(WARNING_TEXT))
                                    .child(n.clone())
                            })
                        })
                };
                let empty = dialog.versions.is_empty() && !dialog.listing;
                div().child(section(
                    "DSH 版本",
                    Some(
                        div().flex().flex_wrap().gap(px(6.)).child(clickable(
                            btn(if dialog.listing { "读取中…" } else { "刷新列表" }, Tone::Default, true),
                            !dialog.listing,
                            cx.listener(|this, _, _, cx| this.list_versions(cx)),
                        )).child(clickable(
                            btn("管理版本…", Tone::Default, true),
                            true,
                            cx.listener(|this, _, _, cx| {
                                this.dialog = None;
                                this.active_view = View::GlobalSettings;
                                cx.notify();
                            }),
                        )),
                    ),
                ))
                .child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .mb(px(8.))
                        .rounded(px(8.))
                        .bg(rgba(PRIMARY_TINT_08))
                        .border_1()
                        .border_color(rgba(PRIMARY_TINT_25))
                        .child(
                            div()
                                .w_full()
                                .min_w(px(0.))
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .text_color(rgb(TEXT_2))
                                .child("💡 运行时共用说明：多个实例可以共用同一个 DSH 运行时（程序文件），但每个实例的数据（插件、配置、会话、登录信息）是完全隔离的，互不影响。")
                        )
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .mb(px(8.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(rgb(BG_ELEVATED))
                        .child(hint("下载来源").flex_shrink_0())
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .text_color(rgb(TEXT_2))
                                .child(if dialog.feed_url.is_empty() { "未设置".to_owned() } else { dialog.feed_url.clone() }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .max_h(px(240.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(rgb(BORDER_SUBTLE))
                        .when(empty, |el| {
                            el.child(div().p(px(12.)).child(hint(
                                "没有可选的版本。实例会跟随最新稳定版，下载好 DSH 后就能启动。",
                            )))
                        })
                        .when_some(local_section, |el, section| el.child(section))
                        .when_some(remote_section, |el, section| el.child(section))
                        .overflow_y_scrollbar(),
                )
                .when_some(status, |el, status| el.child(div().mt(px(6.)).child(status)))
                .when(dialog.only_prerelease, |el| {
                    el.child(div().mt(px(6.)).child(hint("这个来源目前只有预发布版本，所以「只使用稳定版」时也列出了它们。")))
                })
                .child(div().mt(px(6.)).child(hint("选择本地已有的运行时可立即创建实例；选择在线版本会在创建后自动下载。")))
            }
        };

        let submit = clickable(
            btn(
                if dialog.busy {
                    "正在创建…"
                } else {
                    "创建"
                },
                Tone::Primary,
                false,
            ),
            !dialog.busy,
            cx.listener(|this, _, _, cx| this.submit_new_dialog(cx)),
        );

        dialog_shell(
            "新建实例",
            TEXT,
            600.,
            body()
                .gap(px(18.))
                .child(
                    div()
                        .child(section("实例名称", None))
                        .child(Input::new(&dialog.name)),
                )
                .child(div().child(section("初始内容", None)).child(mode_row))
                .child(detail)
                .when_some(dialog.error.clone(), |el, error| {
                    el.child(error_text(error))
                }),
            div().child(self.cancel_button(cx)).child(submit),
        )
    }

    /// 整合包详情：列出包含的所有插件。
    fn render_pack_plugins_dialog(
        &self,
        name: String,
        author: String,
        description: String,
        dsh_version: String,
        plugins: Vec<String>,
        size: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut summary = vec![format!("{} 个插件", plugins.len())];
        if !author.is_empty() {
            summary.push(format!("分享者 {author}"));
        }
        if size > 0 {
            summary.push(storage::format_bytes(size));
        }
        if !dsh_version.is_empty() {
            summary.push(format!("运行时 DSH {dsh_version}"));
        }

        let list = if plugins.is_empty() {
            card().child(
                div()
                    .px(px(20.))
                    .py(px(24.))
                    .child(hint("这个整合包没有启用任何插件")),
            )
        } else {
            card().children(plugins.iter().enumerate().map(|(index, plugin)| {
                row_item(index == 0)
                    .child(
                        div()
                            .w(px(24.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_3))
                            .child(format!("{}", index + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(plugin.clone()),
                    )
            }))
        };

        dialog_shell(
            "整合包详情",
            TEXT,
            560.,
            body()
                .gap(px(12.))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .child(name),
                )
                .child(hint(summary.join(" · ")))
                .when(!description.is_empty(), |el| {
                    el.child(paragraph(description.clone()))
                })
                .child(list),
            div().child(clickable(
                btn("关闭", Tone::Primary, true),
                true,
                cx.listener(|this, _, _, cx| {
                    this.dialog = None;
                    cx.notify();
                }),
            )),
        )
    }

    /// 社区设置：整合包目录与 GitHub 账号。
    fn render_community_settings_dialog(&self, cx: &mut Context<Self>) -> Div {
        let source = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field_label("整合包目录"))
            .children(self.packs.url_input.as_ref().map(Input::new))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(clickable(
                        btn("恢复默认", Tone::Default, true),
                        true,
                        cx.listener(|this, _, window, cx| {
                            if let Some(input) = this.packs.url_input.clone() {
                                input.update(cx, |input, cx| {
                                    input.set_value(DEFAULT_PACK_CATALOG_URL.to_owned(), window, cx)
                                });
                                cx.notify();
                            }
                        }),
                    ))
                    .child(div().flex_1())
                    .child(clickable(
                        btn("保存并刷新", Tone::Primary, true),
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            this.refresh_catalog(cx);
                        }),
                    )),
            )
            .child(hint("默认是社区仓库，也可以换成别的目录地址"));

        let account = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field_label("GitHub 账号"))
            .child(match self.github_auth.as_ref() {
                Some(auth) => div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(chip(format!("已连接 · {}", auth.username), ChipTone::Good))
                    .child(clickable(
                        btn("退出登录", Tone::Default, true),
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.github_auth = None;
                            let data_root = this.state.data_root.clone();
                            crate::github::clear_token(&data_root);
                            cx.notify();
                        }),
                    )),
                None => div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(hint("还没有连接，分享到社区前需要连接一次"))
                    .child(clickable(
                        btn("连接 GitHub", Tone::Primary, true),
                        true,
                        cx.listener(|this, _, window, cx| {
                            this.open_github_login_dialog(window, cx)
                        }),
                    )),
            })
            .when(self.github_auth.is_none(), |el| {
                el.child(hint("授权页面显示为 GitHub CLI 是正常的"))
            });

        dialog_shell(
            "社区设置",
            TEXT,
            560.,
            body()
                .gap(px(18.))
                .child(paragraph(
                    "整合包列表来自社区仓库；分享自己的整合包需要 GitHub 账号",
                ))
                .child(source)
                .child(div().h(px(1.)).bg(rgb(BORDER_SUBTLE)))
                .child(account),
            div()
                .flex()
                .gap(px(8.))
                .child(clickable(
                    btn("社区主页", Tone::Default, true),
                    true,
                    cx.listener(|_, _, _, cx| {
                        cx.open_url("https://github.com/QuaonY/dsh-marina-packs")
                    }),
                ))
                .child(clickable(
                    btn("关闭", Tone::Primary, true),
                    true,
                    cx.listener(|this, _, window, cx| {
                        if let (Some(input), Ok(saved)) = (
                            this.packs.url_input.clone(),
                            this.state.settings_store.load(),
                        ) {
                            input.update(cx, |input, cx| {
                                input.set_value(saved.pack_catalog_url, window, cx)
                            });
                        }
                        this.dialog = None;
                        cx.notify();
                    }),
                )),
        )
    }

    fn render_share_pack_dialog(
        &self,
        instance: Option<String>,
        name: Entity<InputState>,
        description: Entity<InputState>,
        mode: ShareMode,
        selected_folder: Option<PathBuf>,
        error: Option<String>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mode_tab = |target: ShareMode, title: &'static str, cx: &mut Context<Self>| {
            let active = mode == target;
            clickable(
                Hoverable::new(
                    div()
                        .flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .py(px(8.))
                        .rounded(px(9.))
                        .border_1()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .when(active, |el| {
                            el.border_color(rgb(PRIMARY))
                                .bg(rgb(PRIMARY_MUTED))
                                .text_color(rgb(PRIMARY))
                        })
                        .when(!active, |el| {
                            el.border_color(rgb(BORDER_SUBTLE))
                                .bg(rgb(BG_CARD))
                                .text_color(rgb(TEXT_2))
                        })
                        .child(title),
                    move |s| {
                        if active {
                            s
                        } else {
                            s.bg(rgb(BG_HOVER)).text_color(rgb(TEXT))
                        }
                    },
                ),
                !busy,
                cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::SharePack { mode, error, .. }) = this.dialog.as_mut()
                        && *mode != target
                    {
                        *mode = target;
                        *error = None;
                        cx.notify();
                    }
                }),
            )
        };
        let destinations = div()
            .flex()
            .gap(px(8.))
            .child(mode_tab(ShareMode::Community, "分享到社区", cx))
            .child(mode_tab(ShareMode::Local, "只导出到本地", cx));

        let mode_hint = match mode {
            ShareMode::Community => {
                "提交到社区仓库；通过自动校验并由维护者合并后，所有人都能在「整合包」里看到"
            }
            ShareMode::Local => "生成 .zip 和目录条目，放进你选的文件夹，自己决定怎么发给别人",
        };

        let radio = |selected: bool| {
            div()
                .flex()
                .flex_shrink_0()
                .size(px(16.))
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(rgb(if selected { PRIMARY } else { BORDER }))
                .when(selected, |el| {
                    el.child(div().size(px(8.)).rounded_full().bg(rgb(PRIMARY)))
                })
        };

        // 实例选择器：紧凑列表，选中行高亮。
        let instance_selector = div()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(BORDER_SUBTLE))
            .overflow_hidden()
            .max_h(px(190.))
            .overflow_y_scrollbar()
            .children(self.instances.iter().enumerate().map(|(index, inst)| {
                let is_selected = instance.as_ref() == Some(&inst.id);
                let id = inst.id.clone();
                let detail = if inst.notes.is_empty() {
                    format!("DSH {}", inst.runtime.dsh_version)
                } else {
                    inst.notes.clone()
                };
                clickable(
                    Hoverable::new(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(13.))
                            .py(px(9.))
                            .when(index > 0, |el| {
                                el.border_t_1().border_color(rgb(BORDER_SUBTLE))
                            })
                            .when(is_selected, |el| el.bg(rgb(PRIMARY_MUTED)))
                            .child(radio(is_selected))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(inst.name.clone()),
                            )
                            .child(hint(detail).text_size(px(11.5)).flex_shrink_0()),
                        move |s| {
                            if is_selected {
                                s
                            } else {
                                s.bg(rgb(BG_ROW_HOVER))
                            }
                        },
                    ),
                    !busy,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(Dialog::SharePack { instance, .. }) = this.dialog.as_mut() {
                            *instance = Some(id.clone());
                            cx.notify();
                        }
                    }),
                )
            }));

        // 只导出到本地时选文件夹
        let folder_section = (mode == ShareMode::Local).then(|| {
            let folder_display = selected_folder
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "还没有选择保存位置".to_string());
            div().child(field_label("保存位置")).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .px(px(11.))
                            .py(px(7.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(rgb(BORDER_SUBTLE))
                            .bg(rgb(BG_ELEVATED))
                            .font_family(MONO)
                            .text_size(px(12.))
                            .truncate()
                            .text_color(rgb(if selected_folder.is_some() {
                                TEXT_2
                            } else {
                                TEXT_3
                            }))
                            .child(folder_display),
                    )
                    .child(clickable(
                        btn("浏览…", Tone::Default, true),
                        !busy,
                        cx.listener(|this, _, _, cx| this.select_share_folder(cx)),
                    )),
            )
        });

        // 分享到社区时的 GitHub 账号状态：一条紧凑的横条。
        let github_section =
            (mode == ShareMode::Community).then(|| match self.github_auth.as_ref() {
                Some(auth) => div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(9.))
                    .bg(rgba(SUCCESS_TINT_08))
                    .border_1()
                    .border_color(rgba(SUCCESS_TINT_24))
                    .child(
                        div()
                            .size(px(7.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(SUCCESS)),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(auth.username.clone()),
                    )
                    .child(hint("已连接 GitHub").text_size(px(12.)))
                    .child(div().flex_1())
                    .child(clickable(
                        Hoverable::new(
                            div()
                                .flex_shrink_0()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT_3))
                                .child("退出登录"),
                            |s| s.text_color(rgb(DANGER)),
                        ),
                        !busy,
                        cx.listener(|this, _, _, cx| {
                            this.github_auth = None;
                            let data_root = this.state.data_root.clone();
                            crate::github::clear_token(&data_root);
                            cx.notify();
                        }),
                    )),
                None => div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(9.))
                    .bg(rgb(BG_ELEVATED))
                    .border_1()
                    .border_color(rgb(BORDER_SUBTLE))
                    .child(hint("需要连接 GitHub 账号才能提交到社区，连接一次即可"))
                    .child(div().flex_1())
                    .child(clickable(
                        btn("连接 GitHub", Tone::Default, true),
                        !busy,
                        cx.listener(|this, _, window, cx| {
                            this.open_github_login_dialog(window, cx)
                        }),
                    )),
            });

        // 提交按钮的启用条件
        let connected = self.github_auth.is_some();
        let can_submit = !busy
            && instance.is_some()
            && match mode {
                ShareMode::Community => connected,
                ShareMode::Local => selected_folder.is_some(),
            };

        let submit_label = match (mode, busy) {
            (ShareMode::Community, true) => "正在提交…",
            (ShareMode::Community, false) => "分享到社区",
            (ShareMode::Local, true) => "正在导出…",
            (ShareMode::Local, false) => "导出到本地",
        };

        let submit = clickable(
            btn(submit_label, Tone::Primary, false),
            can_submit,
            cx.listener(|this, _, _, cx| this.submit_share_pack(cx)),
        );

        let footer_hint = match mode {
            ShareMode::Community => {
                "首次分享会在你的 GitHub 账号下自动创建一个公开仓库 dsh-marina-shared-packs（用于存放整合包文件），随后在社区仓库创建 PR；通过自动校验后由维护者合并"
            }
            ShareMode::Local => "会生成 .zip 和目录条目，之后自己发送给别人",
        };

        dialog_shell(
            "分享整合包",
            TEXT,
            640.,
            body()
                .gap(px(16.))
                .child(hint("把实例里的插件组合打包，别人下载后能一键创建同样的实例。只包含插件配置，不含会话历史、账号和密钥").text_size(px(12.5)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(field_label("分享方式"))
                        .child(destinations)
                        .child(hint(mode_hint).text_size(px(12.)).min_h(px(16.))),
                )
                .when_some(github_section, |el, section| el.child(section))
                .when_some(folder_section, |el, section| el.child(section))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(field_label("选择实例"))
                        .child(instance_selector),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(div().child(field_label("整合包名称")).child(Input::new(&name)))
                        .child(
                            div()
                                .child(field_label("整合包描述"))
                                .child(Input::new(&description))
                                .child(
                                    hint("简单介绍用途和特点，会显示在社区列表里")
                                        .mt(px(7.))
                                        .text_size(px(12.)),
                                ),
                        ),
                )
                .when_some(error, |el, err| el.child(error_text(err))),
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().flex_1().min_w(px(0.)).child(hint(footer_hint).text_size(px(12.))))
                .child(self.cancel_button(cx))
                .child(submit),
        )
    }

    fn render_github_login_dialog(
        &self,
        user_code: &str,
        verification_uri: &str,
        waiting: bool,
        connecting: bool,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let user_code = user_code.to_string();
        let verification_uri = verification_uri.to_string();

        // 第一步：说明界面，让用户点击"连接"按钮
        if user_code.is_empty() && !connecting {
            return dialog_shell(
                "连接 GitHub 账号",
                TEXT,
                500.,
                body()
                    .gap(px(18.))
                    .child(paragraph("要分享整合包到社区，需要先连接你的 GitHub 账号"))
                    .child(hint("• 点击下方按钮后，会在浏览器打开 GitHub 授权页面"))
                    .child(hint("• 在浏览器完成授权后，这里会自动继续"))
                    .child(hint("• 只需要登录一次，之后会记住你的账号"))
                    .child(hint(
                        "• 授权页面会显示为 GitHub CLI，这是正常的，请放心授权",
                    ))
                    .when_some(error.as_ref(), |el, err| el.child(error_text(err.clone()))),
                div()
                    .flex()
                    .gap(px(8.))
                    .child(self.cancel_button(cx))
                    .child(clickable(
                        btn("连接 GitHub", Tone::Primary, true),
                        true,
                        cx.listener(|this, _, window, cx| {
                            this.start_github_device_flow(window, cx);
                        }),
                    )),
            );
        }

        // 正在请求授权码
        if user_code.is_empty() && connecting {
            return dialog_shell(
                "连接 GitHub 账号",
                TEXT,
                500.,
                body()
                    .gap(px(18.))
                    .child(hint("⏳ 正在连接 GitHub，请稍候..."))
                    .when_some(error.as_ref(), |el, err| el.child(error_text(err.clone()))),
                div().child(self.cancel_button(cx)),
            );
        }

        let code_display = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(16.))
            .px(px(24.))
            .py(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_color(rgb(BORDER_SUBTLE))
            .bg(rgb(BG_ELEVATED))
            .child(
                div()
                    .text_size(px(32.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(PRIMARY))
                    .child(user_code.clone()),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(TEXT_3))
                    .child(format!("请访问 {verification_uri} 并输入上方代码完成授权")),
            );

        let code_for_copy = user_code.clone();
        let copy_button = clickable(
            btn("📋 复制代码", Tone::Default, true),
            true,
            cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(code_for_copy.clone()));
            }),
        );

        let verification_uri_clone = verification_uri.clone();
        let open_button = clickable(
            btn("🌐 打开 GitHub", Tone::Primary, true),
            true,
            cx.listener(move |_, _, _, cx| {
                cx.open_url(&verification_uri_clone);
            }),
        );

        dialog_shell(
            "连接 GitHub 账号",
            TEXT,
            500.,
            body()
                .gap(px(18.))
                .child(paragraph("已在浏览器打开授权页面，代码也已复制。在页面中粘贴下方代码并授权即可，完成后会自动继续"))
                .child(code_display)
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(copy_button)
                        .child(open_button)
                )
                .child(hint("提示：授权页面显示为 GitHub CLI 是正常的"))
                .when(waiting, |el| {
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .text_color(rgb(TEXT_3))
                            .child("⏳")
                            .child("等待授权中...")
                    )
                })
                .when_some(error, |el, err| el.child(error_text(err))),
            div().child(self.cancel_button(cx)),
        )
    }

    fn render_share_to_community_dialog(
        &self,
        pack_name: &str,
        _current_step: usize,
        _total_steps: usize,
        steps: &[(String, ShareStepStatus)],
        progress: u8,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        pr_url: Option<String>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let step_list = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(steps.iter().map(|(step_name, status)| {
                let (icon, color) = match status {
                    ShareStepStatus::Pending => ("○", TEXT_3),
                    ShareStepStatus::InProgress => ("→", PRIMARY),
                    ShareStepStatus::Completed => ("✓", SUCCESS),
                    ShareStepStatus::Failed => ("✗", DANGER),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(16.)).text_color(rgb(color)).child(icon))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(if *status == ShareStepStatus::Pending {
                                TEXT_3
                            } else {
                                TEXT
                            }))
                            .child(step_name.clone()),
                    )
            }));

        let progress_bar = div()
            .w_full()
            .h(px(8.))
            .rounded(px(4.))
            .bg(rgb(BG_ELEVATED))
            .child(
                div()
                    .h_full()
                    .w(relative(progress as f32 / 100.0))
                    .rounded(px(4.))
                    .bg(rgb(PRIMARY)),
            );

        let is_success = pr_url.is_some();
        let failed = error.is_some();
        let content = if let Some(url) = &pr_url {
            // 成功状态
            body()
                .gap(px(18.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(12.))
                        .child(div().text_size(px(48.)).child("✅"))
                        .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child("提交完成！"))
                )
                .child(paragraph(format!("你的整合包「{}」已提交到社区。", pack_name)))
                .child(
                    div()
                        .px(px(16.))
                        .py(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER_SUBTLE))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.)).child("🌟 下一步"))
                                .child(hint("社区会自动校验 PR，通过后由维护者合并。合并后你的整合包就会出现在所有用户的整合包列表里"))
                        )
                )
                .child(
                    clickable(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(6.))
                            .px(px(16.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .bg(rgb(PRIMARY))
                            .text_color(rgb(TEXT_ON_PRIMARY))
                            .hover(|s| s.bg(rgb(PRIMARY_HOVER)))
                            .cursor(CursorStyle::PointingHand)
                            .child("查看 Pull Request"),
                        true,
                        cx.listener({
                            let url = url.clone();
                            move |_, _, _, cx| {
                                cx.open_url(&url);
                            }
                        }),
                    )
                )
        } else {
            // 进行中状态
            body()
                .gap(px(18.))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("正在分享到社区"),
                )
                .child(hint(format!("整合包：{}", pack_name)))
                .child(step_list)
                .child(progress_bar)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(6.))
                        .text_size(px(13.))
                        .text_color(rgb(TEXT_3))
                        .child(format!("{}%", progress)),
                )
                .when_some(error, |el, err| el.child(error_text(err)))
        };

        let footer = if is_success {
            div().child(clickable(
                btn("完成", Tone::Primary, false),
                true,
                cx.listener(|this, _, _, cx| {
                    this.dialog = None;
                    cx.notify();
                }),
            ))
        } else if failed {
            // 失败/取消后要能关掉对话框，别把用户困在这里。
            div().child(clickable(
                btn("关闭", Tone::Default, false),
                true,
                cx.listener(|this, _, _, cx| {
                    this.dialog = None;
                    cx.notify();
                }),
            ))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(clickable(
                    btn("取消分享", Tone::Default, false),
                    true,
                    cx.listener({
                        let cancel = cancel.clone();
                        move |_, _, _, cx| {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            cx.notify();
                        }
                    }),
                ))
                .child(hint("当前步骤结束后停止"))
        };

        dialog_shell(
            if is_success {
                "分享成功"
            } else {
                "正在分享"
            },
            TEXT,
            500.,
            content,
            footer,
        )
    }

    fn render_delete_dialog(
        &self,
        instance_id: String,
        mode: DeleteMode,
        cx: &mut Context<Self>,
    ) -> Div {
        let name = self.instance_name(&instance_id);
        let running = is_running(&self.get_instance_status(&instance_id));
        let imported = self.instances.iter().any(|i| {
            i.id == instance_id
                && i.origin.kind == dsh_marina::instances::OriginKind::ImportOriginal
        });
        let modes = [
            (
                DeleteMode::Export,
                "导出后删除（推荐）",
                "先导出为 .zip 备份，再删除实例",
            ),
            (
                DeleteMode::KeepData,
                "保留数据后删除",
                "home/ 目录移到 Marina 回收目录保留 30 天，其余删除",
            ),
            (DeleteMode::Direct, "直接删除", "全部移到系统回收站"),
        ];
        let options = modes.into_iter().map(|(option, title, desc)| {
            clickable(
                option_card(title, desc, option == mode),
                true,
                cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::DeleteInstance { mode, .. }) = this.dialog.as_mut() {
                        *mode = option;
                        cx.notify();
                    }
                }),
            )
        });

        let submit = clickable(
            btn("确认删除", Tone::Danger, false),
            !running,
            cx.listener(move |this, _, _, cx| {
                this.dialog = None;
                this.delete_instance(&instance_id, mode, cx);
            }),
        );

        dialog_shell(
            "删除实例",
            TEXT,
            580.,
            body()
                .child(paragraph(format!(
                    "即将删除「{name}」及其全部数据：插件、会话、登录凭据、配置。"
                )))
                .when(running, |el| el.child(error_text("实例正在运行，请先停止再删除。".to_owned())))
                .when(imported, |el| {
                    el.child(paragraph(
                        "这个实例是从原版 DSH 导入的副本，删除它不会影响原版。要删除原版 DSH，请到「全局设置 → DSH 版本管理」。",
                    ))
                })
                .child(
                    div()
                        .child(field_label("删除方式"))
                        .child(div().flex().flex_col().gap(px(12.)).children(options)),
                ),
            div().child(self.cancel_button(cx)).child(submit),
        )
    }

    pub(super) fn render_toasts(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.last_error.is_none() && self.last_success.is_none() {
            return None;
        }
        let toast = |message: String, error: bool, cx: &mut Context<Self>| {
            div()
                .flex()
                .items_start()
                .gap(px(12.))
                .max_w(px(420.))
                .px(px(20.))
                .py(px(14.))
                .rounded(px(11.))
                .border_1()
                .border_color(rgb(if error { ERROR_BORDER_SOFT } else { BORDER }))
                .bg(rgb(BG_CARD))
                .shadow_lg()
                .child(
                    div()
                        .flex_shrink_0()
                        .mt(px(5.))
                        .size(px(8.))
                        .rounded_full()
                        .bg(rgb(if error { DANGER } else { SUCCESS })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(px(14.))
                        .child(message),
                )
                .child(clickable(
                    div()
                        .flex_shrink_0()
                        .px(px(4.))
                        .text_color(rgb(TEXT_3))
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .child("✕"),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        if error {
                            this.last_error = None;
                        } else {
                            this.last_success = None;
                        }
                        cx.notify();
                    }),
                ))
        };

        let mut stack = div()
            .absolute()
            .right(px(28.))
            .bottom(px(28.))
            .flex()
            .flex_col()
            .items_end()
            .gap(px(10.));
        if let Some(error) = self.last_error.clone() {
            stack = stack.child(toast(error, true, cx));
        }
        if let Some(message) = self.last_success.clone() {
            stack = stack.child(toast(message, false, cx));
        }
        Some(stack)
    }
}
