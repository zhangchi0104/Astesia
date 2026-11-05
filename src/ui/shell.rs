use crate::ui::components::prelude::*;
use gpui_kit::AnyWindowHandle;

use crate::platform::{DesktopPreferences, NativePreferencesStore, ThemePreference, UiLanguage};

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
    configure_notifications(cx);
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

/// Height of the workspace status bar; toasts sit just above it.
const STATUS_BAR_HEIGHT: f32 = 32.0;

/// Places gpui-kit notifications in the bottom-right corner, just above the
/// status bar. Called after every theme change so the settings stick.
fn configure_notifications(cx: &mut App) {
    let settings = &mut gpui_kit::component::Theme::global_mut(cx).notification;
    settings.placement = gpui_kit::Anchor::BottomRight;
    settings.margins.bottom = px(STATUS_BAR_HEIGHT + 12.0);
    settings.margins.right = px(12.0);
}

/// The main window, whose gpui-kit `Root` shows every in-app notification.
struct NotificationWindow(AnyWindowHandle);
impl gpui_kit::Global for NotificationWindow {}

/// Makes `window` the target of [`show_notification`].
pub(super) fn register_notification_window(window: AnyWindowHandle, cx: &mut App) {
    cx.set_global(NotificationWindow(window));
}

/// Marks notifications pushed by [`show_notification`], so repeating a
/// message replaces its toast instead of stacking a copy.
struct AppNotification;

/// Shows a gpui-kit notification toast in the main window's bottom-right
/// corner. It hides itself after a few seconds, pausing while hovered. Does
/// nothing before the workspace window exists (for example in view tests).
pub(crate) fn show_notification(tone: NotificationTone, message: impl Into<String>, cx: &mut App) {
    use gpui_kit::component::{notification::Notification, WindowExt as _};

    let Some(window) = cx.try_global::<NotificationWindow>().map(|global| global.0) else {
        return;
    };
    let message: SharedString = message.into().into();
    // Callers usually run inside the main window's own update, which cannot
    // be re-entered, so the push waits until the current effect cycle ends.
    cx.defer(move |cx| {
        let note = match tone {
            NotificationTone::Info => Notification::info(message.clone()),
            NotificationTone::Success => Notification::success(message.clone()),
            NotificationTone::Warning => Notification::warning(message.clone()),
            NotificationTone::Error => Notification::error(message.clone()),
        }
        .id1::<AppNotification>(message);
        if let Err(error) = window.update(cx, |_, window, cx| window.push_notification(note, cx)) {
            log::warn!("Could not show a notification: {error}");
        }
    });
}

pub(super) fn notify_preference_error(error: String, cx: &mut App) {
    show_notification(NotificationTone::Error, error, cx);
}

pub(super) struct UiLocale(pub UiLanguage);
impl gpui_kit::Global for UiLocale {}
