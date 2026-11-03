//! The settings window: general preferences and the built-in Tailscale account.

use std::time::Duration;

use crate::ui::components::prelude::*;
use gpui_kit::{
    actions, px, size, AnyWindowHandle, App, Bounds, Entity, FocusHandle, Global, Subscription,
    Task, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions,
};

use super::input_field::InputField;
use super::localization::{text, theme_label};
use super::shell::ShellSettings;
use crate::platform::{ThemePreference, UiLanguage};
use crate::tailnet::TailnetState;
use gpui_kit::component::input::InputEvent;
use gpui_kit::component::switch::Switch;

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
    /// The machine name the node uses on the tailnet.
    hostname: String,
    hostname_input: Entity<InputField>,
    connect_on_launch: bool,
    /// Polls the node's state while the Tailscale section is shown.
    tailscale_poll: Option<Task<()>>,
    _workspace_release: Subscription,
    _hostname_events: Subscription,
}

impl SettingsWindow {
    fn new(settings: &Entity<ShellSettings>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Settings belong to the main window; close this one with it.
        let workspace_release =
            cx.observe_release_in(settings, window, |_, _, window, _| window.remove_window());
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let tailnet_settings = crate::tailnet::settings();
        let hostname = tailnet_settings.effective_hostname();
        let default_hostname = crate::tailnet::default_hostname();
        let hostname_input = cx.new(|cx| {
            let field = InputField::new(window, cx, &default_hostname).code_value();
            field.set_text(hostname.clone(), window, cx);
            field
        });
        let hostname_events =
            cx.subscribe_in(&hostname_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_hostname(window, cx);
                }
            });
        Self {
            settings: settings.downgrade(),
            section: SettingsSection::General,
            focus_handle,
            preference_error: None,
            tailscale_state: None,
            tailscale_action: None,
            tailscale_error: None,
            hostname,
            hostname_input,
            connect_on_launch: tailnet_settings.connect_on_launch,
            tailscale_poll: None,
            _workspace_release: workspace_release,
            _hostname_events: hostname_events,
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
            self.refresh_tailscale(cx);
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

    /// Reads the node's state now and then every few seconds, while the
    /// Tailscale section is shown. Restarting drops the previous poll, so at
    /// most one status request is in flight.
    fn refresh_tailscale(&mut self, cx: &mut Context<Self>) {
        if self.section != SettingsSection::Tailscale {
            self.tailscale_poll = None;
            return;
        }
        self.tailscale_poll = Some(cx.spawn(async move |this, cx| loop {
            let Ok(load) = this.update(cx, |_, cx| {
                super::runtime::spawn_result(cx, async { Ok(crate::tailnet::state().await) })
            }) else {
                break;
            };
            let state = load.await.unwrap_or_else(TailnetState::Failed);
            let unavailable = state == TailnetState::Unavailable;
            let updated = this.update(cx, |this, cx| {
                if this.tailscale_state.as_ref() != Some(&state) {
                    this.tailscale_state = Some(state);
                    cx.notify();
                }
            });
            if updated.is_err() || unavailable {
                break;
            }
            cx.background_executor()
                .timer(TAILSCALE_REFRESH_INTERVAL)
                .await;
        }));
    }

    fn run_tailscale_action(&mut self, action: TailscaleAction, cx: &mut Context<Self>) {
        if self.tailscale_action.is_some() {
            return;
        }
        self.tailscale_action = Some(action);
        self.tailscale_error = None;
        cx.notify();
        let run = super::runtime::spawn_result(cx, async move {
            match action {
                TailscaleAction::SigningIn => crate::tailnet::sign_in().await,
                TailscaleAction::SigningOut => crate::tailnet::sign_out().await,
            }
        });
        cx.spawn(async move |this, cx| {
            let result = run.await;
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

    fn commit_hostname(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.hostname_input.read(cx).text(cx);
        if input.trim() == self.hostname {
            return;
        }
        let save =
            super::runtime::spawn_result(
                cx,
                async move { crate::tailnet::set_hostname(&input).await },
            );
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = save.await;
            cx.update_window(window_handle, |_, window, cx| {
                this.update(cx, |this, cx| {
                    match result {
                        Ok(hostname) => {
                            this.hostname = hostname;
                            this.tailscale_error = None;
                        }
                        Err(error) => this.tailscale_error = Some(error),
                    }
                    let hostname = this.hostname.clone();
                    this.hostname_input
                        .update(cx, |field, cx| field.set_text(hostname, window, cx));
                    this.refresh_tailscale(cx);
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    fn set_connect_on_launch(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.connect_on_launch = enabled;
        cx.notify();
        let save = super::runtime::spawn_result(cx, crate::tailnet::set_connect_on_launch(enabled));
        cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.tailscale_error = Some(error);
                    this.connect_on_launch = crate::tailnet::settings().connect_on_launch;
                }
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
            // The kit button centers its label inside a full-width row, so the
            // label goes in as a growing child to sit at the leading edge.
            Button::new(id, "")
                .aria_label(label)
                .size(ButtonSize::Compact)
                .style(ButtonStyle::Transparent)
                .toggle_state(self.section == section)
                .selected_style(ButtonStyle::Filled)
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                )
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
        let current_theme = self.theme(cx);
        let language_button = |id: &'static str, label: &'static str, value: UiLanguage| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .toggle_state(language == value)
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
            .child(
                card(cx)
                    .child(card_row(
                        text(language, "语言", "Language"),
                        None,
                        language_buttons,
                        cx,
                    ))
                    .child(card_row(
                        text(language, "主题", "Theme"),
                        None,
                        theme_buttons,
                        cx,
                    )),
            )
            .when_some(self.preference_error.clone(), |section, error| {
                section.child(notice(NoticeTone::Warning, error, cx))
            })
            .into_any_element()
    }

    fn render_tailscale(&self, language: UiLanguage, cx: &mut Context<Self>) -> AnyElement {
        let mut section = v_flex().gap_4().child(section_title("Tailscale"));

        let state = self.tailscale_state.as_ref();
        if state == Some(&TailnetState::Unavailable) {
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

        let identity = match state {
            Some(TailnetState::Running(identity)) => Some(identity),
            _ => None,
        };
        let mut card = card(cx).child(self.render_account_row(language, state, cx));

        let weak = cx.entity().downgrade();
        card = card.child(card_row(
            text(language, "启动时连接", "Connect on Launch"),
            Some(match language {
                UiLanguage::Chinese => {
                    format!(
                        "打开 Astesia 时让 {} 在 tailnet 上保持在线。",
                        self.hostname
                    )
                }
                UiLanguage::English => format!(
                    "Keep {} online on your tailnet while Astesia is open.",
                    self.hostname
                ),
            }),
            Switch::new("settings-tailscale-connect-on-launch")
                .checked(self.connect_on_launch)
                .accessibility_label(text(language, "启动时连接", "Connect on Launch"))
                .on_click(move |checked, _, cx| {
                    weak.update(cx, |this, cx| this.set_connect_on_launch(*checked, cx))
                        .ok();
                }),
            cx,
        ));

        if let Some(identity) = identity {
            if let Some(tailnet) = identity.tailnet.clone() {
                card = card.child(card_row("Tailnet", None, value_label(tailnet, cx), cx));
            }
            let address = identity
                .dns_name
                .iter()
                .cloned()
                .chain(identity.addresses.iter().take(1).cloned())
                .collect::<Vec<_>>();
            if !address.is_empty() {
                card = card.child(card_row(
                    text(language, "Tailscale 地址", "Tailscale Address"),
                    None,
                    v_flex()
                        .items_end()
                        .children(address.into_iter().map(|line| value_label(line, cx))),
                    cx,
                ));
            }
        }

        let tags = identity
            .filter(|identity| !identity.tags.is_empty())
            .map(|identity| identity.tags.join(", "))
            .unwrap_or_else(|| text(language, "无", "None").to_string());
        card = card.child(card_row(
            text(language, "标签", "Tags"),
            None,
            h_flex()
                .gap_1p5()
                .child(Label::new(tags).size(LabelSize::Small).color(Color::Muted))
                .child(help_badge(
                    "settings-tailscale-tags-help",
                    text(
                        language,
                        "标签由 tailnet 管理员在 Tailscale 管理后台为设备设置。带标签的设备按标签而不是账户授权访问。",
                        "A tailnet admin sets tags on the device in the Tailscale admin console. A tagged device gets access by its tags instead of by account.",
                    ),
                    cx,
                )),
            cx,
        ));

        card = card.child(card_row(
            text(language, "首选主机名", "Preferred Hostname"),
            Some(
                text(
                    language,
                    "用于生成 Tailscale 地址。按回车保存。",
                    "Used to generate the Tailscale address. Press Return to save.",
                )
                .to_string(),
            ),
            div().w(px(240.0)).child(self.hostname_input.clone()),
            cx,
        ));
        section = section.child(card);

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
        let status_error = match state {
            Some(TailnetState::Failed(error)) => Some(error.clone()),
            _ => None,
        };
        if let Some(error) = self.tailscale_error.clone().or(status_error) {
            section = section.child(notice(NoticeTone::Error, error, cx));
        }
        section
            .child(
                Label::new(text(
                    language,
                    "Astesia 作为一台独立的设备加入 tailnet，使用自己的账户，与系统中安装的 Tailscale 客户端互不影响。勾选了“通过内置 Tailscale 连接”的数据库连接会经由这台设备访问，MCP 服务也会复用这里的登录。",
                    "Astesia joins the tailnet as its own device with its own account, separate from the Tailscale client installed on this computer. Connections with “Connect through built-in Tailscale” turned on go through this device, including when the MCP server uses them.",
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_account_row(
        &self,
        language: UiLanguage,
        state: Option<&TailnetState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title, subtitle) = account_summary(language, state, &self.hostname);
        let signed_in = matches!(state, Some(TailnetState::Running(_)));
        let (id, label, action) = if signed_in {
            (
                "settings-tailscale-sign-out",
                text(language, "退出登录…", "Sign Out…"),
                TailscaleAction::SigningOut,
            )
        } else {
            (
                "settings-tailscale-sign-in",
                text(language, "登录…", "Sign In…"),
                TailscaleAction::SigningIn,
            )
        };
        let button = Button::new(id, label)
            .size(ButtonSize::Compact)
            .style(ButtonStyle::Filled)
            .loading(self.tailscale_action == Some(action))
            .disabled(self.tailscale_action.is_some() || state.is_none())
            .on_click(cx.listener(move |this, _, _, cx| this.run_tailscale_action(action, cx)));
        let colors = cx.theme().colors();
        h_flex()
            .w_full()
            .gap_3()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(colors.border)
            .child(tailscale_mark(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when_some(subtitle, |column, subtitle| {
                        column.child(
                            Label::new(subtitle)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(div().flex_none().child(button))
    }
}

/// Title and subtitle of the account row at the top of the Tailscale card.
fn account_summary(
    language: UiLanguage,
    state: Option<&TailnetState>,
    hostname: &str,
) -> (String, Option<String>) {
    match state {
        None => (
            "Tailscale".to_string(),
            Some(text(language, "正在读取状态…", "Checking status…").to_string()),
        ),
        Some(TailnetState::Unavailable) => (
            text(language, "Tailscale 不可用", "Tailscale unavailable").to_string(),
            None,
        ),
        Some(TailnetState::NotStarted) => (
            text(language, "Tailscale 未启动", "Tailscale is not running").to_string(),
            Some(
                text(
                    language,
                    "登录或打开“启动时连接”后启动。",
                    "Starts when you sign in or turn on Connect on Launch.",
                )
                .to_string(),
            ),
        ),
        Some(TailnetState::Starting) => (
            text(language, "正在连接 Tailscale…", "Connecting to Tailscale…").to_string(),
            None,
        ),
        Some(TailnetState::NeedsLogin) => (
            text(language, "未登录 Tailscale", "Not signed in to Tailscale").to_string(),
            Some(match language {
                UiLanguage::Chinese => format!("登录后，这台设备以 {hostname} 加入 tailnet。"),
                UiLanguage::English => {
                    format!("After you sign in, this device joins the tailnet as {hostname}.")
                }
            }),
        ),
        Some(TailnetState::NeedsApproval) => (
            text(language, "等待管理员批准", "Waiting for approval").to_string(),
            Some(match language {
                UiLanguage::Chinese => {
                    format!("请在 Tailscale 管理后台批准名为 {hostname} 的设备。")
                }
                UiLanguage::English => {
                    format!("Approve the device named {hostname} in the Tailscale admin console.")
                }
            }),
        ),
        Some(TailnetState::Running(identity)) => (
            text(language, "已登录 Tailscale", "Signed in to Tailscale").to_string(),
            identity.account.as_ref().map(|account| match language {
                UiLanguage::Chinese => format!("登录账户：{account}"),
                UiLanguage::English => format!("Signed in as {account}"),
            }),
        ),
        Some(TailnetState::Failed(_)) => (
            text(language, "Tailscale 出错", "Tailscale error").to_string(),
            Some(
                text(
                    language,
                    "无法读取状态，详情见下方。",
                    "Could not read the status. See the details below.",
                )
                .to_string(),
            ),
        ),
    }
}

fn card(cx: &mut Context<SettingsWindow>) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface_background)
        .overflow_hidden()
}

fn card_row(
    label: &'static str,
    detail: Option<String>,
    control: impl IntoElement,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .gap_4()
        .px_4()
        .py_2p5()
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

fn value_label(value: String, cx: &mut Context<SettingsWindow>) -> Label {
    Label::new(value)
        .size(LabelSize::Small)
        .color(Color::Muted)
        .buffer_font(cx)
}

/// A circled question mark that explains a setting on hover.
fn help_badge(
    id: &'static str,
    help: &'static str,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    div()
        .id(id)
        .size(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(colors.text_muted)
        .text_color(colors.text_muted)
        .text_xs()
        .child("?")
        .tooltip(Tooltip::text(help))
}

/// The Tailscale mark: a three-by-three dot grid in a round badge.
fn tailscale_mark(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let colors = cx.theme().colors();
    let strong = colors.text;
    let faint = colors.text.opacity(0.25);
    let dot = move |on: bool| {
        div()
            .size(px(5.0))
            .rounded_full()
            .bg(if on { strong } else { faint })
    };
    let row = move |pattern: [bool; 3]| h_flex().gap(px(3.0)).children(pattern.map(dot));
    div()
        .flex_none()
        .size(px(40.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(colors.border)
        .bg(colors.background)
        .child(
            v_flex()
                .gap(px(3.0))
                .child(row([false, false, false]))
                .child(row([true, true, true]))
                .child(row([false, true, false])),
        )
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .text_lg()
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(title)
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
    use crate::tailnet::TailnetIdentity;

    #[test]
    fn failed_status_points_to_the_details() {
        let (title, subtitle) = account_summary(
            UiLanguage::English,
            Some(&TailnetState::Failed("a very long error".to_string())),
            "box-astesia",
        );
        assert_eq!(title, "Tailscale error");
        assert!(subtitle.expect("subtitle").contains("details below"));
    }

    #[test]
    fn approval_and_sign_in_name_the_device() {
        let (title, subtitle) = account_summary(
            UiLanguage::Chinese,
            Some(&TailnetState::NeedsApproval),
            "box-astesia",
        );
        assert_eq!(title, "等待管理员批准");
        assert!(subtitle.expect("subtitle").contains("box-astesia"));
        let (_, subtitle) = account_summary(
            UiLanguage::English,
            Some(&TailnetState::NeedsLogin),
            "box-astesia",
        );
        assert!(subtitle.expect("subtitle").contains("box-astesia"));
    }

    #[test]
    fn signed_in_summary_names_the_account() {
        let identity = TailnetIdentity {
            account: Some("alex@example.com".into()),
            ..Default::default()
        };
        let (title, subtitle) = account_summary(
            UiLanguage::English,
            Some(&TailnetState::Running(identity)),
            "box-astesia",
        );
        assert_eq!(title, "Signed in to Tailscale");
        assert_eq!(subtitle.as_deref(), Some("Signed in as alex@example.com"));
    }
}
