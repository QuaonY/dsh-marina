//! 设置页的「回收站」区：列出「保留数据后删除」的实例（保留 30 天），
//! 支持一键恢复与彻底删除。列表在后台加载，渲染路径只读缓存。

use super::*;

impl MarinaWindow {
    /// Serve the cached trash list, warming it in the background on a miss.
    pub(super) fn cached_trash(&self, cx: &mut Context<Self>) -> Vec<storage::TrashEntry> {
        if self.trash_cache.borrow().is_none() && !self.trash_warming.get() {
            self.trash_warming.set(true);
            let state = self.state.clone();
            cx.spawn(async move |this, cx| {
                let list = cx
                    .background_executor()
                    .spawn(async move {
                        storage::list_trash(state.repository.data_root()).unwrap_or_else(|e| {
                            dsh_marina::applog::warn(&format!("读取回收站失败：{e}"));
                            Vec::new()
                        })
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.trash_cache.borrow_mut().replace(list);
                    this.trash_warming.set(false);
                    cx.notify();
                });
            })
            .detach();
        }
        self.trash_cache.borrow().clone().unwrap_or_default()
    }

    pub(super) fn render_trash(&self, cx: &mut Context<Self>) -> Div {
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
                            .child("回收站"),
                    )
                    .child(
                        hint("删除时选择「保留数据后删除」的实例，在这里保留 30 天")
                            .text_size(px(13.)),
                    ),
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
                .child(self.render_trash_section(cx))
                .overflow_y_scrollbar(),
        )
    }

    pub(super) fn render_trash_section(&self, cx: &mut Context<Self>) -> Div {
        let entries = self.cached_trash(cx);
        let count = entries.len();
        let rows: Vec<Div> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| self.render_trash_row(entry, index == 0, cx))
            .collect();
        card()
            .child(card_head("回收站", hint(format!("{count} 项 · 保留 30 天"))))
            .when(rows.is_empty(), |el| {
                el.child(form_row(
                    true,
                    "暂无可恢复的实例",
                    "删除实例时选择「保留数据后删除」，就会在这里保留 30 天",
                    div(),
                ))
            })
            .children(rows)
            .child(
                div()
                    .px(px(SP_5))
                    .py(px(SP_3))
                    .border_t_1()
                    .border_color(rgb(t!(BORDER_SUBTLE)))
                    .child(hint(
                        "恢复会重建实例并放回 home 数据与配置；登录状态等其余目录在删除时已移到系统回收站，无法在这里恢复。",
                    )),
            )
    }

    fn render_trash_row(
        &self,
        entry: &storage::TrashEntry,
        first: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let restore_key = entry.key.clone();
        let purge_key = entry.key.clone();
        let purge_name = entry.name.clone();
        let meta = format!(
            "删除于 {} · {} · {} 到期",
            fmt_rfc3339(&entry.deleted_at),
            storage::format_bytes(entry.size_bytes),
            fmt_rfc3339(&entry.expires_at),
        );
        row_item(first)
            .child(badge(&initial(&entry.name), hex_color(&entry.color), 34.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(entry.name.clone()),
                    )
                    .child(hint(meta).truncate()),
            )
            .child(clickable(
                btn("恢复", Tone::Primary, true),
                true,
                cx.listener(move |this, _, _, cx| {
                    this.restore_trash_entry(restore_key.clone(), cx)
                }),
            ))
            .child(clickable(
                btn("彻底删除", Tone::Danger, true),
                true,
                cx.listener(move |this, _, _, cx| {
                    this.dialog = Some(Dialog::PurgeTrash {
                        key: purge_key.clone(),
                        name: purge_name.clone(),
                    });
                    cx.notify();
                }),
            ))
    }

    /// Rebuild an instance from a kept trash entry, then jump to its details.
    pub(crate) fn restore_trash_entry(&mut self, key: String, cx: &mut Context<Self>) {
        let state = self.state.clone();
        self.last_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    storage::restore_trash(&state.repository, &key).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(e) => this.last_error = Some(format!("恢复失败：{e}")),
                    Ok(config) => {
                        this.last_success = Some(format!("已恢复「{}」", config.name));
                        this.trash_cache.borrow_mut().take();
                        this.invalidate_runtimes_cache();
                        this.invalidate_details_cache();
                        this.refresh_instances(cx);
                        this.select_instance(config.id, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Permanently remove one trash entry (confirmed in a dialog).
    pub(crate) fn purge_trash_entry(&mut self, key: String, cx: &mut Context<Self>) {
        let state = self.state.clone();
        self.last_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    storage::purge_trash_entry(state.repository.data_root(), &key)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(e) => this.last_error = Some(format!("删除失败：{e}")),
                    Ok(()) => {
                        this.last_success = Some("已彻底删除该回收站条目".to_owned());
                        this.trash_cache.borrow_mut().take();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
