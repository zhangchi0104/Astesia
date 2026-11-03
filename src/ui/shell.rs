use crate::ui::components::prelude::*;
use std::time::Duration;

use gpui_kit::{Entity, WeakEntity};

use crate::platform::{DesktopPreferences, NativePreferencesStore, ThemePreference, UiLanguage};

const MAX_VISIBLE_NOTIFICATIONS: usize = 4;

pub(super) struct ShellSettings {
    preferences: DesktopPreferences,
    store: Option<NativePreferencesStore>,
}

impl ShellSettings {
    pub(super) fn new(
        preferences: DesktopPreferences,
        store: Option<NativePreferencesStore>,
    ) -> Self {
        Self { preferences, store }
    }

    pub(super) fn theme(&self) -> ThemePreference {
        self.preferences.theme
    }

    pub(super) fn language(&self) -> UiLanguage {
        self.preferences.language
    }

    pub(super) fn sidebar_visible(&self) -> bool {
        self.preferences.sidebar_visible
    }

    pub(super) fn set_theme(
        &mut self,
        theme: ThemePreference,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.preferences.theme = theme;
        apply_theme(theme, cx);
        self.persist_and_notify(cx)
    }

    pub(super) fn set_language(
        &mut self,
        language: UiLanguage,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.preferences.language = language;
        cx.set_global(UiLocale(language));
        cx.refresh_windows();
        self.persist_and_notify(cx)
    }

    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.preferences.sidebar_visible = !self.preferences.sidebar_visible;
        self.persist_and_notify(cx)
    }

    fn persist_and_notify(&self, cx: &mut Context<Self>) -> Result<(), String> {
        let result = self.persist();
        cx.notify();
        result
    }

    fn persist(&self) -> Result<(), String> {
        self.store
            .as_ref()
            .ok_or_else(|| "原生偏好设置不可用，本次更改不会在重启后保留".to_string())?
            .save(&self.preferences)
    }
}

pub(super) fn apply_theme(preference: ThemePreference, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};
    let mode = match preference {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System => {
            if matches!(
                cx.window_appearance(),
                gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
            ) {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            }
        }
    };
    Theme::change(mode, None, cx);
    super::theme::restore_selection_surfaces(cx);
    cx.refresh_windows();
}
pub(super) fn refresh_active_theme(preference: ThemePreference, cx: &mut App) {
    apply_theme(preference, cx);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotificationTone {
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationTone {
    /// How long a toast stays on screen before it dismisses itself. Problems
    /// stay a little longer so there is time to read them.
    fn auto_dismiss_after(self) -> Duration {
        match self {
            Self::Info | Self::Success => Duration::from_secs(4),
            Self::Warning => Duration::from_secs(6),
            Self::Error => Duration::from_secs(8),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Notification {
    id: u64,
    tone: NotificationTone,
    message: String,
}

/// The single place in-app notifications are shown: a stack of toasts in the
/// bottom-right corner of the workspace, just above the status bar. Every toast
/// dismisses itself after [`NotificationTone::auto_dismiss_after`]; hovering a
/// toast keeps it open until the pointer leaves.
pub(crate) struct NotificationCenter {
    next_id: u64,
    notifications: Vec<Notification>,
    hovered: Option<u64>,
}

/// Lets any view reach the workspace's [`NotificationCenter`] without having
/// the entity threaded through its constructor.
struct NotificationHub(WeakEntity<NotificationCenter>);
impl gpui_kit::Global for NotificationHub {}

/// Shows a toast in the workspace notification stack. Does nothing before the
/// workspace exists (for example in isolated view tests).
pub(crate) fn show_notification(tone: NotificationTone, message: impl Into<String>, cx: &mut App) {
    let Some(center) = cx
        .try_global::<NotificationHub>()
        .and_then(|hub| hub.0.upgrade())
    else {
        return;
    };
    let message = message.into();
    center.update(cx, |center, cx| center.push(tone, message, cx));
}

impl NotificationCenter {
    pub(super) fn new() -> Self {
        Self {
            next_id: 0,
            notifications: Vec::new(),
            hovered: None,
        }
    }

    /// Makes this center the target of [`show_notification`].
    pub(super) fn register(center: &Entity<Self>, cx: &mut App) {
        cx.set_global(NotificationHub(center.downgrade()));
    }

    pub(super) fn push(
        &mut self,
        tone: NotificationTone,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        // Repeating the same message (e.g. clicking a failing action twice)
        // refreshes the existing toast instead of stacking copies.
        self.notifications
            .retain(|notification| notification.tone != tone || notification.message != message);
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.notifications.push(Notification { id, tone, message });
        if self.notifications.len() > MAX_VISIBLE_NOTIFICATIONS {
            self.notifications.remove(0);
        }
        self.schedule_dismiss(id, tone.auto_dismiss_after(), cx);
        cx.notify();
    }

    fn schedule_dismiss(&self, id: u64, after: Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |center, cx| loop {
            cx.background_executor().timer(after).await;
            let Ok(hovered) = center.update(cx, |center, cx| {
                if center.hovered == Some(id) {
                    return true;
                }
                center.dismiss(id, cx);
                false
            }) else {
                return;
            };
            if !hovered {
                return;
            }
        })
        .detach();
    }

    fn set_hovered(&mut self, id: u64, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.hovered = Some(id);
        } else if self.hovered == Some(id) {
            self.hovered = None;
        }
        cx.notify();
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.id != id);
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        if self.notifications.len() != before {
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<(NotificationTone, String)> {
        self.notifications
            .iter()
            .map(|notification| (notification.tone, notification.message.clone()))
            .collect()
    }
}

/// Height of the workspace status bar; toasts sit just above it.
const STATUS_BAR_HEIGHT: f32 = 32.0;

impl Render for NotificationCenter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = cx.theme().status();
        // The tone backgrounds are translucent tints, so each toast is layered
        // on an opaque surface to stay readable over grids and editors.
        let surface = cx.theme().colors().elevated_surface_background;
        v_flex()
            .absolute()
            .bottom(px(STATUS_BAR_HEIGHT + 12.0))
            .right_3()
            .w(px(360.0))
            .gap_2()
            .children(self.notifications.iter().map(|notification| {
                let id = notification.id;
                let (foreground, background, border, icon) = match notification.tone {
                    NotificationTone::Info => (
                        status.info,
                        status.info_background,
                        status.info_border,
                        IconName::Info,
                    ),
                    NotificationTone::Success => (
                        status.success,
                        status.success_background,
                        status.success_border,
                        IconName::CircleCheck,
                    ),
                    NotificationTone::Warning => (
                        status.warning,
                        status.warning_background,
                        status.warning_border,
                        IconName::TriangleAlert,
                    ),
                    NotificationTone::Error => (
                        status.error,
                        status.error_background,
                        status.error_border,
                        IconName::CircleX,
                    ),
                };
                let toast = h_flex()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(background)
                    .child(
                        div().pt(px(2.0)).child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(Color::Custom(foreground)),
                        ),
                    )
                    .child(
                        Label::new(notification.message.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Custom(foreground))
                            .line_clamp(4)
                            .flex_1(),
                    )
                    .child(
                        IconButton::new(format!("dismiss-notification-{id}"), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(move |center, _, _, cx| {
                                center.dismiss(id, cx);
                            })),
                    );
                div()
                    .id(format!("notification-{id}"))
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(surface)
                    .shadow_md()
                    .on_hover(cx.listener(move |center, hovered: &bool, _, cx| {
                        center.set_hovered(id, *hovered, cx);
                    }))
                    .child(toast)
            }))
    }
}

pub(super) fn notify_preference_error(
    center: &Entity<NotificationCenter>,
    error: String,
    cx: &mut App,
) {
    center.update(cx, |center, cx| {
        center.push(NotificationTone::Error, error, cx);
    });
}

pub(super) struct UiLocale(pub UiLanguage);
impl gpui_kit::Global for UiLocale {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::TestAppContext;

    use super::*;

    #[gpui_kit::test]
    fn notifications_dismiss_themselves_after_their_timeout(cx: &mut TestAppContext) {
        let center = cx.new(|_| NotificationCenter::new());
        cx.update(|cx| NotificationCenter::register(&center, cx));
        cx.update(|cx| {
            show_notification(NotificationTone::Success, "saved", cx);
            show_notification(NotificationTone::Error, "failed", cx);
        });
        assert_eq!(center.read_with(cx, |center, _| center.messages().len()), 2);

        cx.executor().advance_clock(Duration::from_secs(5));
        assert_eq!(
            center.read_with(cx, |center, _| center.messages()),
            vec![(NotificationTone::Error, "failed".to_string())],
            "success toasts leave first; errors stay a little longer"
        );

        cx.executor().advance_clock(Duration::from_secs(4));
        assert!(center.read_with(cx, |center, _| center.messages().is_empty()));
    }

    #[gpui_kit::test]
    fn a_hovered_notification_waits_until_the_pointer_leaves(cx: &mut TestAppContext) {
        let center = cx.new(|_| NotificationCenter::new());
        center.update(cx, |center, cx| {
            center.push(NotificationTone::Info, "hello", cx);
            let id = center.next_id;
            center.set_hovered(id, true, cx);
        });
        cx.executor().advance_clock(Duration::from_secs(10));
        assert_eq!(center.read_with(cx, |center, _| center.messages().len()), 1);

        center.update(cx, |center, cx| {
            let id = center.next_id;
            center.set_hovered(id, false, cx);
        });
        cx.executor().advance_clock(Duration::from_secs(5));
        assert!(center.read_with(cx, |center, _| center.messages().is_empty()));
    }

    #[gpui_kit::test]
    fn repeating_a_message_refreshes_it_instead_of_stacking(cx: &mut TestAppContext) {
        let center = cx.new(|_| NotificationCenter::new());
        center.update(cx, |center, cx| {
            center.push(NotificationTone::Error, "boom", cx);
            center.push(NotificationTone::Error, "boom", cx);
        });
        assert_eq!(center.read_with(cx, |center, _| center.messages().len()), 1);
    }
}
