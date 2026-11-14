//! Instance details tabs: overview, plugins, snapshots, logs and settings.

use super::*;

/// Log panel length (design §9 列表虚拟化).
const LOG_LINES: usize = 2000;

impl MarinaWindow {
    pub(super) fn render_rescue_bar(
        &self,
        instance: &InstanceConfig,
        record: Option<&LaunchRecord>,
        data: &DetailsData,
        cx: &mut Context<Self>,
    ) -> Div {
        let title = match record {
            Some(r) => match r.exit_code {
                Some(code) => format!(
                    "这个实例启动失败（{}，退出码 {}）",
                    fmt_utc(&r.started_at),
                    code
                ),
                None => format!("这个实例启动失败（{}）", fmt_utc(&r.started_at)),
            },
            None => "这个实例启动失败".to_owned(),
        };
        let suspects: Vec<String> = record.map(|r| r.suspects.clone()).unwrap_or_default();
        let last_good = data
            .snapshots
            .iter()
            .find(|s| s.last_good)
            .map(|s| s.id.clone());

        let body = if suspects.is_empty() {
            div()
                .text_color(rgb(TEXT_2))
                .child("没有找到嫌疑插件。可以回滚到上次正常状态，或查看日志排查原因。")
        } else {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .text_color(rgb(TEXT_2))
                .child("距上次正常启动，这些插件是嫌疑对象：")
                .children(suspects.iter().map(|name| {
                    div()
                        .px(px(10.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgba(DANGER_TINT_25))
                        .bg(rgba(DANGER_TINT_15))
                        .text_color(rgb(DANGER))
                        .font_family(MONO)
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name.clone())
                }))
                .child("这只是线索，不是判决。")
        };

        let id = instance.id.clone();
        let plugin_count = data.plugins.len();
        let mut row = div().flex().flex_wrap().gap(px(10.)).mt(px(14.));
        {
            let id = id.clone();
            row = row.child(clickable(
                btn("回滚到上次正常状态", Tone::Primary, true),
                last_good.is_some(),
                cx.listener(move |this, _, _, cx| {
                    if let Some(snapshot_id) = last_good.clone() {
                        this.dialog = Some(Dialog::RestoreSnapshot {
                            instance_id: id.clone(),
                            snapshot_id,
                        });
                        cx.notify();
                    }
                }),
            ));
        }
        {
            let id = id.clone();
            let names = suspects.clone();
            row = row.child(clickable(
                btn("只禁用嫌疑插件", Tone::Default, true),
                !suspects.is_empty(),
                cx.listener(move |this, _, _, cx| {
                    this.set_plugins_enabled(&id, names.clone(), false, Some("已禁用嫌疑插件"), cx);
                }),
            ));
        }
        {
            let id = id.clone();
            row = row.child(clickable(
                btn("安全模式", Tone::Default, true),
                plugin_count > 0,
                cx.listener(move |this, _, _, cx| this.enter_safe_mode(id.clone(), cx)),
            ));
        }
        {
            let id = id.clone();
            row = row.child(clickable(
                btn("二分排查", Tone::Default, true),
                plugin_count > 1,
                cx.listener(move |this, _, _, cx| this.start_bisect(id.clone(), cx)),
            ));
        }
        row = row.child(clickable(
            btn("查看日志", Tone::Default, true),
            true,
            cx.listener(|this, _, _, cx| {
                this.active_tab = DetailsTab::Logs;
                cx.notify();
            }),
        ));
        row = row.child(clickable(
            btn("忽略", Tone::Default, true),
            true,
            cx.listener(move |this, _, _, cx| {
                lifecycle::dismiss_crash(&this.state, &id);
                this.refresh_statuses();
                cx.notify();
            }),
        ));

        div()
            .flex()
            .gap(px(18.))
            .px(px(22.))
            .py(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_color(rgba(DANGER_TINT_25))
            .bg(rgba(DANGER_TINT_08))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .size(px(44.))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgba(DANGER_TINT_20))
                    .text_color(rgb(DANGER))
                    .text_size(px(20.))
                    .font_weight(FontWeight::BLACK)
                    .child("!"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(DANGER))
                            .child(title),
                    )
                    .child(body)
                    .child(row),
            )
    }

    pub(super) fn render_overview_tab(&self, instance: &InstanceConfig, data: &DetailsData) -> Div {
        let info = [
            ("实例 ID", instance.id.clone()),
            ("创建时间", fmt_rfc3339(&instance.created_at)),
            ("DSH 运行时", runtime_label(&instance.runtime.dsh_version)),
            (
                "上次使用版本",
                instance
                    .runtime
                    .last_used_version
                    .clone()
                    .unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "技能目录",
                match instance.isolation.agents_home {
                    AgentsHomeMode::Shared => "共享".to_owned(),
                    AgentsHomeMode::Isolated => "隔离".to_owned(),
                },
            ),
            (
                "稳定运行阈值",
                format!("{} 秒", instance.safety.stable_seconds),
            ),
            (
                "失败自动回滚",
                if instance.safety.auto_rollback {
                    "开启"
                } else {
                    "关闭"
                }
                .to_owned(),
            ),
        ];
        let info_card = card()
            .child(card_head("实例信息", hint(self.origin_subtitle(instance))))
            .children(info.into_iter().enumerate().map(|(index, (key, value))| {
                row_item(index == 0)
                    .child(
                        div()
                            .w(px(120.))
                            .flex_shrink_0()
                            .text_color(rgb(TEXT_3))
                            .child(key),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(value),
                    )
            }));

        let records: Vec<&LaunchRecord> = data.history.iter().rev().take(10).collect();
        let history_card = card()
            .child(card_head(
                "启动历史",
                hint(format!("最近 {} 次", records.len())),
            ))
            .when(records.is_empty(), |el| {
                el.child(row_item(true).child(hint("还没有启动记录")))
            })
            .children(records.into_iter().enumerate().map(|(index, record)| {
                let tone = match record.outcome {
                    LaunchOutcome::Running => ChipTone::Info,
                    LaunchOutcome::Exited | LaunchOutcome::Stopped => ChipTone::Good,
                    LaunchOutcome::Crashed => ChipTone::Bad,
                    LaunchOutcome::ForcedStop => ChipTone::Warn,
                };
                let mut meta = format!("PID {}", record.process_id);
                if let Some(secs) = record.runtime_seconds {
                    meta.push_str(&format!(" · 运行 {}", storage::format_duration(secs)));
                }
                if let Some(code) = record.exit_code {
                    meta.push_str(&format!(" · 退出码 {code}"));
                }
                row_item(index == 0)
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
                                    .child(fmt_utc(&record.started_at)),
                            )
                            .child(hint(meta)),
                    )
                    .child(chip(record.outcome.label(), tone))
            }));

        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(info_card)
            .child(history_card)
    }

    pub(super) fn render_plugins_tab(
        &self,
        instance: &InstanceConfig,
        status: &InstanceStatus,
        data: &DetailsData,
        cx: &mut Context<Self>,
    ) -> Div {
        let locked =
            is_running(status) || is_busy(status) || self.plugin_busy.contains(&instance.id);
        let suspects: Vec<String> = data
            .crash
            .as_ref()
            .and_then(|r| r.as_ref())
            .map(|r| r.suspects.clone())
            .unwrap_or_default();
        let enabled = data.plugins.iter().filter(|p| p.enabled).count();

        let header_actions = {
            let (safe_id, copy_id, add_id) = (
                instance.id.clone(),
                instance.id.clone(),
                instance.id.clone(),
            );
            let bisect_id = instance.id.clone();
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(hint(format!("{enabled} / {} 已启用", data.plugins.len())))
                .child(clickable(
                    btn("复制插件组合到…", Tone::Default, true),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = Some(Dialog::CopyPlugins {
                            source_id: copy_id.clone(),
                            target: None,
                        });
                        cx.notify();
                    }),
                ))
                .child(clickable(
                    btn("二分排查…", Tone::Default, true),
                    data.plugins.iter().filter(|p| p.enabled).count() > 1,
                    cx.listener(move |this, _, _, cx| this.start_bisect(bisect_id.clone(), cx)),
                ))
                .child(clickable(
                    btn("从 .tgz 安装…", Tone::Default, true),
                    !locked,
                    cx.listener(move |this, _, _, cx| {
                        this.install_plugin_packages(add_id.clone(), cx)
                    }),
                ))
                .child(clickable(
                    btn("安全模式", Tone::Default, true),
                    !locked && !data.plugins.is_empty(),
                    cx.listener(move |this, _, _, cx| this.enter_safe_mode(safe_id.clone(), cx)),
                ))
        };
        let list = card()
            .child(card_head(
                "已安装插件 · 官方内置插件不在此列",
                header_actions,
            ))
            .when(data.plugins.is_empty(), |el| {
                el.child(row_item(true).child(hint("这个实例还没有安装插件")))
            })
            .children(data.plugins.iter().enumerate().map(|(index, plugin)| {
                let id = instance.id.clone();
                let name = plugin.name.clone();
                let on = plugin.enabled;
                let (uninstall_id, uninstall_name) = (id.clone(), name.clone());
                row_item(index == 0)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(plugin.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT_3))
                                    .child(plugin.version.clone()),
                            ),
                    )
                    .when(suspects.contains(&plugin.name), |el| {
                        el.child(chip("嫌疑", ChipTone::Bad))
                    })
                    .when(!on, |el| el.child(chip("已禁用", ChipTone::Off)))
                    .child(clickable(
                        btn("卸载", Tone::Danger, true),
                        !locked,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = Some(Dialog::UninstallPlugin {
                                instance_id: uninstall_id.clone(),
                                package: uninstall_name.clone(),
                            });
                            cx.notify();
                        }),
                    ))
                    .child(clickable(
                        switch(on),
                        !locked,
                        cx.listener(move |this, _, _, cx| {
                            this.set_plugins_enabled(&id, vec![name.clone()], !on, None, cx);
                        }),
                    ))
            }));

        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .when(locked, |el| {
                el.child(note(
                    "实例正在运行，插件开关暂时锁定（桌面版运行时占用着插件配置）。停止实例后才能启用 / 禁用插件。要加新插件，可停止后在这里用「从 .tgz 安装…」，或到插件市场安装。",
                ))
            })
            .child(list)
    }

    pub(super) fn render_snapshots_tab(
        &self,
        instance: &InstanceConfig,
        status: &InstanceStatus,
        data: &DetailsData,
        cx: &mut Context<Self>,
    ) -> Div {
        let locked = is_running(status) || is_busy(status);
        let create = {
            let id = instance.id.clone();
            clickable(
                btn("＋ 创建快照", Tone::Default, true),
                true,
                cx.listener(move |this, _, window, cx| {
                    this.open_create_snapshot_dialog(id.clone(), window, cx)
                }),
            )
        };

        let list = card()
            .child(card_head("配置快照", create))
            .when(data.snapshots.is_empty(), |el| {
                el.child(row_item(true).child(hint(
                    "还没有快照。每次启动前会对比插件配置，有变化时自动保存一份快照。",
                )))
            })
            .children(data.snapshots.iter().enumerate().map(|(index, snapshot)| {
                let mut detail =
                    format!("{} · 插件 {} 个", snapshot.reason, snapshot.plugins.len());
                detail.push_str(&format!(" · {} 个文件", snapshot.files.len()));
                let mut actions = div().flex().flex_shrink_0().gap(px(8.));
                {
                    let instance_id = instance.id.clone();
                    let snapshot_id = snapshot.id.clone();
                    actions = actions.child(clickable(
                        btn("回滚到这里", Tone::Default, true),
                        !locked,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = Some(Dialog::RestoreSnapshot {
                                instance_id: instance_id.clone(),
                                snapshot_id: snapshot_id.clone(),
                            });
                            cx.notify();
                        }),
                    ));
                }
                if !snapshot.last_good {
                    let instance_id = instance.id.clone();
                    let snapshot_id = snapshot.id.clone();
                    actions = actions.child(clickable(
                        btn("标记为正常", Tone::Default, true),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.mark_snapshot_last_good(&instance_id, &snapshot_id, cx);
                        }),
                    ));
                }
                {
                    let instance_id = instance.id.clone();
                    let snapshot_id = snapshot.id.clone();
                    actions = actions.child(clickable(
                        btn("删除", Tone::Danger, true),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.dialog = Some(Dialog::DeleteSnapshot {
                                instance_id: instance_id.clone(),
                                snapshot_id: snapshot_id.clone(),
                            });
                            cx.notify();
                        }),
                    ));
                }
                row_item(index == 0)
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(14.))
                            .rounded_full()
                            .bg(rgb(if snapshot.last_good { SUCCESS } else { STOPPED })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(fmt_rfc3339(&snapshot.created_at)),
                                    )
                                    .when(snapshot.last_good, |el| {
                                        el.child(chip("上次正常", ChipTone::Good))
                                    }),
                            )
                            .child(hint(detail).truncate()),
                    )
                    .child(actions)
            }));

        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(note(
                "快照只含插件配置，几十 KB；不含会话和数据。回滚前会自动把当前配置另存为一份安全快照。",
            ))
            .when(locked, |el| el.child(note("实例正在运行，停止后才能回滚。")))
            .child(list)
    }

    pub(super) fn render_logs_tab(&self, data: &DetailsData) -> Div {
        let log_path = data.paths.as_ref().and_then(|p| latest_log(&p.marina));
        let entries = log_path
            .as_ref()
            .and_then(|path| tail(path, LOG_LINES).ok())
            .unwrap_or_default();

        let head = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(
                hint(
                    log_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "还没有日志文件".to_owned()),
                )
                .truncate(),
            )
            .when_some(data.paths.as_ref().map(|p| p.marina.clone()), |el, dir| {
                el.child(clickable(
                    btn("打开日志目录", Tone::Default, true),
                    true,
                    move |_, _, _| platform::open_path(&dir),
                ))
            });

        // Only the visible rows are laid out, so 2000 lines stay cheap (§9).
        let count = entries.len();
        let entries = std::rc::Rc::new(entries);
        let rows = uniform_list("log-lines", count, move |range, _, _| {
            range
                .filter_map(|index| entries.get(index))
                .map(|entry| {
                    let level_color = match entry.level {
                        "ERROR" => DANGER,
                        "WARN" => ACCENT,
                        "DEBUG" => STOPPED,
                        _ => INFO,
                    };
                    div()
                        .flex()
                        .gap(px(8.))
                        .h(px(20.))
                        .items_center()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(STOPPED))
                                .child(entry.time.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(44.))
                                .text_color(rgb(level_color))
                                .child(entry.level),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(entry.text.clone()),
                        )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.log_scroll)
        .h(px(560.));
        // Follow new output, but leave the position alone while nothing changes
        // so the user can scroll back.
        if count > 0 && self.log_seen.replace(count) != count {
            self.log_scroll
                .scroll_to_item(count - 1, ScrollStrategy::Bottom);
        }
        let pane = div()
            .flex()
            .flex_col()
            .px(px(18.))
            .py(px(16.))
            .rounded(px(11.))
            .border_1()
            .border_color(rgb(BORDER_SUBTLE))
            .bg(rgb(LOG_BG))
            .font_family(MONO)
            .text_size(px(12.))
            .text_color(rgb(LOG_TEXT))
            .when(count == 0, |el| el.child("暂无日志输出"))
            .when(count > 0, |el| {
                el.child(hint(format!("最近 {count} 行（最多 {LOG_LINES} 行）")))
                    .child(rows)
            });

        div().flex().flex_col().gap(px(12.)).child(head).child(pane)
    }

    pub(super) fn render_settings_tab(
        &self,
        instance: &InstanceConfig,
        status: &InstanceStatus,
        data: &DetailsData,
        cx: &mut Context<Self>,
    ) -> Div {
        let locked = is_running(status) || is_busy(status);
        let id = instance.id.clone();

        let rename = {
            let id = id.clone();
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(instance.name.clone()),
                )
                .child(clickable(
                    btn("重命名", Tone::Default, true),
                    true,
                    cx.listener(move |this, _, window, cx| {
                        this.open_rename_dialog(id.clone(), window, cx)
                    }),
                ))
        };

        let isolated = instance.isolation.agents_home == AgentsHomeMode::Isolated;
        let isolation = {
            let shared_id = id.clone();
            let isolated_id = id.clone();
            segmented()
                .when(locked, |el| el.opacity(0.4))
                .child(
                    clickable(
                        segment("共享", !isolated),
                        !locked && isolated,
                        cx.listener(move |this, _, _, cx| {
                            let result =
                                this.state.repository.set_agents_isolated(&shared_id, false);
                            this.apply_instance_update(result, cx);
                        }),
                    )
                    .opacity(1.),
                )
                .child(
                    clickable(
                        segment("隔离", isolated),
                        !locked && !isolated,
                        cx.listener(move |this, _, _, cx| {
                            let result = this
                                .state
                                .repository
                                .set_agents_isolated(&isolated_id, true);
                            this.apply_instance_update(result, cx);
                        }),
                    )
                    .opacity(1.),
                )
        };

        let safety = instance.safety.clone();
        let stable = {
            let minus_id = id.clone();
            let plus_id = id.clone();
            let minus = safety.clone();
            let plus = safety.clone();
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(8.))
                .child(clickable(
                    stepper_button("−"),
                    safety.stable_seconds > 30,
                    cx.listener(move |this, _, _, cx| {
                        let seconds = minus.stable_seconds.saturating_sub(30).max(30);
                        let result = this.state.repository.set_safety(
                            &minus_id,
                            minus.auto_rollback,
                            seconds,
                        );
                        this.apply_instance_update(result, cx);
                    }),
                ))
                .child(
                    div()
                        .w(px(64.))
                        .text_center()
                        .font_weight(FontWeight::BOLD)
                        .child(format!("{} 秒", safety.stable_seconds)),
                )
                .child(clickable(
                    stepper_button("＋"),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        let result = this.state.repository.set_safety(
                            &plus_id,
                            plus.auto_rollback,
                            plus.stable_seconds + 30,
                        );
                        this.apply_instance_update(result, cx);
                    }),
                ))
        };

        let rollback = {
            let id = id.clone();
            let safety = safety.clone();
            clickable(
                switch(safety.auto_rollback),
                true,
                cx.listener(move |this, _, _, cx| {
                    let result = this.state.repository.set_safety(
                        &id,
                        !safety.auto_rollback,
                        safety.stable_seconds,
                    );
                    this.apply_instance_update(result, cx);
                }),
            )
        };

        let mut form = card()
            .child(card_head("实例设置", div()))
            .child(form_row(true, "实例名称", "只影响启动器里的显示", rename))
            .child(form_row(
                false,
                "图标、颜色与备注",
                if instance.notes.is_empty() {
                    "在列表里更好地区分实例"
                } else {
                    instance.notes.as_str()
                },
                {
                    let id = id.clone();
                    clickable(
                        btn("编辑…", Tone::Default, true),
                        true,
                        cx.listener(move |this, _, window, cx| {
                            this.open_appearance_dialog(id.clone(), window, cx)
                        }),
                    )
                },
            ));
        if let Some(paths) = &data.paths {
            form = form
                .child(form_row(
                    false,
                    "数据目录（DSH_HOME）",
                    "插件、会话、登录、插件数据都放这里",
                    path_box(&paths.home),
                ))
                .child(form_row(
                    false,
                    "窗口数据（userData）",
                    "窗口设置、浏览器存储、崩溃报告",
                    path_box(&paths.electron),
                ));
        }
        form = form
            .child(form_row(
                false,
                "技能目录（~/.agents）",
                "共享：所有实例用同一份；隔离：这个实例单独一份。运行中不能修改。",
                isolation,
            ))
            .child(form_row(
                false,
                "稳定运行多久算「正常」",
                "超过这个时间，启动前的快照会被标记为上次正常状态",
                stable,
            ))
            .child(form_row(
                false,
                "启动失败时自动回滚",
                "开启后无需确认，直接回到上次正常状态",
                rollback,
            ));

        let mut actions = div().flex().gap(px(10.));
        {
            let id = id.clone();
            actions = actions.child(clickable(
                btn("导出实例…", Tone::Default, false),
                true,
                cx.listener(move |this, _, _, cx| this.export_instance(&id, cx)),
            ));
        }
        {
            let id = id.clone();
            actions = actions.child(clickable(
                btn("克隆实例…", Tone::Default, false),
                true,
                cx.listener(move |this, _, window, cx| {
                    this.open_clone_dialog(id.clone(), window, cx)
                }),
            ));
        }
        actions = actions.child(clickable(
            btn("删除实例…", Tone::Danger, false),
            !locked,
            cx.listener(move |this, _, _, cx| {
                this.dialog = Some(Dialog::DeleteInstance {
                    instance_id: id.clone(),
                    mode: storage::DeleteMode::Export,
                });
                cx.notify();
            }),
        ));

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(form)
            .child(actions)
    }
}
