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
    bottom_offset: Pixels,
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
            bottom_offset: px(STATUS_BAR_HEIGHT + 12.0),
        }
    }

    /// Distance between the stack and the bottom edge of its container. The
    /// default clears the workspace status bar.
    pub(super) fn bottom_offset(mut self, offset: Pixels) -> Self {
        self.bottom_offset = offset;
        self
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::ThemeStyled as _;

        let status = cx.theme().status();
        let toasts = self
            .notifications
            .iter()
            .map(|notification| {
                let id = notification.id;
                let (accent, icon) = match notification.tone {
                    NotificationTone::Info => (status.info, IconName::Info),
                    NotificationTone::Success => (status.success, IconName::CircleCheck),
                    NotificationTone::Warning => (status.warning, IconName::TriangleAlert),
                    NotificationTone::Error => (status.error, IconName::CircleX),
                };
                // Each toast is a popover: the theme's popover surface,
                // shadow and radius, with only the icon carrying the tone.
                h_flex()
                    .id(format!("notification-{id}"))
                    .occlude()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .popover_style(cx)
                    .on_hover(cx.listener(move |center, hovered: &bool, _, cx| {
                        center.set_hovered(id, *hovered, cx);
                    }))
                    .child(
                        div().pt(px(2.0)).child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(Color::Custom(accent)),
                        ),
                    )
                    .child(
                        Label::new(notification.message.clone())
                            .size(LabelSize::XSmall)
                            .line_clamp(4)
                            .flex_1(),
                    )
                    .child(
                        IconButton::new(format!("dismiss-notification-{id}"), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(move |center, _, _, cx| {
                                center.dismiss(id, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();
        if toasts.is_empty() {
            return div().into_any_element();
        }

        // Rendered in the deferred popover layer so the stack floats above
        // modals, menus and other overlays instead of being painted (or
        // clipped) in the workspace's normal flow.
        let viewport = window.viewport_size();
        let corner = point(
            viewport.width - px(12.0),
            viewport.height - self.bottom_offset,
        );
        deferred(
            anchored()
                .position(corner)
                .anchor(Anchor::BottomRight)
                .snap_to_window()
                .child(v_flex().w(px(360.0)).gap_2().children(toasts)),
        )
        .with_priority(4)
        .into_any_element()
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
