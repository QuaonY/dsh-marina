//! 首页, 插件市场 and 整合包 pages from the sidebar.

use super::*;
use dsh_marina::catalog::{self, CatalogEntry};

impl MarinaWindow {
    /// Sidebar navigation.  The 整合包 page loads its lists when opened.
    pub(crate) fn show_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        self.active_view = view;
        if view == View::Home {
            self.selected_instance = None;
        }
        if view == View::Packs {
            self.open_packs(window, cx);
        }
        if view == View::Market {
            self.open_market(window, cx);
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- 首页

    pub(super) fn render_home(&self, cx: &mut Context<Self>) -> Div {
        let total = self.instances.len();
        let running = self
            .instances
            .iter()
            .filter(|i| is_running(&self.get_instance_status(&i.id)))
            .count();
        let crashed = self
            .instances
            .iter()
            .filter(|i| self.get_instance_status(&i.id) == InstanceStatus::Crashed)
            .count();
        let memory: u64 = self.memory.values().sum();

        let stat_card = |label: &str, value: String, color: u32| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(6.))
                .p(px(18.))
                .rounded(px(12.))
                .border_1()
                .border_color(rgb(BORDER_SUBTLE))
                .bg(rgb(BG_CARD))
                .child(hint(tr(label)))
                .child(
                    div()
                        .text_size(px(24.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(color))
                        .child(value),
                )
        };
        let stats = div()
            .flex()
            .gap(px(14.))
            .child(stat_card("实例", total.to_string(), TEXT))
            .child(stat_card("运行中", running.to_string(), SUCCESS))
            .child(stat_card(
                "需要处理",
                crashed.to_string(),
                if crashed > 0 { DANGER } else { TEXT_3 },
            ))
            .child(stat_card("内存占用", storage::format_bytes(memory), TEXT));

        let rows = self.instances.iter().enumerate().map(|(index, instance)| {
            let status = self.get_instance_status(&instance.id);
            let (open_id, start_id) = (instance.id.clone(), instance.id.clone());
            let control = if is_running(&status) {
                clickable(
                    btn("切到窗口", Tone::Default, true),
                    true,
                    cx.listener(move |this, _, _, cx| this.focus_instance(&start_id, cx)),
                )
            } else {
                clickable(
                    btn("启动", Tone::Primary, true),
                    !is_busy(&status),
                    cx.listener(move |this, _, _, cx| {
                        this.start_instance(start_id.clone(), false, cx)
                    }),
                )
            };
            row_item(index == 0)
                .child(badge(
                    &badge_text(instance),
                    hex_color(&instance.color),
                    36.,
                ))
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
                                .truncate()
                                .child(instance.name.clone()),
                        )
                        .child(hint(runtime_label(&instance.runtime.dsh_version)).truncate()),
                )
                .child(status_pill(&status))
                .child(clickable(
                    btn("详情", Tone::Default, true),
                    true,
                    cx.listener(move |this, _, _, cx| this.select_instance(open_id.clone(), cx)),
                ))
                .child(control)
        });
        let list = card()
            .child(card_head("全部实例", hint(format!("共 {total} 个"))))
            .when(total == 0, |el| {
                el.child(div().p(px(20.)).child(hint(
                    "还没有实例。新建一个，或者到「整合包」里一键使用社区分享的配置组合。",
                )))
            })
            .children(rows);

        let page = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.))
                    .px(px(32.))
                    .py(px(28.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(16.))
                            .child(brand_mark(48.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .text_size(px(22.))
                                            .font_weight(FontWeight::EXTRA_BOLD)
                                            .child(tr("欢迎使用 DSH Marina")),
                                    )
                                    .child(hint(
                                        "每个实例有自己的插件和数据，互不影响；出问题可以一键回滚",
                                    )),
                            ),
                    )
                    .child(stats)
                    .child(list),
            )
            .overflow_y_scrollbar();
        div().flex().flex_col().flex_1().min_h_0().child(page)
    }

    // -------------------------------------------------------------- 整合包

    fn open_packs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.packs.url_input.is_none() {
            let url = self
                .state
                .settings_store
                .load()
                .map(|s| s.pack_catalog_url)
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("整合包目录地址，例如 https://example.com/marina/catalog.json")
                    .default_value(url)
            });
            self.packs.url_input = Some(input);
        }
        if self.packs.search.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索名称、描述、作者或插件，例如 写作、翻译")
            });
            self.packs.search = Some(input);
        }
        if self.packs.catalog.is_none() {
            self.refresh_catalog(cx);
        }
    }

    fn catalog_url(&self, cx: &App) -> String {
        self.packs
            .url_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    /// Filter catalog entries by search text and the selected chip.
    fn filter_packs(&self, entries: &[CatalogEntry], cx: &App) -> Vec<CatalogEntry> {
        let search_text = self
            .packs
            .search
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();

        let mut filtered: Vec<_> = entries
            .iter()
            .filter(|entry| {
                if search_text.is_empty() {
                    return true;
                }
                entry.name.to_lowercase().contains(&search_text)
                    || entry.description.to_lowercase().contains(&search_text)
                    || entry.author.to_lowercase().contains(&search_text)
                    || entry
                        .plugins
                        .iter()
                        .any(|p| p.to_lowercase().contains(&search_text))
            })
            .cloned()
            .collect();

        match self.packs.filter {
            crate::PacksFilter::All => {}
            crate::PacksFilter::Downloaded => {
                filtered.retain(|entry| self.packs.downloaded.contains(&entry.id))
            }
        }

        filtered
    }

    /// Save the address and load the catalog.
    pub(super) fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        let url = self.catalog_url(cx);
        if self
            .state
            .settings_store
            .load()
            .is_ok_and(|s| s.pack_catalog_url != url)
        {
            let saved = url.clone();
            self.update_settings(move |s| s.pack_catalog_url = saved, cx);
        }
        if url.is_empty() {
            self.packs.catalog = None;
            cx.notify();
            return;
        }
        self.packs.loading = true;
        cx.notify();
        let root = self.state.repository.data_root().to_path_buf();
        cx.spawn(async move |this, cx| {
            // 顺手校验一遍本地已有的整合包，渲染时不再重算哈希。
            let (result, downloaded) = cx
                .background_executor()
                .spawn(async move {
                    let result = catalog::fetch_catalog(&url).map_err(|e| e.to_string());
                    // A failed refresh must not forget which packs are already
                    // on disk; only a successful listing tells us that.
                    let downloaded: Option<std::collections::HashSet<String>> =
                        result.as_ref().ok().map(|entries| {
                            entries
                                .iter()
                                .filter(|entry| catalog::is_downloaded(&root, entry))
                                .map(|entry| entry.id.clone())
                                .collect()
                        });
                    (result, downloaded)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.packs.loading = false;
                if let Some(downloaded) = downloaded {
                    this.packs.downloaded = downloaded;
                }
                this.packs.catalog = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Download (if needed) and import `entry` as a new instance.
    fn use_catalog_pack(&mut self, entry: CatalogEntry, cx: &mut Context<Self>) {
        if self.packs.downloading.is_some() || self.packs.importing.is_some() {
            return;
        }
        let address = self.catalog_url(cx);
        let key = format!("pack:{}", entry.id);
        let Some((progress, cancel)) = self.push_job(
            key.clone(),
            format!("整合包「{}」", entry.name),
            entry.size,
            cx,
        ) else {
            return;
        };
        self.packs.downloading = Some((entry.id.clone(), progress.clone(), cancel.clone()));
        self.last_error = None;
        cx.notify();

        let downloaded_id = entry.id.clone();
        let root = self.state.repository.data_root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let worker_cancel = cancel.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    catalog::download_pack(
                        &root,
                        &address,
                        &entry,
                        &worker_cancel,
                        |done, total| {
                            if let Ok(mut slot) = progress.lock() {
                                *slot = (done, total);
                            }
                        },
                    )
                    .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.packs.downloading = None;
                this.finish_job(&key, cx);
                match result {
                    Ok((path, _)) => {
                        this.packs.downloaded.insert(downloaded_id);
                        this.import_pack_file(path, cx);
                    }
                    Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {}
                    Err(e) => this.last_error = Some(format!("下载整合包失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_pack_download(&mut self, cx: &mut Context<Self>) {
        if let Some((_, _, cancel)) = &self.packs.downloading {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        cx.notify();
    }

    /// 删除已下载的整合包文件；已经创建的实例不受影响。
    fn delete_downloaded_pack(&mut self, entry: CatalogEntry, cx: &mut Context<Self>) {
        let root = self.state.repository.data_root().to_path_buf();
        let path = catalog::local_path(&root, &entry);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.packs.downloaded.remove(&entry.id);
                self.last_error = None;
                self.last_success = Some(format!("已删除整合包「{}」的本地文件", entry.name));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.packs.downloaded.remove(&entry.id);
                self.last_error = None;
                self.last_success = Some(format!("整合包「{}」的本地文件已经不在了", entry.name));
            }
            Err(e) => self.last_error = Some(format!("删除整合包「{}」失败：{e}", entry.name)),
        }
        cx.notify();
    }

    /// 一键复用: a new instance from a pack file.  The pack's DSH version is
    /// kept when it is installed; otherwise the instance follows the newest.
    pub(crate) fn import_pack_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.packs.importing = Some(label);
        self.last_error = None;
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = dsh_marina::packs::import_pack(&state.repository, &path, None).map_err(|e| e.to_string())?;
                    let wanted = config.runtime.dsh_version.clone();
                    let installed = state.registry.scan().unwrap_or_default().iter().any(|r| r.version == wanted);
                    if !wanted.is_empty() && wanted != LATEST_STABLE && wanted != EXTERNAL_RUNTIME_ID && !installed {
                        let config = state
                            .repository
                            .set_runtime_binding(&config.id, LATEST_STABLE)
                            .map_err(|e| e.to_string())?;
                        return Ok::<_, String>((config, Some(wanted)));
                    }
                    Ok((config, None))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.packs.importing = None;
                match result {
                    Ok((config, missing)) => {
                        lifecycle::invalidate_disk_usage(&this.state);
                        this.refresh_instances(cx);
                        this.last_success = Some(match missing {
                            Some(version) => format!(
                                "已用整合包创建实例「{}」。整合包基于 DSH {version}，本机没有这个版本，先用最新稳定版",
                                config.name
                            ),
                            None => format!("已用整合包创建实例「{}」", config.name),
                        });
                        let outside = lifecycle::outside_paths(&this.state, &config.id);
                        if !outside.is_empty() {
                            this.last_error = Some(format!(
                                "「{}」的插件配置里有指向实例目录外的路径：{}",
                                config.name,
                                outside.iter().take(3).cloned().collect::<Vec<_>>().join("；")
                            ));
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

    /// Pick a pack file from disk, keep a copy in `packs/` and import it.
    fn import_pack_from_disk(&mut self, cx: &mut Context<Self>) {
        let root = self.state.repository.data_root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move {
                    let path = rfd::FileDialog::new()
                        .set_title("选择整合包")
                        .add_filter("Marina 整合包", &["zip"])
                        .pick_file()?;
                    let result = (|| {
                        let manifest =
                            dsh_marina::packs::preview_pack(&path).map_err(|e| e.to_string())?;
                        let dir = catalog::packs_dir(&root);
                        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                        let name = storage::sanitize_file_name(&format!(
                            "{}.zip",
                            catalog::slug(&manifest.name)
                        ));
                        let target = dir.join(name);
                        // 同名整合包再次导入时用新文件覆盖缓存，
                        // 否则用户选的明明是 v2，导入的却还是上次缓存的 v1。
                        fs::copy(&path, &target).map_err(|e| e.to_string())?;
                        Ok::<_, String>(target)
                    })();
                    Some(result)
                })
                .await;
            let _ = this.update(cx, |this, cx| match picked {
                None => {}
                Some(Ok(path)) => {
                    this.import_pack_file(path, cx);
                }
                Some(Err(e)) => {
                    this.last_error = Some(format!("不是有效的整合包：{e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_packs(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.packs.downloading.is_some() || self.packs.importing.is_some();

        let header = div()
            .flex()
            .items_end()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .child(div().text_size(px(22.)).font_weight(FontWeight::EXTRA_BOLD).child(tr("整合包")))
                    .child(hint("整合包由社区用户分享，Marina 不审核内容；下载后一键创建独立的新实例，导入前会检查文件，不影响已有实例")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(8.))
                    .when_some(self.github_auth.as_ref().map(|auth| auth.username.clone()), |el, user| {
                        el.child(chip(format!("已连接 GitHub · {user}"), ChipTone::Good))
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(clickable(
                                btn("导入文件", Tone::Default, true),
                                !busy,
                                cx.listener(|this, _, _, cx| this.import_pack_from_disk(cx)),
                            ))
                            .when(!self.instances.is_empty(), |el| {
                                el.child(clickable(
                                    btn("分享整合包", Tone::Primary, true),
                                    !busy,
                                    cx.listener(|this, _, window, cx| {
                                        this.open_share_pack_dialog(crate::ShareMode::Community, window, cx);
                                    }),
                                ))
                            })
                            .child(clickable(
                                btn("社区设置", Tone::Default, true),
                                true,
                                cx.listener(|this, _, _, cx| {
                                    this.dialog = Some(crate::Dialog::CommunitySettings);
                                    cx.notify();
                                }),
                            ))
                            .child(clickable(
                                btn(if self.packs.loading { "刷新中…" } else { "刷新" }, Tone::Default, true),
                                !self.packs.loading,
                                cx.listener(|this, _, _, cx| this.refresh_catalog(cx)),
                            )),
                    ),
            );

        let catalog_entries = match &self.packs.catalog {
            Some(Ok(entries)) => Some(self.filter_packs(entries, cx)),
            _ => None,
        };
        let all_entries: &[CatalogEntry] = match &self.packs.catalog {
            Some(Ok(entries)) => entries,
            _ => &[],
        };
        let downloaded_count = all_entries
            .iter()
            .filter(|e| self.packs.downloaded.contains(&e.id))
            .count();

        let filter_chip = |label: String, filter: crate::PacksFilter, cx: &mut Context<Self>| {
            let active = self.packs.filter == filter;
            clickable(
                div()
                    .px(px(11.))
                    .py(px(5.))
                    .rounded(px(14.))
                    .border_1()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .when(active, |el| {
                        el.border_color(rgb(PRIMARY))
                            .bg(rgb(PRIMARY_MUTED))
                            .text_color(rgb(PRIMARY))
                    })
                    .when(!active, |el| {
                        el.border_color(rgb(BORDER_SUBTLE))
                            .bg(rgb(BG_CARD))
                            .text_color(rgb(TEXT_2))
                            .hover(|s| s.border_color(rgb(BORDER)).text_color(rgb(TEXT)))
                    })
                    .child(label),
                true,
                cx.listener(move |this, _, _, cx| {
                    let next = if this.packs.filter == filter {
                        crate::PacksFilter::All
                    } else {
                        filter
                    };
                    if this.packs.filter != next {
                        this.packs.filter = next;
                        cx.notify();
                    }
                }),
            )
        };

        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .children(self.packs.search.as_ref().map(|input| {
                        Input::new(input)
                            .prefix(
                                svg()
                                    .path("icons/search.svg")
                                    .size(px(14.))
                                    .text_color(rgb(TEXT_3)),
                            )
                            .cleanable(true)
                    })),
            )
            .child(div().flex_shrink_0().child(filter_chip(
                format!("已下载 {downloaded_count}"),
                crate::PacksFilter::Downloaded,
                cx,
            )));

        let catalog_body = match (&self.packs.catalog, &self.packs.loading) {
            (Some(Err(e)), _) => card().child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .px(px(24.))
                    .py(px(40.))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child("整合包列表读取失败"),
                    )
                    .child(
                        div()
                            .max_w(px(560.))
                            .text_center()
                            .text_size(px(12.))
                            .text_color(rgb(DANGER))
                            .child(e.clone()),
                    )
                    .child(hint(
                        "检查网络或系统代理后重试；也可以在「社区设置」里换一个目录地址。",
                    ))
                    .child(clickable(
                        btn("重试", Tone::Primary, true),
                        !self.packs.loading,
                        cx.listener(|this, _, _, cx| this.refresh_catalog(cx)),
                    )),
            ),
            (None, true) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.))
                .py(px(60.))
                .child(
                    svg()
                        .path("icons/loader-circle.svg")
                        .size(px(26.))
                        .text_color(rgb(PRIMARY)),
                )
                .child(hint("正在读取社区整合包列表…")),
            (None, false) => card().child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .px(px(24.))
                    .py(px(40.))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child("还没有加载整合包列表"),
                    )
                    .child(hint(
                        "点右上角「刷新」从社区仓库读取，或在「社区设置」里填一个自定义地址。",
                    ))
                    .child(clickable(
                        btn("刷新", Tone::Primary, true),
                        true,
                        cx.listener(|this, _, _, cx| this.refresh_catalog(cx)),
                    )),
            ),
            (Some(Ok(_)), _) => match &catalog_entries {
                Some(entries) if entries.is_empty() => card().child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.))
                        .px(px(24.))
                        .py(px(40.))
                        .child(
                            svg()
                                .path("icons/inbox.svg")
                                .size(px(28.))
                                .text_color(rgb(TEXT_3)),
                        )
                        .child(hint(match self.packs.filter {
                            crate::PacksFilter::All => "没有符合条件的整合包，换个关键词试试",
                            crate::PacksFilter::Downloaded => {
                                "还没有下载过整合包，点任意整合包上的「下载并复用」会把文件下载到本地"
                            }
                        })),
                ),
                Some(entries) => {
                    if self.packs.filter == crate::PacksFilter::Downloaded {
                        self.render_downloaded_list(entries, busy, cx)
                    } else {
                        self.render_packs_grid(entries, busy, cx)
                    }
                }
                None => div().p(px(20.)).child(hint("目录里暂时还没有整合包。")),
            },
        };

        let result_count = catalog_entries.as_ref().map(|e| e.len()).unwrap_or(0);
        let status = {
            let count = if result_count == all_entries.len() {
                format!("找到 {result_count} 个整合包")
            } else {
                format!("显示 {result_count} / 共 {} 个", all_entries.len())
            };
            let mut parts = vec![count];
            if self.packs.loading {
                parts.push("正在刷新…".to_owned());
            } else if matches!(self.packs.catalog, Some(Ok(_))) {
                parts.push("来自社区仓库".to_owned());
            }
            hint(parts.join(" · "))
        };

        // Main page layout
        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .when_some(self.last_error.clone(), |el, err| {
                el.child(
                    div()
                        .px(px(16.))
                        .py(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(ERROR_BG))
                        .border_1()
                        .border_color(rgb(ERROR_BORDER))
                        .child(div().text_color(rgb(DANGER)).text_size(px(13.)).child(err)),
                )
            })
            .when_some(self.last_success.clone(), |el, msg| {
                el.child(
                    div()
                        .px(px(16.))
                        .py(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(SUCCESS_BG))
                        .border_1()
                        .border_color(rgb(SUCCESS_BORDER))
                        .child(div().text_color(rgb(SUCCESS)).text_size(px(13.)).child(msg)),
                )
            })
            .when_some(self.packs.importing.clone(), |el, name| {
                el.child(note(format!("正在用 {name} 创建实例…")))
            })
            .child(toolbar)
            .child(status)
            .child(catalog_body);

        let page = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(18.))
                    .px(px(32.))
                    .py(px(28.))
                    .child(header)
                    .child(body),
            )
            .overflow_y_scrollbar();

        div().flex().flex_col().flex_1().min_h_0().child(page)
    }

    /// 卡片/列表行的主要操作：下载中显示进度，否则是一键复用。
    fn pack_primary_action(
        &self,
        entry: &CatalogEntry,
        downloaded: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Some((_, progress, _)) = self
            .packs
            .downloading
            .as_ref()
            .filter(|(id, _, _)| id == &entry.id)
        {
            let (done, total) = progress.lock().map(|p| *p).unwrap_or((0, 0));
            return div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(10.))
                .child(hint(if total > 0 {
                    format!("{}%", done * 100 / total.max(1))
                } else {
                    storage::format_bytes(done)
                }))
                .child(clickable(
                    btn("取消", Tone::Default, true),
                    true,
                    cx.listener(|this, _, _, cx| this.cancel_pack_download(cx)),
                ));
        }
        let entry = entry.clone();
        clickable(
            btn(
                if downloaded {
                    "一键复用"
                } else {
                    "下载并复用"
                },
                Tone::Primary,
                true,
            ),
            !busy,
            cx.listener(move |this, _, _, cx| this.use_catalog_pack(entry.clone(), cx)),
        )
    }

    /// 卡片上的「详情」：打开该整合包的插件清单。
    fn pack_details_link(&self, entry: &CatalogEntry, cx: &mut Context<Self>) -> Div {
        let name = entry.name.clone();
        let author = entry.author.clone();
        let description = entry.description.clone();
        let dsh_version = entry.dsh_version.clone();
        let plugins = entry.plugins.clone();
        let size = entry.size;
        clickable(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(3.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(PRIMARY))
                .hover(|s| s.text_color(rgb(PRIMARY_HOVER)))
                .child("详情")
                .child("›"),
            true,
            cx.listener(move |this, _, _, cx| {
                this.dialog = Some(crate::Dialog::PackPlugins {
                    name: name.clone(),
                    author: author.clone(),
                    description: description.clone(),
                    dsh_version: dsh_version.clone(),
                    plugins: plugins.clone(),
                    size,
                });
                cx.notify();
            }),
        )
    }

    /// 「已下载」筛选下的列表视图：每行显示整合包信息，可以查看详情、复用或删除。
    fn render_downloaded_list(
        &self,
        entries: &[CatalogEntry],
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let rows = entries.iter().enumerate().map(|(index, entry)| {
            let reuse_entry = entry.clone();
            let delete_entry = entry.clone();
            let details_entry = entry.clone();

            row_item(index == 0)
                .child(badge(
                    &initial(&entry.name),
                    pack_badge_color(&entry.id),
                    36.,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(5.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .truncate()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(entry.name.clone()),
                                )
                                .child(hint(if entry.author.is_empty() {
                                    "社区分享".to_owned()
                                } else {
                                    format!("分享者 {}", entry.author)
                                })),
                        )
                        .child(
                            div()
                                .w_full()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT_2))
                                .child(if entry.description.is_empty() {
                                    "作者没有写介绍".to_owned()
                                } else {
                                    entry.description.clone()
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .flex_wrap()
                                .gap(px(6.))
                                .child(chip(
                                    format!("{} 个插件", entry.plugins.len()),
                                    ChipTone::Info,
                                ))
                                .when(entry.size > 0, |el| {
                                    el.child(chip(storage::format_bytes(entry.size), ChipTone::Off))
                                })
                                .when(!entry.dsh_version.is_empty(), |el| {
                                    el.child(chip(
                                        format!("DSH {}", entry.dsh_version),
                                        ChipTone::Off,
                                    ))
                                })
                                .child(chip("已下载", ChipTone::Good)),
                        ),
                )
                .child(clickable(
                    btn("详情", Tone::Default, true),
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.dialog = Some(crate::Dialog::PackPlugins {
                            name: details_entry.name.clone(),
                            author: details_entry.author.clone(),
                            description: details_entry.description.clone(),
                            dsh_version: details_entry.dsh_version.clone(),
                            plugins: details_entry.plugins.clone(),
                            size: details_entry.size,
                        });
                        cx.notify();
                    }),
                ))
                .child(clickable(
                    btn("复用", Tone::Primary, true),
                    !busy,
                    cx.listener(move |this, _, _, cx| {
                        this.use_catalog_pack(reuse_entry.clone(), cx)
                    }),
                ))
                .child(clickable(
                    btn("删除", Tone::Danger, true),
                    !busy,
                    cx.listener(move |this, _, _, cx| {
                        this.delete_downloaded_pack(delete_entry.clone(), cx)
                    }),
                ))
        });

        card()
            .child(card_head(
                "已下载整合包",
                hint(format!("共 {} 个", entries.len())),
            ))
            .children(rows)
    }

    fn render_packs_grid(
        &self,
        entries: &[CatalogEntry],
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let cards: Vec<Div> = entries
            .iter()
            .map(|entry| {
                let downloaded = self.packs.downloaded.contains(&entry.id);
                let author = if entry.author.is_empty() {
                    "社区分享".to_owned()
                } else {
                    format!("分享者 {}", entry.author)
                };
                let plugin_count = entry.plugins.len();

                let stats = div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(6.))
                    .child(chip(format!("{plugin_count} 个插件"), ChipTone::Info))
                    .when(entry.size > 0, |el| {
                        el.child(chip(storage::format_bytes(entry.size), ChipTone::Off))
                    })
                    .when(!entry.dsh_version.is_empty(), |el| {
                        el.child(chip(format!("DSH {}", entry.dsh_version), ChipTone::Off))
                    });

                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .gap(px(10.))
                    .p(px(16.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(BORDER_SUBTLE))
                    .bg(rgb(BG_CARD))
                    .hover(|s| s.border_color(rgb(BORDER)).bg(rgb(BG_ROW_HOVER)))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(12.))
                            .child(badge(
                                &initial(&entry.name),
                                pack_badge_color(&entry.id),
                                38.,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .gap(px(3.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .font_weight(FontWeight::BOLD)
                                                    .truncate()
                                                    .child(entry.name.clone()),
                                            )
                                            .when(downloaded, |el| {
                                                el.child(chip("已下载", ChipTone::Good))
                                            }),
                                    )
                                    .child(hint(author).min_w(px(0.)).truncate()),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .min_h(px(36.))
                            .max_h(px(36.))
                            .overflow_hidden()
                            .text_size(px(12.5))
                            .text_color(rgb(TEXT_2))
                            .line_height(px(18.))
                            .line_clamp(2)
                            .child(if entry.description.is_empty() {
                                "作者没有写介绍".to_owned()
                            } else {
                                entry.description.clone()
                            }),
                    )
                    .child(stats)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .w_full()
                            .min_w(px(0.))
                            .pt(px(2.))
                            .child(self.pack_details_link(entry, cx))
                            .child(div().flex_1())
                            .child(self.pack_primary_action(entry, downloaded, busy, cx)),
                    )
            })
            .collect();

        // 两列排布，卡片按顺序交替落到左右两列。
        let mut columns: [Vec<Div>; 2] = [Vec::new(), Vec::new()];
        for (index, card) in cards.into_iter().enumerate() {
            columns[index % 2].push(card);
        }
        let [left, right] = columns;
        let column = |cards: Vec<Div>| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(14.))
                .children(cards)
        };
        div()
            .flex()
            .items_start()
            .w_full()
            .gap(px(14.))
            .child(column(left))
            .child(column(right))
    }
}

/// 卡片徽标的颜色：按整合包 ID 稳定取一个。
fn pack_badge_color(id: &str) -> u32 {
    palette_color(id)
}
