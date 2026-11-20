//! Global settings view.

use super::*;
use dsh_marina::download::DownloadSource;
use dsh_marina::settings::MarinaSettings;

impl MarinaWindow {
    pub(super) fn render_global_settings(&self, cx: &mut Context<Self>) -> Div {
        // In-memory snapshot: reading the settings file per frame put disk I/O
        // on the render path and stalled the UI under disk load.
        let settings = self.settings_cache.borrow().clone();

        let toggle = |this_flag: bool, apply: fn(&mut MarinaSettings), cx: &mut Context<Self>| {
            clickable(
                switch(this_flag),
                true,
                cx.listener(move |this, _, _, cx| {
                    this.update_settings(apply, cx);
                }),
            )
        };

        let auto_start_on = settings.auto_start;
        let auto_start = clickable(
            switch(auto_start_on),
            true,
            cx.listener(move |this, _, _, cx| {
                let enabled = !auto_start_on;
                match platform::set_auto_start(enabled) {
                    Ok(()) => {
                        this.update_settings(move |s| s.auto_start = enabled, cx);
                    }
                    Err(e) => {
                        this.last_error = Some(format!("设置开机自启动失败：{e}"));
                        cx.notify();
                    }
                }
            }),
        );

        let general = card()
            .child(card_head("通用", div()))
            .child(form_row(
                false,
                "深色模式",
                "切换深色外观，立即生效并自动保存",
                toggle(settings.dark_mode, |s| s.dark_mode = !s.dark_mode, cx),
            ))
            .child(form_row(
                false,
                "开机自启动",
                "Windows 启动时自动运行 Marina",
                auto_start,
            ))
            .child(form_row(
                false,
                "退出前确认",
                "还有实例在运行时，退出前先确认",
                toggle(
                    settings.confirm_on_exit,
                    |s| s.confirm_on_exit = !s.confirm_on_exit,
                    cx,
                ),
            ))
            .child(form_row(
                false,
                "停止实例前提醒",
                "停止或重启前提示「可能丢失未保存内容」",
                toggle(
                    settings.confirm_stop,
                    |s| s.confirm_stop = !s.confirm_stop,
                    cx,
                ),
            ))
            .child(form_row(
                false,
                "自动检查更新",
                "每次启动 Marina 时检查一次新版本，发现后通知我",
                toggle(
                    settings.auto_check_update,
                    |s| s.auto_check_update = !s.auto_check_update,
                    cx,
                ),
            ));

        let runtimes = self.cached_runtimes(cx);
        let newest_managed = runtimes
            .iter()
            .find(|r| !r.is_external())
            .map(|r| r.version.clone());

        // 分离托管版本和外部版本
        let (managed_runtimes, external_runtimes): (Vec<_>, Vec<_>) =
            runtimes.iter().partition(|r| !r.is_external());

        let render_runtime_row = |runtime: &&dsh_marina::runtimes::RuntimeInfo, is_first: bool| {
            let used = dsh_marina::runtimes::usage_count(
                &self.instances,
                runtime,
                newest_managed.as_deref(),
            );
            let size = self
                .runtime_disk
                .get(&runtime.id)
                .map(|bytes| format!(" · {}", storage::format_bytes(*bytes)))
                .unwrap_or_default();
            let usage_text = if used == 0 {
                format!("未被使用{size}")
            } else if used == 1 {
                format!("1 个实例在用{size}")
            } else {
                format!("{} 个实例在用{size}", used)
            };

            let action = if runtime.is_external() {
                clickable(
                    btn("卸载原版", Tone::Danger, true),
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.dialog = Some(Dialog::UninstallExternal { delete_data: false });
                        cx.notify();
                    }),
                )
            } else {
                let (runtime_id, version) = (runtime.id.clone(), runtime.version.clone());
                let users: Vec<String> = self
                    .instances
                    .iter()
                    .filter(|i| {
                        dsh_marina::runtimes::usage_count(
                            std::slice::from_ref(*i),
                            runtime,
                            newest_managed.as_deref(),
                        ) > 0
                    })
                    .map(|i| i.name.clone())
                    .collect();
                let mut targets = vec![(
                    dsh_marina::runtimes::LATEST_STABLE.to_owned(),
                    "最新稳定版（自动跟随）".to_owned(),
                )];
                targets.extend(runtimes.iter().filter(|r| r.id != runtime.id).map(|r| {
                    let binding = if r.is_external() {
                        dsh_marina::runtimes::EXTERNAL_RUNTIME_ID.to_owned()
                    } else {
                        r.version.clone()
                    };
                    (binding, r.display_label())
                }));
                clickable(
                    btn(
                        if used == 0 {
                            "删除"
                        } else {
                            "解绑并删除…"
                        },
                        Tone::Default,
                        true,
                    ),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = Some(if users.is_empty() {
                            Dialog::DeleteRuntime {
                                runtime_id: runtime_id.clone(),
                                version: version.clone(),
                            }
                        } else {
                            Dialog::RebindRuntime {
                                runtime_id: runtime_id.clone(),
                                version: version.clone(),
                                users: users.clone(),
                                targets: targets.clone(),
                                selected: 0,
                            }
                        });
                        cx.notify();
                    }),
                )
            };
            row_item(is_first)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(runtime.display_label()),
                        )
                        .child(hint(usage_text))
                        .child(
                            hint(runtime.executable.display().to_string())
                                .font_family(MONO)
                                .truncate(),
                        ),
                )
                .child(if runtime.is_external() {
                    chip("原版", ChipTone::Warn)
                } else {
                    chip("已托管", ChipTone::Good)
                })
                .child(action)
        };

        let mut runtime_rows = Vec::new();

        // 托管版本区块
        if !managed_runtimes.is_empty() {
            let is_first_section = runtime_rows.is_empty();
            runtime_rows.push(
                row_item(is_first_section).border_0().child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(12.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(t!(SUCCESS)))
                        .child(format!("✓ 已托管运行时（{} 个）", managed_runtimes.len())),
                ),
            );
            let start_index = runtime_rows.len();
            for (idx, runtime) in managed_runtimes.iter().enumerate() {
                runtime_rows.push(render_runtime_row(runtime, idx == 0 && start_index == 1));
            }
        }

        // 外部版本区块
        if !external_runtimes.is_empty() {
            runtime_rows.push(
                row_item(false).border_0().child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(12.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(t!(WARNING_TEXT)))
                        .child("⚠ 外部安装版本"),
                ),
            );
            for runtime in external_runtimes.iter() {
                runtime_rows.push(render_runtime_row(runtime, false));
            }
        }

        let offline = self.cached_offline_installers(cx);
        let offline_rows = offline
            .into_iter()
            .map(|installer| {
                let name = installer
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                row_item(false)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                            .child(hint("在 runtimes 目录里发现的离线安装包")),
                    )
                    .child(clickable(
                        btn("安装", Tone::Primary, true),
                        !self.download.installing,
                        cx.listener(move |this, _, _, cx| {
                            this.install_offline(installer.clone(), cx)
                        }),
                    ))
            })
            .collect::<Vec<_>>();

        let download_row = self.render_download_row(&runtimes, cx);
        let max_runtimes = settings.max_runtimes;
        let max_stepper = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(8.))
            .child(clickable(
                stepper_button("−"),
                max_runtimes > 1,
                cx.listener(|this, _, _, cx| {
                    this.update_settings(
                        |s| s.max_runtimes = s.max_runtimes.saturating_sub(1).max(1),
                        cx,
                    );
                }),
            ))
            .child(
                div()
                    .w(px(40.))
                    .text_center()
                    .font_weight(FontWeight::BOLD)
                    .child(max_runtimes.to_string()),
            )
            .child(clickable(
                stepper_button("＋"),
                true,
                cx.listener(|this, _, _, cx| {
                    this.update_settings(|s| s.max_runtimes += 1, cx);
                }),
            ));

        // Only the two official sources are offered; an older mirror choice
        // counts as the official one, a saved custom link selects neither.
        let current_source = match settings.download_source {
            DownloadSource::Mirror => DownloadSource::Official,
            source => source,
        };
        let source_picker = div().flex().gap(px(6.)).children(
            [DownloadSource::Official, DownloadSource::GitHub]
                .into_iter()
                .map(|source| {
                    let selected = source == current_source;
                    clickable(
                        div()
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(7.))
                            .border_1()
                            .border_color(rgb(if selected {
                                t!(PRIMARY)
                            } else {
                                t!(BORDER_SUBTLE)
                            }))
                            .bg(rgb(if selected {
                                t!(PRIMARY_MUTED)
                            } else {
                                t!(BG_CARD)
                            }))
                            .text_color(rgb(if selected { t!(PRIMARY) } else { t!(TEXT_2) }))
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(source.label()),
                        !selected,
                        cx.listener(move |this, _, window, cx| {
                            let url = this
                                .state
                                .settings_store
                                .load()
                                .ok()
                                .and_then(|s| crate::maintenance::feed_options(&s).feed_url(source))
                                .unwrap_or_default();
                            this.custom_link_input
                                .update(cx, |input, cx| input.set_value(url, window, cx));
                            this.download.lookup = None;
                            this.update_settings(move |s| s.download_source = source, cx);
                        }),
                    )
                    .id(SharedString::from(format!("source-{}", source.key())))
                }),
        );

        let link_row = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(20.))
            .py(px(14.))
            .border_t_1()
            .border_color(rgb(t!(BORDER_SUBTLE)))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(if current_source == DownloadSource::Custom {
                "下载链接（自定义）"
            } else {
                "下载链接"
            }))
            .child(hint("选择来源后这里显示对应的链接；也可以改成自己的地址（latest.yml 文件或它所在的目录），点保存生效"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex_1().min_w(px(0.)).child(Input::new(&self.custom_link_input)))
                    .child(clickable(
                        btn("保存", Tone::Primary, true),
                        true,
                        cx.listener(|this, _, _, cx| this.save_download_link(cx)),
                    )),
            );

        let versions = card()
            .child(card_head("DSH 版本与下载", hint(format!("已检测到 {} 个版本", runtimes.len()))))
            .child(
                row_item(true)
                    .border_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .px(px(12.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .bg(rgba(t!(PRIMARY_TINT_08)))
                            .border_1()
                            .border_color(rgba(t!(PRIMARY_TINT_25)))
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .line_height(px(18.))
                                    .text_size(px(12.))
                                    .text_color(rgb(t!(TEXT_2)))
                                    .child("💡 运行时共用机制：多个实例可以共用同一个 DSH 运行时（程序文件），节省磁盘空间。每个实例的数据（插件、配置、会话、登录信息）完全隔离，互不影响。删除运行时不会影响实例数据。")
                            )
                    )
            )
            .when(runtimes.is_empty(), |el| {
                el.child(row_item(false).child(hint("还没有可用的 DSH 运行时。DSH Marina 需要 DSH 桌面版才能运行实例。")))
            })
            .children(runtime_rows)
            .children(offline_rows)
            .child(download_row)
            .child(form_row(
                false,
                "只使用稳定版",
                "「最新稳定版」绑定不跟随预发布版本",
                toggle(settings.stable_only, |s| s.stable_only = !s.stable_only, cx),
            ))
            .child(form_row(
                false,
                "最多保留版本数",
                "超出时会弹窗提示清理不再被实例使用的旧版本",
                max_stepper,
            ))
            .child(form_row(false, "下载来源", "首选来源失败时会自动换另一个", source_picker))
            .child(link_row);
        let handle_links = settings.handle_links;
        let default_link = settings
            .default_link_instance
            .as_ref()
            .and_then(|id| self.instances.iter().find(|i| &i.id == id))
            .map(|i| i.name.clone());
        let links_card = card()
            .child(card_head("dsh:// 链接", div()))
            .child(form_row(
                true,
                "由 Marina 处理 dsh:// 链接",
                "每个 DSH 实例启动时都会抢注 dsh://，打开后由 Marina 询问用哪个实例打开；关闭后恢复原来的处理程序",
                clickable(
                    switch(handle_links),
                    true,
                    cx.listener(move |this, _, _, cx| this.set_handle_links(!handle_links, cx)),
                ),
            ))
            .child(form_row(
                false,
                "默认实例",
                &match &default_link {
                    Some(name) => format!("链接直接交给「{name}」"),
                    None => "每次都询问".to_owned(),
                },
                clickable(
                    btn("改为每次询问", Tone::Default, true),
                    default_link.is_some(),
                    cx.listener(|this, _, _, cx| {
                        this.update_settings(|s| s.default_link_instance = None, cx);
                    }),
                ),
            ))
            .child(form_row(
                false,
                "手动打开链接",
                "把 dsh:// 链接交给指定实例，不依赖系统里的协议关联",
                clickable(
                    btn("打开链接…", Tone::Default, true),
                    true,
                    cx.listener(|this, _, window, cx| this.open_link_dialog(window, cx)),
                ),
            ));

        let data_root = self.state.repository.data_root().to_path_buf();
        let env_locked = crate::data_root_override().is_some();
        let storage_card = card().child(card_head("数据存储", div())).child(
            row_item(true)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(4.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("数据位置"))
                        .child(hint(if env_locked {
                            "由环境变量 DSH_MARINA_DATA 指定，这里不能更改"
                        } else {
                            "实例、DSH 版本和设置都在这里。移到其他盘需要先停止所有实例"
                        }))
                        .child(path_box(&data_root)),
                )
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .gap(px(8.))
                        .child(clickable(
                            btn("打开", Tone::Default, true),
                            true,
                            cx.listener(move |_, _, _, _| platform::open_path(&data_root)),
                        ))
                        .child(clickable(
                            btn("更改…", Tone::Default, true),
                            !env_locked && self.copy_job.is_none(),
                            cx.listener(|this, _, _, cx| this.pick_data_location(cx)),
                        )),
                ),
        );

        let about_row = |first: bool, label: &str, value: Div| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .px(px(20.))
                .py(px(13.))
                .when(!first, |el| {
                    el.border_t_1().border_color(rgb(t!(BORDER_SUBTLE)))
                })
                .child(
                    div()
                        .w(px(80.))
                        .flex_shrink_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(label.to_owned()),
                )
                .child(value.flex_1().min_w(px(0.)))
        };

        let license_hint = {
            let count = license_count();
            let base = "界面基于 gpui / gpui-kit（Apache-2.0），托盘基于 gpui-tray";
            if count > 0 {
                format!("{base}。共 {count} 个开源组件，完整许可见清单")
            } else {
                format!("{base}，完整依赖许可见清单")
            }
        };

        let current_version = env!("CARGO_PKG_VERSION");
        let check_button = |label: &'static str, primary: bool, cx: &mut Context<Self>| {
            clickable(
                btn(
                    label,
                    if primary {
                        Tone::Primary
                    } else {
                        Tone::Default
                    },
                    true,
                ),
                true,
                cx.listener(|this, _, _, cx| this.check_app_update(false, cx)),
            )
        };
        let (update_status, update_action) = if self.app_update.checking {
            (
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(hint("正在检查更新…")),
                clickable(
                    btn("检查中…", Tone::Default, true),
                    false,
                    cx.listener(|_, _, _, _| {}),
                ),
            )
        } else if let Some(error) = &self.app_update.error {
            (
                div().flex().items_center().gap(px(8.)).child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(t!(DANGER)))
                        .child(format!("检查失败：{error}")),
                ),
                check_button("重试", false, cx),
            )
        } else if let Some(release) = &self.app_update.latest {
            let latest = release.tag_name.trim_start_matches('v').to_owned();
            if dsh_marina::runtimes::compare_versions(&latest, current_version).is_gt() {
                let staged = self
                    .app_update
                    .staged
                    .as_ref()
                    .is_some_and(|staged| staged.version == latest);
                (
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(chip(format!("新版本 {latest}"), ChipTone::Warn))
                        .child(hint(if staged {
                            format!("已下载，重启后安装 · 当前 {current_version}")
                        } else {
                            format!("当前 {current_version}")
                        })),
                    clickable(
                        btn(
                            if staged {
                                "重启并更新"
                            } else {
                                "查看更新"
                            },
                            Tone::Primary,
                            true,
                        ),
                        true,
                        cx.listener(|this, _, _, cx| this.open_or_apply_update(cx)),
                    ),
                )
            } else {
                (
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(chip("已是最新版本", ChipTone::Good))
                        .child(hint(format!("当前 {current_version}"))),
                    check_button("重新检查", false, cx),
                )
            }
        } else if self.app_update.checked {
            (
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(chip("还没有发布正式版本", ChipTone::Off))
                    .child(hint(format!("当前 {current_version}"))),
                check_button("重新检查", false, cx),
            )
        } else {
            (
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(hint(format!("当前 {current_version}，正式版发布在 GitHub"))),
                check_button("检查更新", true, cx),
            )
        };

        let about = card()
            .child(card_head("关于", div()))
            .child(
                row_item(true)
                    .child(brand_mark(40.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(FontWeight::BOLD).child("DSH Marina"))
                            .child(hint(format!(
                                "版本 {} · DSH 多实例启动器",
                                env!("CARGO_PKG_VERSION")
                            ))),
                    )
                    .child(div().flex_1())
                    .child(chip("MIT License", ChipTone::Info)),
            )
            .child(about_row(
                false,
                "版本更新",
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(update_status)
                    .child(div().flex_1())
                    .child(update_action),
            ))
            .child(about_row(
                false,
                "作者",
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(t!(TEXT_2)))
                    .child("Quaon"),
            ))
            .child(about_row(
                false,
                "项目主页",
                clickable(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.5))
                        .font_family(MONO)
                        .text_color(rgb(t!(PRIMARY)))
                        .hover(|s| s.text_color(rgb(t!(PRIMARY_HOVER))))
                        .child("https://github.com/QuaonY/dsh-marina"),
                    true,
                    cx.listener(|_, _, _, cx| cx.open_url("https://github.com/QuaonY/dsh-marina")),
                ),
            ))
            .child(about_row(
                false,
                "开源许可",
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(hint(license_hint))
                    .child(div().flex_1())
                    .child(clickable(
                        btn("查看清单", Tone::Default, true),
                        true,
                        cx.listener(|this, _, _, cx| this.show_licenses(cx)),
                    )),
            ));

        let danger = card().child(card_head("卸载", div())).child(form_row(
            true,
            "卸载 DSH Marina",
            "停止所有实例，按你的选择保存数据，然后删除 Marina",
            clickable(
                btn("卸载…", Tone::Danger, true),
                true,
                cx.listener(|this, _, _, cx| this.open_uninstall_marina(cx)),
            ),
        ));

        let header = div()
            .flex()
            .items_center()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .child("设置"),
                    )
                    .child(hint("这里的设置对所有实例生效").text_size(px(13.))),
            )
            .child(clickable(
                btn("返回", Tone::Default, false),
                true,
                cx.listener(|this, _, _, cx| {
                    this.active_view = if this.selected_instance.is_some() {
                        View::InstanceDetails
                    } else {
                        View::Home
                    };
                    cx.notify();
                }),
            ));

        div().flex().flex_col().flex_1().min_h_0().child(
            div()
                .flex()
                .flex_col()
                .gap(px(20.))
                .px(px(32.))
                .py(px(26.))
                .child(header)
                .child(general)
                .child(versions)
                .child(storage_card)
                .child(links_card)
                .child(about)
                .child(danger)
                .overflow_y_scrollbar(),
        )
    }

    /// 保存 next to the download link: an official link selects that source,
    /// anything else becomes the custom source.
    pub(crate) fn save_download_link(&mut self, cx: &mut Context<Self>) {
        let link = self.custom_link_input.read(cx).value().trim().to_owned();
        if !link.to_ascii_lowercase().starts_with("https://") {
            self.last_error = Some("下载链接需要以 https:// 开头".to_owned());
            cx.notify();
            return;
        }
        let settings = match self.state.settings_store.load() {
            Ok(settings) => settings,
            Err(error) => {
                self.last_error = Some(format!("无法读取设置：{error}"));
                cx.notify();
                return;
            }
        };
        let options = crate::maintenance::feed_options(&settings);
        let official = [DownloadSource::Official, DownloadSource::GitHub]
            .into_iter()
            .find(|source| options.feed_url(*source).as_deref() == Some(link.as_str()));
        self.download.lookup = None;
        self.last_error = None;
        let saved = self.update_settings(
            move |s| match official {
                Some(source) => {
                    s.download_source = source;
                    s.custom_url.clear();
                }
                None => {
                    s.download_source = DownloadSource::Custom;
                    s.custom_url = link;
                }
            },
            cx,
        );
        if saved {
            self.last_success = Some("下载链接已保存".to_owned());
        } else {
            self.last_success = None;
        }
    }

    /// The runtime list scans directories and probes the external install
    /// (multi-MB asar index); keep that off the render path and serve a
    /// background-warmed cache, refreshed at most every 5 seconds.
    fn cached_runtimes(&self, cx: &mut Context<Self>) -> Vec<dsh_marina::runtimes::RuntimeInfo> {
        let stale = self
            .runtimes_cache
            .borrow()
            .as_ref()
            .map(|(at, _)| at.elapsed() > std::time::Duration::from_secs(5))
            .unwrap_or(true);
        if stale && !self.runtimes_warming.get() {
            self.runtimes_warming.set(true);
            let state = self.state.clone();
            cx.spawn(async move |this, cx| {
                let list = cx
                    .background_executor()
                    .spawn(async move {
                        let settings = state.runtime_store.load().unwrap_or_default();
                        state.registry.all(&settings).unwrap_or_default()
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.runtimes_cache
                        .borrow_mut()
                        .replace((std::time::Instant::now(), list));
                    this.runtimes_warming.set(false);
                    cx.notify();
                });
            })
            .detach();
        }
        self.runtimes_cache
            .borrow()
            .as_ref()
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    }

    /// Offline installers are discovered by reading the runtimes directory; do
    /// that on a background task and serve the cached list on the render path.
    fn cached_offline_installers(&self, cx: &mut Context<Self>) -> Vec<std::path::PathBuf> {
        let stale = self
            .offline_cache
            .borrow()
            .as_ref()
            .map(|(at, _)| at.elapsed() > std::time::Duration::from_secs(5))
            .unwrap_or(true);
        if stale && !self.offline_warming.get() {
            self.offline_warming.set(true);
            let registry = self.state.registry.clone();
            cx.spawn(async move |this, cx| {
                let list = cx
                    .background_executor()
                    .spawn(async move { dsh_marina::download::offline_installers(&registry) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.offline_cache
                        .borrow_mut()
                        .replace((std::time::Instant::now(), list));
                    this.offline_warming.set(false);
                    cx.notify();
                });
            })
            .detach();
        }
        self.offline_cache
            .borrow()
            .as_ref()
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    }

    fn render_download_row(
        &self,
        runtimes: &[dsh_marina::runtimes::RuntimeInfo],
        cx: &mut Context<Self>,
    ) -> Div {
        let state = &self.download;
        let installed = |version: &str| {
            runtimes
                .iter()
                .any(|r| !r.is_external() && r.version == version)
        };
        let mut info = div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(4.));
        let mut actions = div().flex().flex_shrink_0().items_center().gap(px(8.));

        if let Some(job) = state.jobs.iter().find(|j| j.key.starts_with("dsh:")) {
            let (done, total) = job.amounts();
            let ratio = if total > 0 {
                (done as f32 / total as f32).clamp(0., 1.)
            } else {
                0.
            };
            let version = job.key.trim_start_matches("dsh:").to_owned();
            let label = if job.phase == JobPhase::Installing {
                format!("正在校验并安装 DSH {version}…")
            } else if job.phase == JobPhase::Paused {
                format!("DSH {version} 已暂停，可继续下载")
            } else {
                let rate = match job.speed(done) {
                    Some(speed) => {
                        let remaining = total.saturating_sub(done) as f64 / speed;
                        format!(
                            " · {}/s · 剩余约 {}",
                            storage::format_bytes(speed as u64),
                            storage::format_duration(remaining.ceil() as u64)
                        )
                    }
                    None => String::new(),
                };
                format!(
                    "正在下载 DSH {version}：{} / {}{rate}",
                    storage::format_bytes(done),
                    storage::format_bytes(total)
                )
            };
            info = info
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                .child(
                    div()
                        .h(px(6.))
                        .w_full()
                        .rounded(px(3.))
                        .bg(rgb(t!(BG_ELEVATED)))
                        .child(
                            div()
                                .h_full()
                                .rounded(px(3.))
                                .bg(rgb(t!(PRIMARY)))
                                .w(relative(ratio)),
                        ),
                );
            let downloading = job.phase == JobPhase::Downloading;
            let paused = job.phase == JobPhase::Paused;
            if downloading {
                actions = actions.child(clickable(btn("暂停", Tone::Default, true), true, {
                    let key = job.key.clone();
                    cx.listener(move |this, _, _, cx| this.pause_job(&key, cx))
                }));
            }
            if paused {
                actions = actions.child(clickable(btn("继续", Tone::Primary, true), true, {
                    let version = version.clone();
                    cx.listener(move |this, _, _, cx| {
                        this.download_runtime(
                            download::RemoteVersion {
                                version: version.clone(),
                                prerelease: false,
                                release_date: String::new(),
                                feed_url: String::new(),
                                feed: None,
                            },
                            cx,
                        )
                    })
                }));
            }
            actions = actions.child(clickable(
                btn("取消", Tone::Default, true),
                downloading || paused,
                {
                    let key = job.key.clone();
                    cx.listener(move |this, _, _, cx| this.cancel_job(&key, cx))
                },
            ));
        } else {
            match &state.lookup {
                Some(lookup) if !installed(&lookup.feed.version) => {
                    info = info
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                            "可下载 DSH {}（{}）",
                            lookup.feed.version,
                            lookup.source.label()
                        )))
                        .child(hint(format!(
                            "{} · 发布于 {}",
                            storage::format_bytes(lookup.feed.size),
                            lookup.feed.release_date
                        )));
                    actions = actions.child(clickable(
                        btn("下载并安装", Tone::Primary, true),
                        !state.installing,
                        cx.listener(|this, _, _, cx| this.start_download(cx)),
                    ));
                }
                Some(lookup) => {
                    info = info.child(hint(format!("已是最新：DSH {}", lookup.feed.version)));
                }
                None => {
                    info = info.child(hint(
                        "检查官方发布的新版本，下载后装到 Marina 自己的目录，不影响原版。",
                    ));
                }
            }
            actions = actions.child(clickable(
                btn(
                    if state.checking {
                        "检查中…"
                    } else {
                        "检查更新"
                    },
                    Tone::Default,
                    true,
                ),
                !state.checking,
                cx.listener(|this, _, _, cx| this.check_feed(false, cx)),
            ));
        }
        if let Some(error) = &state.error {
            info = info.child(div().text_color(rgb(t!(DANGER))).child(error.clone()));
        }
        row_item(false).child(info).child(actions)
    }
}
/// Third-party license list shown on the About page (generated from `cargo tree`).
pub(crate) const THIRD_PARTY_LICENSES: &str = include_str!("../../assets/THIRD-PARTY-LICENSES.txt");

fn license_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        THIRD_PARTY_LICENSES
            .lines()
            .filter(|line| line.starts_with("  "))
            .count()
    })
}
