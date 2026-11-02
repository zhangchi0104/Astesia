use super::*;

impl AstesiaWorkspace {
    pub(super) fn settings_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let language = self.settings.read(cx).language();
        IconButton::new("open-settings", IconName::Settings)
            .icon_size(IconSize::Small)
            .aria_label(text(language, "设置", "Settings"))
            .tooltip(Tooltip::text(text(language, "设置 (⌘,)", "Settings (⌘,)")))
            .on_click(cx.listener(|workspace, _, _, cx| workspace.open_settings(cx)))
    }
}
