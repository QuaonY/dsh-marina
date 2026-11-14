//! Main window rendering, styled after `DSH Marina 设计方案/design/ui-preview.html`.

mod dialogs;
mod market;
mod packs;
mod settings;
mod tabs;

pub(crate) use settings::THIRD_PARTY_LICENSES;

use super::*;
use dsh_marina::history::LaunchRecord;
use dsh_marina::i18n::tr;
use dsh_marina::instances::{AgentsHomeMode, InstancePaths, OriginKind};
use dsh_marina::plugins::PluginInfo;
use dsh_marina::snapshots::SnapshotInfo;

// Light theme tokens from ui-preview.html.
const BG: u32 = 0xfafbfc;
const BG_CARD: u32 = 0xffffff;
const BG_ELEVATED: u32 = 0xf8f9fa;
const BORDER_SUBTLE: u32 = 0xe8ecf0;
const BORDER: u32 = 0xd4dae0;
const PRIMARY: u32 = 0x0d6efd;
const PRIMARY_HOVER: u32 = 0x0a58ca;
const PRIMARY_MUTED: u32 = 0xe7f1ff;
const BG_HOVER: u32 = 0xdfeaf8;
const BG_ROW_HOVER: u32 = 0xf4f7fb;
const BORDER_HOVER: u32 = 0x9db1c7;
const ACCENT: u32 = 0xf59e0b;
const SUCCESS: u32 = 0x10b981;
const DANGER: u32 = 0xef4444;
const TEXT: u32 = 0x1a1f2e;
const TEXT_2: u32 = 0x4b5563;
const TEXT_3: u32 = 0x9ca3af;
const STOPPED: u32 = 0x64748b;
const INFO: u32 = 0x60a5fa;
const WARNING_TEXT: u32 = 0xb45309;
const ERROR_BG: u32 = 0xfef2f2;
const ERROR_BORDER: u32 = 0xfecaca;
const SUCCESS_BG: u32 = 0xf0fdf4;
const SUCCESS_BORDER: u32 = 0xbbf7d0;
const PRIMARY_TINT_08: u32 = 0x3b9eff14;
const PRIMARY_TINT_25: u32 = 0x3b9eff40;
const DANGER_TINT_08: u32 = 0xf8717114;
const DANGER_TINT_15: u32 = 0xf8717126;
const DANGER_TINT_20: u32 = 0xf8717133;
const DANGER_TINT_25: u32 = 0xf8717140;
const SUCCESS_TINT_08: u32 = 0x10b98114;
const SUCCESS_TINT_24: u32 = 0x10b9813d;
const ERROR_BORDER_SOFT: u32 = 0xfca5a5;
const LOG_BG: u32 = 0x0a0e14;
const LOG_TEXT: u32 = 0x9ca9ba;
const TEXT_ON_PRIMARY: u32 = 0xffffff;
const OVERLAY: u32 = 0x0a0e1473;

const TITLEBAR_HEIGHT: f32 = 56.;
const FONT: &str = "Microsoft YaHei UI";
const MONO: &str = "Consolas";

/// 卡片徽标与头像的备选色，按名称稳定取一个。
const PALETTE: [u32; 8] = [
    0x0d6efd, 0x10b981, 0xf59e0b, 0x8b5cf6, 0xef4444, 0x06b6d4, 0xec4899, 0x64748b,
];

fn palette_color(seed: &str) -> u32 {
    let hue = seed
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hue % PALETTE.len() as u32) as usize]
}

/// `#rrggbb` -> `0xrrggbb`, falling back to the primary color.
fn hex_color(color: &str) -> u32 {
    color
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .unwrap_or(PRIMARY)
}

/// Plain text tooltip shown while hovering an element.
struct TextTooltip(SharedString);

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(420.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(rgb(BG_CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .text_size(px(12.))
            .text_color(rgb(TEXT_2))
            .child(self.0.clone())
    }
}

fn is_running(status: &InstanceStatus) -> bool {
    matches!(
        status,
        InstanceStatus::Running | InstanceStatus::AdoptedRunning
    )
}

fn is_busy(status: &InstanceStatus) -> bool {
    matches!(status, InstanceStatus::Starting | InstanceStatus::Stopping)
}

fn status_label(status: &InstanceStatus) -> &'static str {
    match status {
        InstanceStatus::Running => "运行中",
        InstanceStatus::AdoptedRunning => "运行中（已接管）",
        InstanceStatus::Starting => "启动中…",
        InstanceStatus::Stopping => "停止中…",
        InstanceStatus::Crashed => "启动失败",
        InstanceStatus::Stopped => "已停止",
    }
}

fn status_color(status: &InstanceStatus) -> u32 {
    match status {
        InstanceStatus::Running | InstanceStatus::AdoptedRunning => SUCCESS,
        InstanceStatus::Starting | InstanceStatus::Stopping => ACCENT,
        InstanceStatus::Crashed => DANGER,
        InstanceStatus::Stopped => STOPPED,
    }
}

fn initial(name: &str) -> String {
    name.chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string()
}

fn runtime_label(binding: &str) -> String {
    if binding == LATEST_STABLE {
        "最新稳定版".to_owned()
    } else if binding == EXTERNAL_RUNTIME_ID {
        "原版 DSH".to_owned()
    } else {
        format!("DSH {binding}")
    }
}

fn fmt_rfc3339(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

fn fmt_utc(value: &chrono::DateTime<chrono::Utc>) -> String {
    value
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// A control that only shows hover feedback while it can be used.
///
/// The element is built without its hover style; [`clickable`] asks for that
/// style only when the control is enabled, so a disabled button stays flat
/// instead of suggesting a click.
struct Hoverable {
    el: Div,
    hover: Box<dyn FnOnce(StyleRefinement) -> StyleRefinement>,
}

impl Hoverable {
    fn new(el: Div, hover: impl FnOnce(StyleRefinement) -> StyleRefinement + 'static) -> Self {
        Self {
            el,
            hover: Box::new(hover),
        }
    }
}

/// How [`clickable`] presents a control in each state.
trait Clickable {
    /// Usable: pointer cursor and hover feedback.
    fn enabled(self) -> Div;
    /// Not usable: dimmed and flat, so it does not invite a click.
    fn disabled(self) -> Div;
}

impl Clickable for Div {
    fn enabled(self) -> Div {
        self.cursor_pointer()
    }

    fn disabled(self) -> Div {
        self.opacity(0.4)
    }
}

impl Clickable for Hoverable {
    fn enabled(self) -> Div {
        self.el.hover(self.hover).cursor_pointer()
    }

    fn disabled(self) -> Div {
        self.el.opacity(0.4)
    }
}

/// Attach a left-click handler only when `enabled`; disabled controls are
/// dimmed and show no hover feedback.
fn clickable(
    el: impl Clickable,
    enabled: bool,
    handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    if enabled {
        el.enabled().on_mouse_down(MouseButton::Left, handler)
    } else {
        el.disabled()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Default,
    Primary,
    Danger,
}

fn btn(label: impl Into<SharedString>, tone: Tone, small: bool) -> Hoverable {
    let el = div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .px(px(if small { 14. } else { 18. }))
        .py(px(if small { 7. } else { 9. }))
        .rounded(px(9.))
        .border_1()
        .text_size(px(if small { 12. } else { 13. }))
        .font_weight(FontWeight::SEMIBOLD)
        .child(tr(&label.into()));
    match tone {
        Tone::Default => Hoverable::new(
            el.bg(rgb(BG_CARD))
                .border_color(rgb(BORDER))
                .text_color(rgb(TEXT)),
            |s| s.bg(rgb(BG_HOVER)).border_color(rgb(BORDER_HOVER)),
        ),
        Tone::Primary => Hoverable::new(
            el.bg(rgb(PRIMARY))
                .border_color(rgb(PRIMARY))
                .text_color(white())
                .font_weight(FontWeight::BOLD),
            |s| s.bg(rgb(PRIMARY_HOVER)).border_color(rgb(PRIMARY_HOVER)),
        ),
        Tone::Danger => Hoverable::new(
            el.bg(rgb(BG_CARD))
                .border_color(rgba(0xf871714d))
                .text_color(rgb(DANGER)),
            |s| s.bg(rgba(0xef444414)).border_color(rgb(DANGER)),
        ),
    }
}

/// One caption button of our own title bar.
///
/// The platform turns the area into a native window control (minimise,
/// maximise, close), so clicks behave like the system ones while the hover
/// feedback is painted here and appears the moment the pointer arrives.
fn caption_button(area: WindowControlArea, danger: bool, icon: Div) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(46.))
        .h_full()
        .text_color(rgb(TEXT_2))
        .window_control_area(area)
        .hover(move |s| {
            if danger {
                s.bg(rgb(0xe81123)).text_color(rgb(BG_CARD))
            } else {
                s.bg(rgb(BG_HOVER)).text_color(rgb(TEXT))
            }
        })
        .child(icon)
}

/// Toggle switch; the caller attaches the click handler.
fn switch(on: bool) -> Div {
    div()
        .relative()
        .flex_shrink_0()
        .w(px(42.))
        .h(px(24.))
        .rounded(px(13.))
        .border_1()
        .border_color(rgb(if on { PRIMARY } else { BORDER }))
        .bg(rgb(if on { PRIMARY } else { BG_ELEVATED }))
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(if on { 20. } else { 2. }))
                .size(px(18.))
                .rounded_full()
                .bg(if on { rgb(BG_CARD) } else { rgb(TEXT_3) }),
        )
}

#[derive(Clone, Copy)]
enum ChipTone {
    Warn,
    Bad,
    Off,
    Good,
    Info,
}

fn chip(label: impl Into<SharedString>, tone: ChipTone) -> Div {
    let (bg, fg, border) = match tone {
        ChipTone::Warn => (0xffc24726, ACCENT, 0xffc2474d),
        ChipTone::Bad => (0xf8717126, DANGER, 0xf871714d),
        ChipTone::Off => (0x64748b26, 0x94a3b8, 0x64748b4d),
        ChipTone::Good => (0x4ade8026, SUCCESS, 0x4ade804d),
        ChipTone::Info => (0xe7f1ffff, PRIMARY, 0x3b9eff4d),
    };
    div()
        .flex_shrink_0()
        .px(px(10.))
        .py(px(3.))
        .rounded(px(12.))
        .border_1()
        .border_color(rgba(border))
        .bg(rgba(bg))
        .text_color(rgb(fg))
        .text_size(px(10.5))
        .font_weight(FontWeight::BOLD)
        .child(label.into())
}

fn status_pill(status: &InstanceStatus) -> Div {
    let (bg, fg, border) = match status {
        InstanceStatus::Running | InstanceStatus::AdoptedRunning => {
            (0x10b9811f, SUCCESS, 0x10b98140)
        }
        InstanceStatus::Starting | InstanceStatus::Stopping => (0xffc24726, ACCENT, 0xffc2474d),
        InstanceStatus::Crashed => (0xef44441f, DANGER, 0xef444440),
        InstanceStatus::Stopped => (0x64748b26, 0x94a3b8, 0x64748b4d),
    };
    div()
        .flex_shrink_0()
        .px(px(8.))
        .py(px(1.))
        .rounded(px(10.))
        .border_1()
        .border_color(rgba(border))
        .bg(rgba(bg))
        .text_color(rgb(fg))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(status_label(status))
}

/// The instance's icon, or the first letter of its name.
fn badge_text(instance: &InstanceConfig) -> String {
    if instance.icon.is_empty() {
        initial(&instance.name)
    } else {
        instance.icon.clone()
    }
}

/// Badge colors offered in the appearance dialog (design palette).
const BADGE_COLORS: [&str; 8] = [
    "#0d6efd", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899", "#14b8a6", "#64748b",
];

/// Colored square with the instance's icon or initial.
fn badge(text: &str, color: u32, size: f32) -> Div {
    let chars = text.chars().count().max(1) as f32;
    let font = (size * if chars > 1. { 0.36 } else { 0.46 }).min(size * 0.8 / chars);
    div()
        .relative()
        .flex()
        .flex_shrink_0()
        .size(px(size))
        .items_center()
        .justify_center()
        .rounded(px((size * 0.26).max(5.)))
        .bg(rgb(color))
        .text_color(white())
        .text_size(px(font))
        .font_weight(FontWeight::EXTRA_BOLD)
        .child(text.to_owned())
}

fn logo_image() -> Arc<Image> {
    static LOGO: std::sync::OnceLock<Arc<Image>> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../assets/logo.png").to_vec(),
        ))
    })
    .clone()
}

fn brand_mark(size: f32) -> Div {
    div()
        .flex_shrink_0()
        .size(px(size))
        .child(img(logo_image()).size_full())
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .bg(rgb(BG_CARD))
        .border_1()
        .border_color(rgb(BORDER_SUBTLE))
        .rounded(px(12.))
        .overflow_hidden()
}

fn card_head(title: &str, right: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .px(px(20.))
        .py(px(15.))
        .border_b_1()
        .border_color(rgb(BORDER_SUBTLE))
        .child(
            div()
                .font_weight(FontWeight::BOLD)
                .text_size(px(14.))
                .child(tr(title)),
        )
        .child(right)
}

fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(TEXT_3))
        .child(text.into())
}

fn row_item(first: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.))
        .px(px(20.))
        .py(px(13.))
        .when(!first, |el| {
            el.border_t_1().border_color(rgb(BORDER_SUBTLE))
        })
}

fn form_row(first: bool, title: &str, desc: &str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(20.))
        .px(px(20.))
        .py(px(16.))
        .when(!first, |el| {
            el.border_t_1().border_color(rgb(BORDER_SUBTLE))
        })
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(3.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(tr(title)))
                .child(hint(tr(desc))),
        )
        .child(control)
}

fn path_box(path: &Path) -> Div {
    div()
        .max_w(px(420.))
        .px(px(11.))
        .py(px(6.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(BORDER_SUBTLE))
        .bg(rgb(BG_ELEVATED))
        .font_family(MONO)
        .text_size(px(12.))
        .text_color(rgb(TEXT_3))
        .truncate()
        .child(path.display().to_string())
}

fn note(text: impl Into<SharedString>) -> Div {
    div()
        .px(px(18.))
        .py(px(14.))
        .rounded(px(12.))
        .border_1()
        .border_color(rgba(0x3b9eff40))
        .bg(rgba(0x3b9eff14))
        .text_color(rgb(TEXT_2))
        .child(text.into())
}

fn segment(label: &str, active: bool) -> Div {
    div()
        .px(px(14.))
        .py(px(6.))
        .rounded(px(7.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .when(active, |el| el.bg(rgb(BG_CARD)).text_color(rgb(PRIMARY)))
        .when(!active, |el| el.text_color(rgb(TEXT_2)))
        .child(label.to_owned())
}

fn segmented() -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(9.))
        .border_1()
        .border_color(rgb(BORDER_SUBTLE))
        .bg(rgb(BG_ELEVATED))
}

fn stepper_button(label: &str) -> Hoverable {
    Hoverable::new(
        div()
            .flex()
            .size(px(30.))
            .items_center()
            .justify_center()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_CARD))
            .text_color(rgb(TEXT_2))
            .font_weight(FontWeight::BOLD)
            .child(label.to_owned()),
        |s| s.bg(rgb(BG_HOVER)).border_color(rgb(BORDER_HOVER)),
    )
}

/// A panel under the titlebar's right side; clicking outside it closes it.
/// The titlebar is left out of the click-away area, so the button that opened
/// the panel toggles it instead of closing and reopening it in one click.
fn popover(content: Div, close: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            div()
                .absolute()
                .top(px(TITLEBAR_HEIGHT))
                .left_0()
                .right_0()
                .bottom_0()
                .on_mouse_down(MouseButton::Left, close),
        )
        .child(
            div()
                .absolute()
                .top(px(52.))
                .right(px(24.))
                .w(px(360.))
                .rounded(px(12.))
                .border_1()
                .border_color(rgb(BORDER_SUBTLE))
                .bg(rgb(BG_CARD))
                .shadow_lg()
                .overflow_hidden()
                .occlude()
                .child(content),
        )
}

/// A circular progress ring of `size` px; `ratio` in 0..=1.
fn progress_ring(size: f32, ratio: f32) -> impl IntoElement {
    let ratio = ratio.clamp(0., 1.);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let stroke = 2.5;
            let center = bounds.center();
            let radius = bounds.size.width.min(bounds.size.height).as_f32() / 2. - stroke;
            let at = |turn: f32| {
                let angle = turn * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                point(
                    center.x + px(radius * angle.cos()),
                    center.y + px(radius * angle.sin()),
                )
            };
            let mut track = PathBuilder::stroke(px(stroke));
            track.move_to(at(0.));
            track.arc_to(point(px(radius), px(radius)), px(0.), true, true, at(0.5));
            track.arc_to(
                point(px(radius), px(radius)),
                px(0.),
                true,
                true,
                at(0.9999),
            );
            if let Ok(path) = track.build() {
                window.paint_path(path, rgb(BORDER_SUBTLE));
            }
            if ratio > 0.005 {
                let mut arc = PathBuilder::stroke(px(stroke));
                arc.move_to(at(0.));
                let end = ratio.min(0.9999);
                arc.arc_to(
                    point(px(radius), px(radius)),
                    px(0.),
                    end > 0.5,
                    true,
                    at(end),
                );
                if let Ok(path) = arc.build() {
                    window.paint_path(path, rgb(PRIMARY));
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size(px(size))
}

/// Everything the details pane reads from disk for one render.
struct DetailsData {
    paths: Option<InstancePaths>,
    plugins: Vec<PluginInfo>,
    snapshots: Vec<SnapshotInfo>,
    /// Oldest first.
    history: Vec<LaunchRecord>,
    /// Latest launch record while the crash rescue bar should show.
    crash: Option<Option<LaunchRecord>>,
}

impl Render for MarinaWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.track_notices(cx);
        let dialog = self.render_dialog(cx);
        let toasts = self.render_toasts(cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .font_family(FONT)
            .text_size(px(13.))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                // 键盘关窗：对话框优先，其次是两个浮层；进行中的任务不打断。
                if let Some(dialog) = this.dialog.as_ref() {
                    let busy = match dialog {
                        Dialog::ShareToCommunity { .. } => true,
                        Dialog::SharePack { busy, .. } => *busy,
                        Dialog::NewInstance(inner) => inner.busy,
                        Dialog::GitHubLogin { connecting, .. } => *connecting,
                        _ => false,
                    };
                    if busy || this.copy_job.is_some() {
                        return;
                    }
                    this.dialog = None;
                } else if this.notices.panel_open {
                    this.notices.panel_open = false;
                } else if this.download.panel_open {
                    this.download.panel_open = false;
                } else {
                    return;
                }
                cx.notify();
            }))
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_main(cx)),
            )
            .when_some(self.render_download_panel(cx), |el, panel| el.child(panel))
            .when_some(self.render_notice_panel(cx), |el, panel| el.child(panel))
            .when_some(toasts, |el, toasts| el.child(toasts))
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

impl MarinaWindow {
    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let maximized = window.is_maximized();
        let in_settings = self.active_view == View::GlobalSettings;
        let settings_btn = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(15.))
            .py(px(7.))
            .rounded(px(8.))
            .border_1()
            .font_weight(FontWeight::MEDIUM)
            .when(in_settings, |el| {
                el.bg(rgb(PRIMARY_MUTED))
                    .border_color(rgba(0x0d6efd4d))
                    .text_color(rgb(PRIMARY))
            })
            .when(!in_settings, |el| {
                el.border_color(rgb(BORDER))
                    .text_color(rgb(TEXT_2))
                    .hover(|s| {
                        s.bg(rgb(BG_HOVER))
                            .border_color(rgb(BORDER_HOVER))
                            .text_color(rgb(TEXT))
                    })
            })
            .child(
                svg()
                    .path("icons/settings.svg")
                    .size(px(15.))
                    .text_color(rgb(if in_settings { PRIMARY } else { TEXT_2 })),
            )
            .child("设置");
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(8.))
            .h(px(TITLEBAR_HEIGHT))
            .pl(px(24.))
            .bg(rgb(BG_CARD))
            .border_b_1()
            .border_color(rgb(BORDER_SUBTLE))
            .child(
                div()
                    .flex()
                    .h_full()
                    .items_center()
                    .gap(px(14.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(brand_mark(30.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(FontWeight::BOLD)
                            .child("DSH Marina"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT_3))
                            .child("MULTI-INSTANCE"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            .when_some(self.render_download_button(cx), |el, button| {
                el.child(button)
            })
            .child(self.render_bell_button(cx))
            .child(clickable(
                settings_btn,
                true,
                cx.listener(|this, _, _, cx| {
                    this.active_view = if this.active_view == View::GlobalSettings {
                        if this.selected_instance.is_some() {
                            View::InstanceDetails
                        } else {
                            View::Home
                        }
                    } else {
                        View::GlobalSettings
                    };
                    this.download.panel_open = false;
                    this.notices.panel_open = false;
                    cx.notify();
                }),
            ))
            .child(div().w(px(16.)).flex_shrink_0())
            // Caption buttons sit flush against each other and the window edge,
            // the way the system ones do, so no white seam shows between them.
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .h_full()
                    .child(caption_button(
                        WindowControlArea::Min,
                        false,
                        div().w(px(10.)).h(px(1.)).bg(rgb(TEXT_2)),
                    ))
                    .child(caption_button(
                        WindowControlArea::Max,
                        false,
                        if maximized {
                            div()
                                .relative()
                                .size(px(11.))
                                .child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .right_0()
                                        .size(px(8.))
                                        .border_1()
                                        .border_color(rgb(TEXT_2)),
                                )
                                .child(
                                    div()
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .size(px(8.))
                                        .border_1()
                                        .border_color(rgb(TEXT_2)),
                                )
                        } else {
                            div().size(px(10.)).border_1().border_color(rgb(TEXT_2))
                        },
                    ))
                    .child(caption_button(
                        WindowControlArea::Close,
                        true,
                        div().text_size(px(14.)).child("✕"),
                    )),
            )
    }

    /// Overall `(done, total)` of the download list.
    fn download_totals(&self) -> (u64, u64) {
        self.download
            .jobs
            .iter()
            .map(|job| job.amounts())
            .fold((0, 0), |(d, t), (jd, jt)| {
                (d + jd.min(jt.max(jd)), t + jt.max(jd))
            })
    }

    /// Download indicator: only while something downloads, with a ring
    /// showing the progress of the whole list.
    fn render_download_button(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.download.jobs.is_empty() {
            return None;
        }
        let (done, total) = self.download_totals();
        let ratio = if total > 0 {
            done as f32 / total as f32
        } else {
            0.
        };
        let count = self.download.jobs.len();
        let open = self.download.panel_open;
        Some(clickable(
            div()
                .relative()
                .flex()
                .flex_shrink_0()
                .size(px(34.))
                .items_center()
                .justify_center()
                .rounded_full()
                .when(open, |el| el.bg(rgb(PRIMARY_MUTED)))
                .when(!open, |el| el.hover(|s| s.bg(rgb(BG_HOVER))))
                .child(progress_ring(34., ratio))
                .child(
                    svg()
                        .path("icons/download.svg")
                        .size(px(15.))
                        .text_color(rgb(PRIMARY)),
                )
                .when(count > 1, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(-3.))
                            .right(px(-3.))
                            .flex()
                            .size(px(15.))
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(PRIMARY))
                            .text_color(white())
                            .text_size(px(9.))
                            .font_weight(FontWeight::BOLD)
                            .child(count.to_string()),
                    )
                }),
            true,
            cx.listener(|this, _, _, cx| {
                this.download.panel_open = !this.download.panel_open;
                this.notices.panel_open = false;
                cx.notify();
            }),
        ))
    }

    fn render_download_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.download.panel_open || self.download.jobs.is_empty() {
            return None;
        }
        let rows = self.download.jobs.iter().enumerate().map(|(index, job)| {
            let (done, total) = job.amounts();
            let ratio = if total > 0 {
                (done as f32 / total as f32).clamp(0., 1.)
            } else {
                0.
            };
            let detail = match job.phase {
                JobPhase::Installing => "正在校验并安装…".to_owned(),
                JobPhase::Downloading => {
                    let mut text = if total > 0 {
                        format!(
                            "{} / {}",
                            storage::format_bytes(done),
                            storage::format_bytes(total)
                        )
                    } else {
                        storage::format_bytes(done)
                    };
                    if let Some(speed) = job.speed(done) {
                        text.push_str(&format!(" · {}/s", storage::format_bytes(speed as u64)));
                        if total > done {
                            let left = ((total - done) as f64 / speed).ceil() as u64;
                            text.push_str(&format!(" · 剩余 {}", storage::format_duration(left)));
                        }
                    }
                    text
                }
            };
            let key = job.key.clone();
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(16.))
                .py(px(12.))
                .when(index > 0, |el| {
                    el.border_t_1().border_color(rgb(BORDER_SUBTLE))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(job.title.clone()),
                        )
                        .child(hint(format!("{}%", (ratio * 100.).round() as u32)))
                        .child(clickable(
                            Hoverable::new(
                                div()
                                    .px(px(6.))
                                    .rounded(px(5.))
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT_3))
                                    .child("取消"),
                                |s| s.bg(rgb(BG_HOVER)).text_color(rgb(DANGER)),
                            ),
                            job.phase == JobPhase::Downloading,
                            cx.listener(move |this, _, _, cx| this.cancel_job(&key, cx)),
                        )),
                )
                .child(
                    div()
                        .h(px(5.))
                        .w_full()
                        .rounded(px(3.))
                        .bg(rgb(BG_ELEVATED))
                        .child(
                            div()
                                .h_full()
                                .rounded(px(3.))
                                .bg(rgb(PRIMARY))
                                .w(relative(ratio)),
                        ),
                )
                .child(hint(detail).text_size(px(11.)))
        });
        let panel = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .py(px(11.))
                    .border_b_1()
                    .border_color(rgb(BORDER_SUBTLE))
                    .child(div().font_weight(FontWeight::BOLD).child("下载"))
                    .child(hint(format!("{} 项进行中", self.download.jobs.len()))),
            )
            .children(rows);
        Some(popover(
            panel,
            cx.listener(|this, _, _, cx| {
                this.download.panel_open = false;
                cx.notify();
            }),
        ))
    }
    fn render_bell_button(&self, cx: &mut Context<Self>) -> Div {
        let open = self.notices.panel_open;
        let unread = self.notices.unread;
        clickable(
            div()
                .relative()
                .flex()
                .flex_shrink_0()
                .size(px(34.))
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .when(open, |el| el.bg(rgb(PRIMARY_MUTED)))
                .when(!open, |el| el.hover(|s| s.bg(rgb(BG_HOVER))))
                .child(
                    svg()
                        .path("icons/bell.svg")
                        .size(px(17.))
                        .text_color(rgb(if open { PRIMARY } else { TEXT_2 })),
                )
                .when(unread > 0, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .right(px(1.))
                            .flex()
                            .min_w(px(15.))
                            .h(px(15.))
                            .px(px(3.))
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .border_2()
                            .border_color(rgb(BG_CARD))
                            .bg(rgb(DANGER))
                            .text_color(white())
                            .text_size(px(8.5))
                            .font_weight(FontWeight::BOLD)
                            .child(if unread > 99 {
                                "99+".to_owned()
                            } else {
                                unread.to_string()
                            }),
                    )
                }),
            true,
            cx.listener(|this, _, _, cx| {
                this.notices.panel_open = !this.notices.panel_open;
                this.download.panel_open = false;
                this.notices.unread = 0;
                cx.notify();
            }),
        )
    }

    fn render_notice_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.notices.panel_open {
            return None;
        }
        let today = chrono::Local::now().date_naive();
        let rows =
            self.notices
                .history
                .iter()
                .rev()
                .enumerate()
                .map(|(index, (error, text, at))| {
                    let time = if at.date_naive() == today {
                        at.format("%H:%M:%S").to_string()
                    } else {
                        at.format("%m-%d %H:%M").to_string()
                    };
                    div()
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .px(px(16.))
                        .py(px(10.))
                        .when(index > 0, |el| {
                            el.border_t_1().border_color(rgb(BORDER_SUBTLE))
                        })
                        .child(
                            div()
                                .flex_shrink_0()
                                .mt(px(5.))
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgb(if *error { DANGER } else { SUCCESS })),
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
                                        .text_size(px(12.5))
                                        .text_color(rgb(if *error { DANGER } else { TEXT }))
                                        .child(text.clone()),
                                )
                                .child(hint(time).text_size(px(10.5))),
                        )
                });
        let empty = self.notices.history.is_empty();
        let panel = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .py(px(11.))
                    .border_b_1()
                    .border_color(rgb(BORDER_SUBTLE))
                    .child(div().font_weight(FontWeight::BOLD).child("消息"))
                    .child(clickable(
                        Hoverable::new(
                            div()
                                .px(px(6.))
                                .rounded(px(5.))
                                .text_size(px(12.))
                                .text_color(rgb(TEXT_3))
                                .child("清空"),
                            |s| s.bg(rgb(BG_HOVER)).text_color(rgb(TEXT)),
                        ),
                        !empty,
                        cx.listener(|this, _, _, cx| {
                            this.notices.history.clear();
                            cx.notify();
                        }),
                    )),
            )
            .when(empty, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.))
                        .py(px(28.))
                        .child(
                            svg()
                                .path("icons/inbox.svg")
                                .size(px(26.))
                                .text_color(rgb(TEXT_3)),
                        )
                        .child(hint("还没有消息")),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_h(px(420.))
                    .children(rows)
                    .overflow_y_scrollbar(),
            );
        Some(popover(
            panel,
            cx.listener(|this, _, _, cx| {
                this.notices.panel_open = false;
                cx.notify();
            }),
        ))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let total = self.instances.len();
        let running = self
            .instances
            .iter()
            .filter(|i| is_running(&self.get_instance_status(&i.id)))
            .count();
        let ratio = if total == 0 {
            0.
        } else {
            running as f32 / total as f32
        };
        let total_memory: u64 = self
            .instances
            .iter()
            .filter(|i| is_running(&self.get_instance_status(&i.id)))
            .filter_map(|i| self.memory.get(&i.id))
            .sum();
        let data_root = self.state.repository.data_root().display().to_string();

        let items: Vec<Div> = self
            .instances
            .iter()
            .map(|instance| self.render_instance_item(instance, cx))
            .collect();

        // 首页 / 插件市场 / 整合包, then a divider and the instances.
        let nav = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(10.))
            .pt(px(12.))
            .children(
                [
                    (View::Home, "首页"),
                    (View::Market, "插件市场"),
                    (View::Packs, "整合包"),
                ]
                .into_iter()
                .map(|(view, label)| {
                    let active = self.active_view == view
                        || (view == View::Home
                            && self.active_view == View::InstanceDetails
                            && self.selected_instance.is_none());
                    clickable(
                        div()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .text_size(px(13.5))
                            .font_weight(if active {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .when(active, |el| {
                                el.bg(rgb(PRIMARY_MUTED)).text_color(rgb(PRIMARY))
                            })
                            .when(!active, |el| {
                                el.text_color(rgb(TEXT)).hover(|s| s.bg(rgb(BG_HOVER)))
                            })
                            .child(tr(label)),
                        true,
                        cx.listener(move |this, _, window, cx| this.show_view(view, window, cx)),
                    )
                }),
            )
            .child(
                div()
                    .mt(px(10.))
                    .mx(px(2.))
                    .h(px(1.))
                    .bg(rgb(BORDER_SUBTLE)),
            );

        let new_instance = clickable(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .mx(px(10.))
                .mt(px(12.))
                .py(px(8.))
                .rounded(px(8.))
                .border_1()
                .border_dashed()
                .border_color(rgba(0x0d6efd66))
                .bg(rgba(0x0d6efd0a))
                .text_color(rgb(PRIMARY))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(rgb(PRIMARY_MUTED)).border_color(rgb(PRIMARY)))
                .child(
                    svg()
                        .path("icons/plus.svg")
                        .size(px(14.))
                        .text_color(rgb(PRIMARY)),
                )
                .child(tr("新建实例")),
            true,
            cx.listener(|this, _, window, cx| this.open_new_dialog(window, cx)),
        );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(248.))
            .h_full()
            .bg(rgb(BG_CARD))
            .border_r_1()
            .border_color(rgb(BORDER_SUBTLE))
            .child(nav)
            .child(new_instance)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(22.))
                    .pt(px(12.))
                    .pb(px(6.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_3))
                            .child(format!("实例 · {total}")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(1.))
                    .px(px(10.))
                    .pt(px(2.))
                    .pb(px(10.))
                    .when(items.is_empty(), |el| {
                        el.child(
                            div()
                                .px(px(6.))
                                .py(px(12.))
                                .text_size(px(12.))
                                .text_color(rgb(TEXT_3))
                                .child("还没有实例，点上面的「新建实例」开始"),
                        )
                    })
                    .children(items)
                    .overflow_y_scrollbar(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(18.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(rgb(BORDER_SUBTLE))
                    .text_size(px(12.))
                    .text_color(rgb(TEXT_2))
                    .child(
                        div().flex().gap(px(4.)).child("内存占用").child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(TEXT))
                                .child(if running == 0 {
                                    "—".to_owned()
                                } else {
                                    storage::format_bytes(total_memory)
                                }),
                        ),
                    )
                    .child(
                        div()
                            .h(px(4.))
                            .w_full()
                            .rounded(px(2.))
                            .bg(rgb(BG_ELEVATED))
                            .overflow_hidden()
                            .child(div().h_full().w(relative(ratio)).bg(rgb(PRIMARY))),
                    )
                    .child(
                        div()
                            .id("sidebar-data-root")
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_3))
                            .truncate()
                            .child(format!("数据目录 {data_root}"))
                            .tooltip({
                                let full = data_root.clone();
                                move |_, cx| {
                                    cx.new(|_| TextTooltip(SharedString::from(full.clone())))
                                        .into()
                                }
                            })
                            .tooltip_show_delay(Duration::ZERO),
                    ),
            )
    }

    fn render_instance_item(&self, instance: &InstanceConfig, cx: &mut Context<Self>) -> Div {
        let id = instance.id.clone();
        let active = self.active_view == View::InstanceDetails
            && self.selected_instance.as_deref() == Some(id.as_str());
        let status = self.get_instance_status(&id);
        let crashed = status == InstanceStatus::Crashed;
        let meta = format!(
            "{} · {}",
            status_label(&status),
            runtime_label(&instance.runtime.dsh_version)
        );

        let item = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .py(px(7.))
            .rounded(px(8.))
            .when(active, |el| el.bg(rgb(PRIMARY_MUTED)))
            .when(!active, |el| el.hover(|s| s.bg(rgb(BG_HOVER))))
            .child(badge(
                &badge_text(instance),
                hex_color(&instance.color),
                30.,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(if active {
                                        FontWeight::BOLD
                                    } else {
                                        FontWeight::SEMIBOLD
                                    })
                                    .text_color(rgb(if active { PRIMARY } else { TEXT }))
                                    .child(instance.name.clone()),
                            )
                            .when(
                                matches!(instance.origin.kind, OriginKind::ImportOriginal),
                                |el| {
                                    el.child(
                                        div()
                                            .flex_shrink_0()
                                            .px(px(5.))
                                            .rounded(px(4.))
                                            .bg(rgba(0xd9770622))
                                            .text_color(rgb(0xd97706))
                                            .text_size(px(9.5))
                                            .font_weight(FontWeight::BOLD)
                                            .child("原版"),
                                    )
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(rgb(status_color(&status))),
                            )
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(rgb(if crashed { DANGER } else { TEXT_3 }))
                                    .child(meta),
                            ),
                    ),
            );
        clickable(
            item,
            true,
            cx.listener(move |this, _, _, cx| this.select_instance(id.clone(), cx)),
        )
    }

    fn origin_subtitle(&self, instance: &InstanceConfig) -> String {
        match instance.origin.kind {
            OriginKind::Blank => "空白创建".to_owned(),
            OriginKind::ImportOriginal => "从原版 DSH 导入".to_owned(),
            OriginKind::Clone => {
                let source = instance.origin.from.as_deref().map(|from| {
                    self.instances
                        .iter()
                        .find(|i| i.id == from)
                        .map(|i| i.name.clone())
                        .unwrap_or_else(|| from.to_owned())
                });
                match source {
                    Some(name) => format!("复制自 {name}"),
                    None => "复制创建".to_owned(),
                }
            }
            OriginKind::Package => "从整合包导入".to_owned(),
        }
    }

    fn render_main(&self, cx: &mut Context<Self>) -> Div {
        let content = match self.active_view {
            View::GlobalSettings => self.render_global_settings(cx),
            View::Home => self.render_home(cx),
            View::Market => self.render_market(cx),
            View::Packs => self.render_packs(cx),
            View::InstanceDetails => match self
                .selected_instance
                .as_deref()
                .and_then(|id| self.instances.iter().find(|i| i.id == id))
            {
                Some(instance) => self.render_details(&instance.clone(), cx),
                None => self.render_home(cx),
            },
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .bg(rgb(BG))
            .child(content)
    }

    fn load_details(&self, instance: &InstanceConfig, status: &InstanceStatus) -> DetailsData {
        let paths = self.state.repository.paths(&instance.id).ok();
        let plugins = paths
            .as_ref()
            .and_then(|p| list_plugins(&p.home).ok())
            .unwrap_or_default();
        let snapshots = paths
            .as_ref()
            .and_then(|p| SnapshotStore::new(&p.root).list().ok())
            .unwrap_or_default();
        let history = paths
            .as_ref()
            .and_then(|p| LaunchHistoryStore::new(&p.root).list().ok())
            .unwrap_or_default();
        let crash = (*status == InstanceStatus::Crashed).then(|| history.last().cloned());
        DetailsData {
            paths,
            plugins,
            snapshots,
            history,
            crash,
        }
    }

    fn render_details(&self, instance: &InstanceConfig, cx: &mut Context<Self>) -> Div {
        let status = self.get_instance_status(&instance.id);
        let data = self.load_details(instance, &status);

        let mut pane = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(20.))
            .px(px(32.))
            .pt(px(26.))
            .pb(px(32.));
        if matches!(
            self.active_tab,
            DetailsTab::Overview | DetailsTab::Plugins | DetailsTab::Snapshots
        ) && let Some(record) = &data.crash
        {
            pane = pane.child(self.render_rescue_bar(instance, record.as_ref(), &data, cx));
        }
        let pane = pane.child(match self.active_tab {
            DetailsTab::Overview => self.render_overview_tab(instance, &data),
            DetailsTab::Plugins => self.render_plugins_tab(instance, &status, &data, cx),
            DetailsTab::Snapshots => self.render_snapshots_tab(instance, &status, &data, cx),
            DetailsTab::Logs => self.render_logs_tab(&data),
            DetailsTab::Settings => self.render_settings_tab(instance, &status, &data, cx),
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_hero(instance, &status, &data, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(pane)
                    .overflow_y_scrollbar(),
            )
    }

    fn render_hero(
        &self,
        instance: &InstanceConfig,
        status: &InstanceStatus,
        data: &DetailsData,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = instance.id.clone();
        let running = is_running(status);
        let mut actions = div().flex().flex_shrink_0().gap(px(10.));
        if running || *status == InstanceStatus::Stopping {
            let stopping = *status == InstanceStatus::Stopping;
            {
                let id = id.clone();
                actions = actions.child(clickable(
                    btn("打开窗口", Tone::Primary, true),
                    !stopping,
                    cx.listener(move |this, _, _, cx| this.focus_instance(&id, cx)),
                ));
            }
            {
                let id = id.clone();
                actions = actions.child(clickable(
                    btn("重启", Tone::Default, true),
                    !stopping,
                    cx.listener(move |this, _, _, cx| this.request_stop(id.clone(), true, cx)),
                ));
            }
            let id = id.clone();
            actions = actions.child(clickable(
                btn(
                    if stopping { "停止中…" } else { "停止" },
                    Tone::Danger,
                    true,
                ),
                !stopping,
                cx.listener(move |this, _, _, cx| this.request_stop(id.clone(), false, cx)),
            ));
        } else {
            let starting = *status == InstanceStatus::Starting;
            {
                let id = id.clone();
                actions = actions.child(clickable(
                    btn(
                        if starting {
                            "启动中…"
                        } else {
                            "▶ 启动"
                        },
                        Tone::Primary,
                        true,
                    ),
                    !starting,
                    cx.listener(move |this, _, _, cx| this.start_instance(id.clone(), false, cx)),
                ));
            }
            if let Some(root) = data.paths.as_ref().map(|p| p.root.clone()) {
                actions = actions.child(clickable(
                    btn("打开目录", Tone::Default, true),
                    true,
                    move |_, _, _| platform::open_path(&root),
                ));
            }
            let id = id.clone();
            actions = actions.child(clickable(
                btn("复制", Tone::Default, true),
                !starting,
                cx.listener(move |this, _, window, cx| {
                    this.open_clone_dialog(id.clone(), window, cx)
                }),
            ));
        }

        let enabled = data.plugins.iter().filter(|p| p.enabled).count();
        let (last_day, last_time) = data
            .history
            .last()
            .map(|r| {
                let at = r.started_at.with_timezone(&chrono::Local);
                (
                    at.format("%m-%d").to_string(),
                    Some(at.format("%H:%M").to_string()),
                )
            })
            .unwrap_or_else(|| ("从未".to_owned(), None));
        let binding = &instance.runtime.dsh_version;
        let (runtime_value, runtime_sub) = if binding == LATEST_STABLE {
            ("最新稳定版".to_owned(), Some("自动跟随".to_owned()))
        } else if binding == EXTERNAL_RUNTIME_ID {
            ("原版 DSH".to_owned(), None)
        } else {
            (binding.clone(), Some("固定版本".to_owned()))
        };
        let memory = self
            .memory
            .get(&instance.id)
            .filter(|_| running)
            .map(|bytes| storage::format_bytes(*bytes))
            .unwrap_or_else(|| "—".to_owned());
        let disk = self
            .disk
            .get(&instance.id)
            .map(|bytes| storage::format_bytes(*bytes))
            .unwrap_or_else(|| "计算中…".to_owned());

        // 资源占用显示优化：整合内存和磁盘信息
        let resource_value = if running {
            format!("内存 {} · 磁盘 {}", memory, disk)
        } else {
            format!("磁盘 {}", disk)
        };
        let resource_sub = if running {
            Some("运行时内存 · 数据磁盘".to_owned())
        } else {
            Some("未运行 · 仅数据磁盘".to_owned())
        };

        let cells = [
            (
                "插件",
                format!("{enabled} / {}", data.plugins.len()),
                Some("已启用 / 全部".to_owned()),
            ),
            ("快照", data.snapshots.len().to_string(), None),
            ("DSH 版本", runtime_value, runtime_sub),
            ("资源占用", resource_value, resource_sub),
            ("最近启动", last_day, last_time),
        ];
        let stats = div()
            .flex()
            .mt(px(16.))
            .mb(px(14.))
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(BORDER_SUBTLE))
            .bg(rgb(BG_ELEVATED))
            .children(
                cells
                    .into_iter()
                    .enumerate()
                    .map(|(index, (key, value, sub))| stat(index == 0, key, value, sub)),
            );
        let tabs = [
            (DetailsTab::Overview, "概览", None),
            (DetailsTab::Plugins, "插件", Some(data.plugins.len())),
            (
                DetailsTab::Snapshots,
                "快照与回滚",
                Some(data.snapshots.len()),
            ),
            (DetailsTab::Logs, "日志", None),
            (DetailsTab::Settings, "设置", None),
        ];
        let tab_row =
            div()
                .flex()
                .gap(px(4.))
                .children(tabs.into_iter().map(|(tab, label, count)| {
                    let active = self.active_tab == tab;
                    let el = div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(14.))
                        .py(px(10.))
                        .border_b_2()
                        .border_color(if active {
                            rgb(PRIMARY).into()
                        } else {
                            transparent_black()
                        })
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if active { PRIMARY } else { TEXT_3 }))
                        .when(!active, |el| el.hover(|s| s.text_color(rgb(TEXT_2))))
                        .child(label)
                        .when_some(count, |el, count| {
                            el.child(
                                div()
                                    .px(px(9.))
                                    .py(px(1.))
                                    .rounded(px(11.))
                                    .border_1()
                                    .border_color(if active {
                                        rgba(0x3b9eff4d)
                                    } else {
                                        rgb(BORDER_SUBTLE)
                                    })
                                    .bg(rgb(if active { PRIMARY_MUTED } else { BG_ELEVATED }))
                                    .text_color(rgb(if active { PRIMARY } else { TEXT_3 }))
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(count.to_string()),
                            )
                        });
                    clickable(
                        el,
                        true,
                        cx.listener(move |this, _, _, cx| {
                            this.active_tab = tab;
                            cx.notify();
                        }),
                    )
                }));

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .px(px(28.))
            .pt(px(20.))
            .bg(rgb(BG_CARD))
            .border_b_1()
            .border_color(rgb(BORDER_SUBTLE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(badge(
                        &badge_text(instance),
                        hex_color(&instance.color),
                        46.,
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
                                    .gap(px(10.))
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_size(px(19.))
                                            .font_weight(FontWeight::EXTRA_BOLD)
                                            .child(instance.name.clone()),
                                    )
                                    .child(status_pill(status)),
                            )
                            .child(hint(self.origin_subtitle(instance)))
                            .when(!instance.notes.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(TEXT_2))
                                        .line_clamp(1)
                                        .child(instance.notes.clone()),
                                )
                            }),
                    )
                    .child(actions),
            )
            .child(stats)
            .child(tab_row)
    }
}

/// One cell of the instance stats strip.
fn stat(first: bool, key: &str, value: String, sub: Option<String>) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .gap(px(2.))
        .px(px(14.))
        .py(px(9.))
        .when(!first, |el| {
            el.border_l_1().border_color(rgb(BORDER_SUBTLE))
        })
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_3))
                .child(tr(key)),
        )
        .child(
            div()
                .truncate()
                .text_size(px(14.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TEXT))
                .child(value),
        )
        .child(
            div()
                .truncate()
                .text_size(px(10.5))
                .text_color(rgb(TEXT_3))
                .child(sub.unwrap_or_else(|| "\u{00a0}".to_owned())),
        )
}
