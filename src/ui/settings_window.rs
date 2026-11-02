//! The settings window: general preferences and the built-in Tailscale account.

use std::time::Duration;

use crate::ui::components::prelude::*;
use gpui_kit::{
    actions, px, size, AnyWindowHandle, App, Bounds, Entity, FocusHandle, Global, Subscription,
    Task, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions,
};

use super::localization::{text, theme_label};
use super::shell::ShellSettings;
use crate::platform::{ThemePreference, UiLanguage};
use crate::tailnet::{TailnetIdentity, TailnetState};

actions!(astesia_settings, [CloseSettingsWindow]);

const TAILSCALE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(super) fn bind_settings_window_keys(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("cmd-w", CloseSettingsWindow, Some("SettingsWindow")),
        gpui_kit::KeyBinding::new("ctrl-w", CloseSettingsWindow, Some("SettingsWindow")),
        gpui_kit::KeyBinding::new("escape", CloseSettingsWindow, Some("SettingsWindow")),
    ]);
}

/// The open settings window, so a second request focuses it instead of
/// opening another one.
#[derive(Default)]
struct OpenSettingsWindow(Option<AnyWindowHandle>);

impl Global for OpenSettingsWindow {}

/// Opens the settings window, or brings the open one to the front.
pub(super) fn open_settings_window(settings: &Entity<ShellSettings>, cx: &mut App) {
    if let Some(handle) = cx.default_global::<OpenSettingsWindow>().0 {
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            return;
        }
    }

    let language = settings.read(cx).language();
    let bounds = Bounds::centered(None, size(px(760.0), px(520.0)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(text(language, "设置", "Settings").into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(640.0), px(420.0))),
        ..Default::default()
    };
    let settings = settings.clone();
    let opened = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| SettingsWindow::new(&settings, window, cx));
        cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
    });
    match opened {
        Ok(handle) => cx.set_global(OpenSettingsWindow(Some(handle.into()))),
        Err(error) => log::error!("Could not open the settings window: {error}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsSection {
    General,
    Tailscale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TailscaleAction {
    SigningIn,
    SigningOut,
}

struct SettingsWindow {
    settings: WeakEntity<ShellSettings>,
    section: SettingsSection,
    focus_handle: FocusHandle,
    preference_error: Option<String>,
    tailscale_state: Option<TailnetState>,
    tailscale_action: Option<TailscaleAction>,
    tailscale_error: Option<String>,
    _tailscale_refresh: Task<()>,
    _workspace_release: Subscription,
}

impl SettingsWindow {
    fn new(settings: &Entity<ShellSettings>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Settings belong to the main window; close this one with it.
        let workspace_release =
            cx.observe_release_in(settings, window, |_, _, window, _| window.remove_window());
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let tailscale_refresh = cx.spawn(async move |this, cx| loop {
            let refreshed = this
                .update(cx, |this, cx| this.refresh_tailscale(cx))
                .is_ok();
            if !refreshed {
                break;
            }
            cx.background_executor()
                .timer(TAILSCALE_REFRESH_INTERVAL)
                .await;
        });
        Self {
            settings: settings.downgrade(),
            section: SettingsSection::General,
            focus_handle,
            preference_error: None,
            tailscale_state: None,
            tailscale_action: None,
            tailscale_error: None,
            _tailscale_refresh: tailscale_refresh,
            _workspace_release: workspace_release,
        }
    }

    fn language(&self, cx: &App) -> UiLanguage {
        self.settings
            .upgrade()
            .map(|settings| settings.read(cx).language())
            .unwrap_or_default()
    }

    fn theme(&self, cx: &App) -> ThemePreference {
        self.settings
            .upgrade()
            .map(|settings| settings.read(cx).theme())
            .unwrap_or_default()
    }

    fn close(&mut self, _: &CloseSettingsWindow, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            cx.notify();
        }
    }

    fn set_language(&mut self, language: UiLanguage, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.upgrade() else {
            return;
        };
        let result = settings.update(cx, |settings, cx| settings.set_language(language, cx));
        self.preference_error = result.err();
        window.set_window_title(text(language, "设置", "Settings"));
        cx.notify();
    }

    fn set_theme(&mut self, theme: ThemePreference, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.upgrade() else {
            return;
        };
        let result = settings.update(cx, |settings, cx| settings.set_theme(theme, cx));
        self.preference_error = result.err();
        cx.notify();
    }

    fn refresh_tailscale(&mut self, cx: &mut Context<Self>) {
        let load = super::runtime::spawn(cx, crate::tailnet::state());
        cx.spawn(async move |this, cx| {
            let state = load
                .await
                .unwrap_or_else(|error| TailnetState::Failed(error.to_string()));
            this.update(cx, |this, cx| {
                if this.tailscale_state.as_ref() != Some(&state) {
                    this.tailscale_state = Some(state);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn run_tailscale_action(&mut self, action: TailscaleAction, cx: &mut Context<Self>) {
        if self.tailscale_action.is_some() {
            return;
        }
        self.tailscale_action = Some(action);
        self.tailscale_error = None;
        cx.notify();
        let run = super::runtime::spawn(cx, async move {
            match action {
                TailscaleAction::SigningIn => crate::tailnet::sign_in().await,
                TailscaleAction::SigningOut => crate::tailnet::sign_out().await,
            }
        });
        cx.spawn(async move |this, cx| {
            let result = match run.await {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.tailscale_action = None;
                this.tailscale_error = result.err();
                this.refresh_tailscale(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_nav(&self, language: UiLanguage, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let item = |id: &'static str, label: &'static str, section: SettingsSection| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .style(ButtonStyle::Transparent)
                .toggle_state(self.section == section)
                .selected_style(ButtonStyle::Filled)
                .w_full()
                .justify_start()
                .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
        };
        v_flex()
            .w(px(180.0))
            .h_full()
            .flex_none()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(item(
                "settings-section-general",
                text(language, "通用", "General"),
                SettingsSection::General,
            ))
            .child(item(
                "settings-section-tailscale",
                "Tailscale",
                SettingsSection::Tailscale,
            ))
    }

    fn render_general(&self, language: UiLanguage, cx: &mut Context<Self>) -> AnyElement {
        let current_language = language;
        let current_theme = self.theme(cx);
        let language_button = |id: &'static str, label: &'static str, value: UiLanguage| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .toggle_state(current_language == value)
                .selected_style(ButtonStyle::Filled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_language(value, window, cx);
                }))
        };
        let theme_button = |id: &'static str, value: ThemePreference| {
            Button::new(id, theme_label(language, value))
                .size(ButtonSize::Compact)
                .toggle_state(current_theme == value)
                .selected_style(ButtonStyle::Filled)
                .on_click(cx.listener(move |this, _, _, cx| this.set_theme(value, cx)))
        };
        let language_buttons = h_flex()
            .gap_1()
            .child(language_button(
                "settings-language-zh",
                "简体中文",
                UiLanguage::Chinese,
            ))
            .child(language_button(
                "settings-language-en",
                "English",
                UiLanguage::English,
            ));
        let theme_buttons = h_flex()
            .gap_1()
            .child(theme_button(
                "settings-theme-system",
                ThemePreference::System,
            ))
            .child(theme_button("settings-theme-light", ThemePreference::Light))
            .child(theme_button("settings-theme-dark", ThemePreference::Dark));
        v_flex()
            .gap_4()
            .child(section_title(text(language, "通用", "General")))
            .child(setting_row(
                text(language, "语言", "Language"),
                None,
                language_buttons,
                cx,
            ))
            .child(setting_row(
                text(language, "主题", "Theme"),
                None,
                theme_buttons,
                cx,
            ))
            .when_some(self.preference_error.clone(), |section, error| {
                section.child(notice(NoticeTone::Warning, error, cx))
            })
            .into_any_element()
    }

    fn render_tailscale(&self, language: UiLanguage, cx: &mut Context<Self>) -> AnyElement {
        let intro = text(
            language,
            "内置 Tailscale 使用独立的账户，与系统中安装的 Tailscale 客户端互不影响。勾选了“通过内置 Tailscale 连接”的数据库连接会通过它访问。",
            "Built-in Tailscale signs in with its own account, separate from the Tailscale client installed on this computer. Connections with “Connect through built-in Tailscale” turned on go through it.",
        );
        let mut section = v_flex()
            .gap_4()
            .child(section_title("Tailscale"))
            .child(Label::new(intro).size(LabelSize::Small).color(Color::Muted));

        let state = self.tailscale_state.clone();
        if state == Some(TailnetState::Unavailable) {
            return section
                .child(notice(
                    NoticeTone::Info,
                    text(
                        language,
                        "此版本不包含内置 Tailscale。macOS 与 Linux 版本提供该功能。",
                        "This build does not include built-in Tailscale. It is available in the macOS and Linux builds.",
                    )
                    .to_string(),
                    cx,
                ))
                .into_any_element();
        }

        let (status, detail) = tailscale_status_text(language, state.as_ref());
        section = section.child(setting_row(
            text(language, "状态", "Status"),
            detail,
            Label::new(status).size(LabelSize::Small),
            cx,
        ));

        if let Some(TailnetState::Running(identity)) = &state {
            section = section.children(identity_rows(language, identity, cx));
        }

        let signed_in = matches!(state, Some(TailnetState::Running(_)));
        let busy = self.tailscale_action.is_some();
        let action_button = if signed_in {
            Button::new(
                "settings-tailscale-sign-out",
                text(language, "退出登录", "Sign Out"),
            )
            .size(ButtonSize::Compact)
            .loading(self.tailscale_action == Some(TailscaleAction::SigningOut))
            .disabled(busy)
            .on_click(cx.listener(|this, _, _, cx| {
                this.run_tailscale_action(TailscaleAction::SigningOut, cx);
            }))
        } else {
            Button::new(
                "settings-tailscale-sign-in",
                text(language, "登录", "Sign In"),
            )
            .size(ButtonSize::Compact)
            .style(ButtonStyle::Filled)
            .loading(self.tailscale_action == Some(TailscaleAction::SigningIn))
            .disabled(busy || state.is_none())
            .on_click(cx.listener(|this, _, _, cx| {
                this.run_tailscale_action(TailscaleAction::SigningIn, cx);
            }))
        };
        section = section.child(h_flex().gap_2().child(action_button));

        if self.tailscale_action == Some(TailscaleAction::SigningIn) {
            section = section.child(notice(
                NoticeTone::Info,
                text(
                    language,
                    "请在浏览器中完成登录，最多等待 5 分钟。",
                    "Finish signing in in your browser. Astesia waits up to 5 minutes.",
                )
                .to_string(),
                cx,
            ));
        }
        let status_error = match &state {
            Some(TailnetState::Failed(error)) => Some(error.clone()),
            _ => None,
        };
        if let Some(error) = self.tailscale_error.clone().or(status_error) {
            section = section.child(notice(NoticeTone::Error, error, cx));
        }
        section.into_any_element()
    }
}

fn tailscale_status_text(
    language: UiLanguage,
    state: Option<&TailnetState>,
) -> (&'static str, Option<&'static str>) {
    match state {
        None => (text(language, "正在读取…", "Checking…"), None),
        Some(TailnetState::Unavailable) => (text(language, "不可用", "Unavailable"), None),
        Some(TailnetState::NotStarted) => (
            text(language, "未启动", "Not started"),
            Some(text(
                language,
                "登录或首次使用 Tailscale 连接时启动。",
                "Starts when you sign in or first use a Tailscale connection.",
            )),
        ),
        Some(TailnetState::Starting) => (text(language, "正在连接…", "Connecting…"), None),
        Some(TailnetState::NeedsLogin) => (text(language, "未登录", "Signed out"), None),
        Some(TailnetState::NeedsApproval) => (
            text(language, "等待批准", "Waiting for approval"),
            Some(text(
                language,
                "请在 Tailscale 管理后台批准名为 astesia 的设备。",
                "Approve the device named astesia in the Tailscale admin console.",
            )),
        ),
        Some(TailnetState::Running(_)) => (text(language, "已连接", "Connected"), None),
        Some(TailnetState::Failed(_)) => (
            text(language, "出错", "Error"),
            Some(text(
                language,
                "无法读取状态，详情见下方。",
                "Could not read the status. See the details below.",
            )),
        ),
    }
}

fn identity_rows(
    language: UiLanguage,
    identity: &TailnetIdentity,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let rows = [
        (text(language, "账户", "Account"), identity.account.clone()),
        ("Tailnet", identity.tailnet.clone()),
        (
            text(language, "设备名", "Device name"),
            identity.dns_name.clone(),
        ),
        (
            text(language, "地址", "Addresses"),
            (!identity.addresses.is_empty()).then(|| identity.addresses.join(", ")),
        ),
    ];
    rows.into_iter()
        .filter_map(|(label, value)| {
            value.map(|value| {
                setting_row(
                    label,
                    None,
                    Label::new(value).size(LabelSize::Small).buffer_font(cx),
                    cx,
                )
                .into_any_element()
            })
        })
        .collect()
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .text_lg()
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(title)
}

fn setting_row(
    label: &'static str,
    detail: Option<&'static str>,
    control: impl IntoElement,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .items_start()
        .justify_between()
        .gap_4()
        .pb_3()
        .border_b_1()
        .border_color(colors.border)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small))
                .when_some(detail, |column, detail| {
                    column.child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
        .child(h_flex().flex_none().justify_end().child(control))
}

#[derive(Clone, Copy)]
enum NoticeTone {
    Info,
    Warning,
    Error,
}

fn notice(tone: NoticeTone, message: String, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let status = cx.theme().status();
    let (border, background) = match tone {
        NoticeTone::Info => (status.info_border, status.info_background),
        NoticeTone::Warning => (status.warning_border, status.warning_background),
        NoticeTone::Error => (status.error_border, status.error_background),
    };
    div()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(background)
        .child(Label::new(message).size(LabelSize::Small))
        .into_any_element()
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = self.language(cx);
        let colors = cx.theme().colors();
        let content = match self.section {
            SettingsSection::General => self.render_general(language, cx),
            SettingsSection::Tailscale => self.render_tailscale(language, cx),
        };
        h_flex()
            .key_context("SettingsWindow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .size_full()
            .bg(colors.background)
            .text_color(colors.text)
            .child(self.render_nav(language, cx))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .child(content),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_status_stays_short_and_points_to_the_details() {
        let (status, detail) = tailscale_status_text(
            UiLanguage::English,
            Some(&TailnetState::Failed("a very long error".to_string())),
        );
        assert_eq!(status, "Error");
        assert!(detail.expect("detail").contains("details below"));
    }

    #[test]
    fn approval_status_explains_the_next_step() {
        let (status, detail) =
            tailscale_status_text(UiLanguage::Chinese, Some(&TailnetState::NeedsApproval));
        assert_eq!(status, "等待批准");
        assert!(detail.expect("detail").contains("astesia"));
    }
}
