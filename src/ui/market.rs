//! 插件市场 page: browse the awesome-dsh-plugin community list and install a
//! plugin into one stopped instance.

use super::*;
use dsh_marina::market::{self, MarketIndex, MarketPlugin, MarketSort};
use gpui_kit::component::input::InputEvent;

const PAGE_SIZE: usize = 24;

impl MarinaWindow {
    pub(crate) fn open_market(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.market.search.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索插件名、作者或功能，例如 记忆、qq、主题")
            });
            let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.market.page = 0;
                    this.refilter_market(cx);
                }
            });
            self.market.search = Some(input);
            self.market._search_changed = Some(subscription);
        }
        self.refresh_market_installed(cx);
        if self.market.index.is_none() && !self.market.loading {
            self.load_market(false, cx);
        }
    }

    fn load_market(&mut self, force: bool, cx: &mut Context<Self>) {
        self.market.loading = true;
        self.market.error = None;
        cx.notify();
        let root = self.state.repository.data_root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { market::load_index(&root, force).map_err(|e| e.to_string()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.market.loading = false;
                match result {
                    Ok(mut index) => {
                        if let Some(table) = &this.market.stats {
                            market::apply_stats(&mut index, table);
                        }
                        this.market.index = Some(Arc::new(index));
                        this.refilter_market(cx);
                    }
                    Err(e) => this.market.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        if force || (self.market.stats.is_none() && !self.market.stats_loading) {
            self.load_market_stats(force, cx);
        }
    }

    /// Statistics come from a separate, larger file; the list is usable
    /// before they arrive.
    fn load_market_stats(&mut self, force: bool, cx: &mut Context<Self>) {
        self.market.stats_loading = true;
        self.market.stats_failed = false;
        let root = self.state.repository.data_root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let table = cx
                .background_executor()
                .spawn(async move { market::load_stats(&root, force) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.market.stats_loading = false;
                match table {
                    Some(table) => {
                        if let Some(index) = &this.market.index {
                            let mut index = (**index).clone();
                            market::apply_stats(&mut index, &table);
                            this.market.index = Some(Arc::new(index));
                        }
                        this.market.stats = Some(Arc::new(table));
                        this.refilter_market(cx);
                    }
                    None => this.market.stats_failed = true,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refilter_market(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.market.index.clone() else {
            return;
        };
        let query = self
            .market
            .search
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        self.market.hits = Arc::new(market::search(
            &index,
            &query,
            self.market.category.as_deref(),
            self.market.sort,
        ));
        let pages = self.market.hits.len().div_ceil(PAGE_SIZE).max(1);
        self.market.page = self.market.page.min(pages - 1);
        cx.notify();
    }

    /// Which plugins each instance has, to mark them on the cards.
    fn refresh_market_installed(&mut self, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let ids: Vec<String> = self.instances.iter().map(|i| i.id.clone()).collect();
        cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move {
                    ids.into_iter()
                        .filter_map(|id| {
                            let paths = state.repository.paths(&id).ok()?;
                            let names = list_plugins(&paths.home)
                                .ok()?
                                .into_iter()
                                .map(|p| p.name)
                                .collect();
                            Some((id, names))
                        })
                        .collect::<HashMap<_, _>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.market.installed = installed;
                cx.notify();
            });
        })
        .detach();
    }

    fn installed_in(&self, package: &str) -> Vec<String> {
        self.instances
            .iter()
            .filter(|i| {
                self.market
                    .installed
                    .get(&i.id)
                    .is_some_and(|names| names.contains(package))
            })
            .map(|i| i.name.clone())
            .collect()
    }

    pub(super) fn market_plugin(&self, index: usize) -> Option<&MarketPlugin> {
        self.market.index.as_ref()?.plugins.get(index)
    }

    fn open_market_install(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(plugin) = self.market_plugin(index) else {
            return;
        };
        let name = plugin.name.clone();
        // Preselect the instance being looked at, else the only stopped one.
        let usable: Vec<&InstanceConfig> = self
            .instances
            .iter()
            .filter(|i| {
                let status = self.get_instance_status(&i.id);
                !is_running(&status)
                    && !is_busy(&status)
                    && !self
                        .market
                        .installed
                        .get(&i.id)
                        .is_some_and(|n| n.contains(&name))
            })
            .collect();
        let target = self
            .selected_instance
            .clone()
            .filter(|id| usable.iter().any(|i| &i.id == id))
            .or_else(|| (usable.len() == 1).then(|| usable[0].id.clone()));
        self.dialog = Some(Dialog::MarketInstall {
            index,
            target,
            versions: None,
        });
        cx.notify();
        // Resolving versions reads runtime settings and scans runtimes; do it
        // once in the background instead of per instance per render frame.
        let state = self.state.clone();
        let bindings: Vec<(String, String)> = self
            .instances
            .iter()
            .map(|i| (i.id.clone(), i.runtime.dsh_version.clone()))
            .collect();
        cx.spawn(async move |this, cx| {
            let versions = cx
                .background_executor()
                .spawn(async move {
                    let settings = state.runtime_store.load().unwrap_or_default();
                    bindings
                        .into_iter()
                        .map(|(id, binding)| {
                            let version = state
                                .registry
                                .resolve(&binding, &settings)
                                .ok()
                                .map(|r| r.version);
                            (id, version)
                        })
                        .collect::<HashMap<_, _>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Dialog::MarketInstall {
                    index: open,
                    versions: slot,
                    ..
                }) = this.dialog.as_mut()
                    && *open == index
                {
                    *slot = Some(versions);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn install_market_plugin(
        &mut self,
        index: usize,
        instance_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(plugin) = self.market_plugin(index).cloned() else {
            return;
        };
        if self
            .market
            .installing
            .iter()
            .any(|(id, name)| id == &instance_id && name == &plugin.name)
        {
            return;
        }
        let instance_name = self.instance_name_of(&instance_id);
        self.market
            .installing
            .push((instance_id.clone(), plugin.name.clone()));
        self.pending
            .insert(instance_id.clone(), InstanceStatus::Stopping);
        self.last_error = None;
        self.last_success = Some(format!(
            "正在把 {} 安装到「{instance_name}」…",
            plugin.short_title()
        ));
        cx.notify();
        let state = self.state.clone();
        let (spec, title) = (plugin.spec(), plugin.short_title().to_owned());
        let id = instance_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lifecycle::install_market_plugin(&state, &id, &spec, &title) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.market.installing.retain(|(i, n)| !(i == &instance_id && n == &plugin.name));
                this.pending.remove(&instance_id);
                match result {
                    Ok(()) => {
                        this.last_success = Some(format!(
                            "已把 {} {} 安装到「{instance_name}」，下次启动生效。装之前的配置已存为快照，出问题可以回滚",
                            plugin.short_title(),
                            plugin.version
                        ))
                    }
                    Err(e) => this.last_error = Some(e),
                }
                lifecycle::invalidate_disk_usage(&this.state);
                this.refresh_market_installed(cx);
                this.refresh_instances(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn set_market_category(&mut self, category: Option<String>, cx: &mut Context<Self>) {
        self.market.category = category;
        self.market.page = 0;
        self.refilter_market(cx);
    }

    // ------------------------------------------------------------------ view

    pub(super) fn render_market(&self, cx: &mut Context<Self>) -> Div {
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
                    .child(div().text_size(px(22.)).font_weight(FontWeight::EXTRA_BOLD).child(tr("插件市场")))
                    .child(hint("来自 awesome-dsh-plugin 社区精选列表。插件装进你选的实例，只影响那一个实例，装之前会自动存快照")),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(clickable(
                        btn("社区主页", Tone::Default, true),
                        true,
                        cx.listener(|_, _, _, cx| cx.open_url(market::SITE)),
                    ))
                    .child(clickable(
                        btn(if self.market.loading { "刷新中…" } else { "刷新" }, Tone::Default, true),
                        !self.market.loading,
                        cx.listener(|this, _, _, cx| this.load_market(true, cx)),
                    )),
            );

        let body = match (&self.market.index, &self.market.error) {
            (None, Some(error)) => self.render_market_error(error, cx),
            (None, None) => loading_block("正在读取插件列表（约 3 MB，第一次会慢一些）…", "market"),
            (Some(index), error) => {
                let mut body = div().flex().flex_col().gap(px(14.));
                // 已有缓存时刷新失败：保留旧列表，但把原因说出来。
                if let Some(error) = error {
                    body = body.child(note(format!(
                        "刷新插件列表失败，当前显示的是上次的结果：{error}"
                    )));
                }
                body.child(self.render_market_body(index, cx))
            }
        };

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

    fn render_market_error(&self, error: &str, cx: &mut Context<Self>) -> Div {
        card().child(
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
                        .child(tr("插件列表读取失败")),
                )
                .child(
                    div()
                        .max_w(px(560.))
                        .text_center()
                        .text_size(px(12.))
                        .text_color(rgb(t!(DANGER)))
                        .child(error.to_owned()),
                )
                .child(hint(
                    "检查网络或系统代理后重试；也可以在实例的「插件」页从 .tgz 离线安装。",
                ))
                .child(clickable(
                    btn("重试", Tone::Primary, true),
                    !self.market.loading,
                    cx.listener(|this, _, _, cx| this.load_market(true, cx)),
                )),
        )
    }

    fn render_market_body(&self, index: &Arc<MarketIndex>, cx: &mut Context<Self>) -> Div {
        // Category chips, largest first, with counts.
        let counts = index.category_counts();
        let mut categories: Vec<(&str, &str, usize)> = index
            .categories
            .iter()
            .filter_map(|(id, title)| {
                counts
                    .get(id.as_str())
                    .map(|n| (id.as_str(), title.as_str(), *n))
            })
            .collect();
        categories.sort_by_key(|category| std::cmp::Reverse(category.2));
        let category_chip =
            |label: String, active: bool, value: Option<String>, cx: &mut Context<Self>| {
                let chip = div()
                    .px(px(11.))
                    .py(px(5.))
                    .rounded(px(14.))
                    .border_1()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .when(active, |el| {
                        el.border_color(rgb(t!(PRIMARY)))
                            .bg(rgb(t!(PRIMARY_MUTED)))
                            .text_color(rgb(t!(PRIMARY)))
                    })
                    .when(!active, |el| {
                        el.border_color(rgb(t!(BORDER_SUBTLE)))
                            .bg(rgb(t!(BG_CARD)))
                            .text_color(rgb(t!(TEXT_2)))
                            .hover(|s| s.border_color(rgb(t!(BORDER))).text_color(rgb(t!(TEXT))))
                    })
                    .child(label);
                // 已选中的分类再点一次没有意义：不给它指针和点击处理。
                if active {
                    chip
                } else {
                    clickable(
                        chip,
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.set_market_category(value.clone(), cx);
                        }),
                    )
                }
            };
        let mut chips = vec![category_chip(
            format!("全部 {}", index.plugins.len()),
            self.market.category.is_none(),
            None,
            cx,
        )];
        for (id, title, count) in categories {
            chips.push(category_chip(
                format!("{title} {count}"),
                self.market.category.as_deref() == Some(id),
                Some(id.to_owned()),
                cx,
            ));
        }

        let sort_button = |label: &str, sort: MarketSort, cx: &mut Context<Self>| {
            let active = self.market.sort == sort;
            let el = segment(label, active);
            if active {
                el
            } else {
                clickable(
                    el,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.market.sort = sort;
                        this.market.page = 0;
                        this.refilter_market(cx);
                    }),
                )
            }
        };
        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .children(self.market.search.as_ref().map(|input| {
                        Input::new(input)
                            .prefix(
                                svg()
                                    .path("icons/search.svg")
                                    .size(px(14.))
                                    .text_color(rgb(t!(TEXT_3))),
                            )
                            .cleanable(true)
                    })),
            )
            .child(
                segmented()
                    .child(sort_button("下载量", MarketSort::Downloads, cx))
                    .child(sort_button("星标", MarketSort::Stars, cx))
                    .child(sort_button("最近更新", MarketSort::Updated, cx))
                    .child(sort_button("名称", MarketSort::Name, cx)),
            );

        let hits = self.market.hits.clone();
        let pages = hits.len().div_ceil(PAGE_SIZE).max(1);
        let page = self.market.page.min(pages - 1);
        // Two independent columns (masonry): each card goes to the shorter
        // column, so cards keep one gap between them whatever their height.
        let mut columns: [Vec<Div>; 2] = [Vec::new(), Vec::new()];
        let mut heights = [0f32; 2];
        for &i in hits.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
            let Some(plugin) = index.plugins.get(i) else {
                continue;
            };
            let column = usize::from(heights[1] < heights[0]);
            heights[column] += self.estimate_card_height(plugin) + 14.;
            // The row wrapper lets the card's flex_1 fill the column width.
            columns[column].push(
                div()
                    .flex()
                    .w_full()
                    .min_w(px(0.))
                    .child(self.render_market_card(i, plugin, index, cx)),
            );
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
        let grid = div()
            .flex()
            .items_start()
            .w_full()
            .gap(px(14.))
            .child(column(left))
            .child(column(right));

        let status = {
            let mut parts = vec![format!("找到 {} 个插件", hits.len())];
            if index.stale {
                parts.push("网络读取失败，显示的是上次缓存的列表".to_owned());
            } else if let Some(at) = index.fetched_at {
                let at: chrono::DateTime<chrono::Local> = at.into();
                parts.push(format!("列表更新于 {}", at.format("%m-%d %H:%M")));
            }
            if self.market.stats_loading && self.market.stats.is_none() {
                parts.push("正在读取下载量和 Star 数…".to_owned());
            } else if self.market.stats_failed {
                parts.push("下载量和 Star 数暂时读不到".to_owned());
            } else if self.market.stats.is_some() {
                parts.push("下载量为 npm 近 30 天".to_owned());
            }
            hint(parts.join(" · ")).when(index.stale, |el| el.text_color(rgb(t!(ACCENT))))
        };

        let pager = (pages > 1).then(|| {
            let go = |label: &str, target: usize, enabled: bool, cx: &mut Context<Self>| {
                clickable(
                    btn(label.to_owned(), Tone::Default, true),
                    enabled,
                    cx.listener(move |this, _, _, cx| {
                        this.market.page = target;
                        cx.notify();
                    }),
                )
            };
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .pt(px(4.))
                .child(go("首页", 0, page > 0, cx))
                .child(go("上一页", page.saturating_sub(1), page > 0, cx))
                .child(hint(format!("第 {} / {pages} 页", page + 1)))
                .child(go(
                    "下一页",
                    (page + 1).min(pages - 1),
                    page + 1 < pages,
                    cx,
                ))
                .child(go("末页", pages - 1, page + 1 < pages, cx))
        });

        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(toolbar)
            .child(div().flex().flex_wrap().gap(px(6.)).children(chips))
            .child(status)
            .when(hits.is_empty(), |el| {
                el.child(
                    card().child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.))
                            .py(px(40.))
                            .child(svg().path("icons/puzzle.svg").size(px(28.)).text_color(rgb(t!(TEXT_3))))
                            .child(hint("没有符合条件的插件，换个关键词或分类试试")),
                    ),
                )
            })
            .child(grid)
            .children(pager)
            .child(
                hint("列表来自 awesome-dsh-plugin 社区（CC0-1.0）。插件由各自作者维护并以各自的许可发布，从 npm 直接装进你的实例；Marina 不审核也不分发插件代码。装之前会自动存快照，启动失败可以一键回滚。")
                    .text_size(px(11.)),
            )
    }

    fn render_market_card(
        &self,
        i: usize,
        plugin: &MarketPlugin,
        index: &MarketIndex,
        cx: &mut Context<Self>,
    ) -> Div {
        let installed = self.installed_in(&plugin.name);
        let installing = self
            .market
            .installing
            .iter()
            .any(|(_, name)| name == &plugin.name);
        let published = chrono::DateTime::from_timestamp(plugin.published, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default();
        let initial = plugin
            .short_title()
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .chars()
            .next()
            .unwrap_or('?');
        let color = palette_color(&plugin.name);

        let mut meta = Vec::new();
        if !plugin.author().is_empty() {
            meta.push(plugin.author().to_owned());
        }
        meta.push(format!("v{}", plugin.version));
        if !published.is_empty() {
            meta.push(published);
        }

        let stats_row = plugin.stats.as_ref().map(|stats| {
            let figure = |icon: &'static str, value: Option<u64>, tip: &str| {
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(12.))
                    .text_color(rgb(t!(TEXT_2)))
                    .child(svg().path(icon).size(px(13.)).text_color(rgb(t!(TEXT_3))))
                    .child(format!(
                        "{}{tip}",
                        value.map(compact_count).unwrap_or_else(|| "—".to_owned())
                    ))
            };
            let mut capabilities: Vec<&String> = stats.capabilities.iter().collect();
            capabilities.sort_by_key(|c| !market::is_sensitive_capability(c));
            let numbers = div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(14.))
                .child(figure("icons/star.svg", stats.stars, ""))
                .child(figure("icons/download.svg", stats.downloads, " /月"));
            let labels: Vec<(String, ChipTone)> = (!stats.red_lines.is_empty())
                .then(|| ("⚠ 风险提示".to_owned(), ChipTone::Bad))
                .into_iter()
                .chain(capabilities.into_iter().map(|c| {
                    let tone = if market::is_sensitive_capability(c) {
                        ChipTone::Warn
                    } else {
                        ChipTone::Off
                    };
                    (market::capability_label(c).to_owned(), tone)
                }))
                .collect();
            let chips = (!labels.is_empty()).then(|| self.chip_marquee(&plugin.name, labels));
            // Numbers first, then the chips in the space left on the same line.
            div()
                .flex()
                .items_center()
                .gap(px(14.))
                .w_full()
                .min_w(px(0.))
                .child(numbers)
                .children(chips)
        });

        let homepage = if plugin.homepage.is_empty() {
            plugin.repository.clone()
        } else {
            plugin.homepage.clone()
        };
        let install_label = if installing {
            "安装中…"
        } else if installed.is_empty() {
            "安装"
        } else {
            "装到其他实例"
        };

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
            .border_color(rgb(t!(BORDER_SUBTLE)))
            .bg(rgb(t!(BG_CARD)))
            .hover(|s| s.border_color(rgb(t!(BORDER))).bg(rgb(t!(BG_ROW_HOVER))))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .child(badge(&initial.to_uppercase().to_string(), color, 38.))
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
                                            .min_w(px(0.))
                                            .font_weight(FontWeight::BOLD)
                                            .truncate()
                                            .child(plugin.short_title().to_owned()),
                                    )
                                    .when(!installed.is_empty(), |el| {
                                        el.child(chip("已安装", ChipTone::Good))
                                    }),
                            )
                            .child(hint(meta.join(" · ")).min_w(px(0.)).truncate())
                            .when(!plugin.dsh_range.is_empty(), |el| {
                                el.child(
                                    hint(format!("需要 DSH {}", plugin.dsh_range))
                                        .text_size(px(11.))
                                        .min_w(px(0.))
                                        .truncate(),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .max_h(px(36.))
                    .overflow_hidden()
                    .text_size(px(12.5))
                    .text_color(rgb(t!(TEXT_2)))
                    .line_height(px(18.))
                    .line_clamp(2)
                    .child(if plugin.description().is_empty() {
                        "作者没有写介绍".to_owned()
                    } else {
                        plugin.description().to_owned()
                    }),
            )
            .children(stats_row)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .w_full()
                    .min_w(px(0.))
                    .child(chip(
                        index.category_title(&plugin.category).to_owned(),
                        ChipTone::Info,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(t!(TEXT_3)))
                            .font_family(MONO)
                            .child(plugin.name.clone()),
                    )
                    .when(!homepage.is_empty(), |el| {
                        el.child(clickable(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .size(px(30.))
                                .rounded(px(7.))
                                .hover(|s| s.bg(rgb(t!(BG_HOVER))))
                                .child(
                                    svg()
                                        .path("icons/external-link.svg")
                                        .size(px(14.))
                                        .text_color(rgb(t!(TEXT_2))),
                                ),
                            true,
                            cx.listener(move |_, _, _, cx| cx.open_url(&homepage)),
                        ))
                    })
                    .child(clickable(
                        btn(
                            install_label,
                            if installed.is_empty() {
                                Tone::Primary
                            } else {
                                Tone::Default
                            },
                            true,
                        ),
                        !installing && !self.instances.is_empty(),
                        cx.listener(move |this, _, _, cx| this.open_market_install(i, cx)),
                    )),
            )
            .when(!installed.is_empty(), |el| {
                el.child(
                    hint(format!("已装在：{}", installed.join("、")))
                        .text_size(px(11.))
                        .truncate(),
                )
            })
    }
}

impl MarinaWindow {
    /// Rough height of a card, only to pick the shorter column.
    fn estimate_card_height(&self, plugin: &MarketPlugin) -> f32 {
        // Card padding, border, header (title + meta) and the bottom row.
        let mut height = 34. + 44. + 10. + 32.;
        if !plugin.dsh_range.is_empty() {
            height += 20.;
        }
        // Description: one or two lines of about 480 px.
        let width: f32 = plugin
            .description()
            .chars()
            .map(|c| if c.is_ascii() { 7. } else { 12.5 })
            .sum();
        height += 10. + if width > 480. { 36. } else { 18. };
        if plugin.stats.is_some() {
            height += 10. + 26.;
        }
        if !self.installed_in(&plugin.name).is_empty() {
            height += 10. + 16.;
        }
        height
    }

    /// One line of chips.  When they are wider than the card the line
    /// scrolls slowly in a loop instead of being cut off or wrapped.
    fn chip_marquee(&self, key: &str, labels: Vec<(String, ChipTone)>) -> Div {
        let (visible, content) = self
            .market
            .chip_widths
            .borrow()
            .get(key)
            .copied()
            .unwrap_or_default();
        let overflowing = visible > 0. && content > visible + 1.;
        let copy = |labels: &[(String, ChipTone)]| {
            div()
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(4.))
                // Space before the repeated copy; part of the scroll period.
                .when(overflowing, |el| el.pr(px(28.)))
                .children(
                    labels
                        .iter()
                        .map(|(label, tone)| chip(label.clone(), *tone)),
                )
        };
        // Records the painted width: slot 0 the visible row, slot 1 the chips.
        let measure = |slot: usize| {
            let widths = self.market.chip_widths.clone();
            let key = key.to_owned();
            gpui::canvas(
                move |bounds, window, _| {
                    let width = f32::from(bounds.size.width);
                    let mut widths = widths.borrow_mut();
                    let entry = widths.entry(key).or_default();
                    let value = if slot == 0 {
                        &mut entry.0
                    } else {
                        &mut entry.1
                    };
                    if (*value - width).abs() > 0.5 {
                        *value = width;
                        window.on_next_frame(|window, _| window.refresh());
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
        };
        let track = div()
            .flex()
            .flex_shrink_0()
            .child(copy(&labels).child(measure(1)));
        let row = if overflowing {
            // `content` includes the trailing space, so moving by it lands the
            // second copy exactly where the first started.
            let period = content;
            let seconds = (period / 32.).max(4.);
            div().child(track.child(copy(&labels)).with_animation(
                gpui::ElementId::Name(format!("caps-{key}").into()),
                gpui::Animation::new(std::time::Duration::from_secs_f32(seconds)).repeat(),
                move |el, delta| el.relative().left(px(-period * delta)),
            ))
        } else {
            div().child(track)
        };
        div()
            .relative()
            .flex()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .child(row.flex().flex_shrink_0())
            .child(measure(0))
    }
}

/// `1234` → `1.2k`, `1234567` → `1.2M`.
pub(super) fn compact_count(value: u64) -> String {
    match value {
        0..1_000 => value.to_string(),
        1_000..1_000_000 => format!("{:.1}k", value as f64 / 1_000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}
