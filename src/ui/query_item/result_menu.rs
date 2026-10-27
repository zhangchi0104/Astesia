use super::*;
use crate::ui::components::{ContextMenu, ContextMenuEntry};

#[derive(Clone, Copy)]
pub(super) enum ResultMenuAction {
    Copy,
    CopyWithHeaders,
    CopyAsJson,
    SelectAll,
    ClearSelection,
    ToggleChart,
    CopyError,
    CopySql,
    CopyErrorAndSql,
}

impl QueryItem {
    /// Right-clicking an unselected cell selects it first, so Copy acts on what was clicked.
    pub(super) fn open_result_cell_menu(
        &mut self,
        row: usize,
        column: usize,
        position: gpui_kit::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.is_result_row_selected(row)
            && !self.state.is_result_cell_selected(row, column)
        {
            self.state.select_result_cell(row, column, false);
        }
        self.open_result_menu(position, window, cx);
    }

    pub(super) fn open_result_row_menu(
        &mut self,
        row: usize,
        position: gpui_kit::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.is_result_row_selected(row) {
            self.state.select_result_row(row, false, false);
        }
        self.open_result_menu(position, window, cx);
    }

    pub(super) fn open_result_menu(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.is_running() {
            return;
        }
        let language = self.settings.read(cx).language();
        use ResultMenuAction::*;
        let copy_error = (
            text(language, "复制错误信息", "Copy Error"),
            CopyError,
            false,
        );
        if self.state.error().is_some() {
            self.show_result_menu(vec![vec![copy_error]], position, window, cx);
            return;
        }
        let Some(result) = self.state.active_result() else {
            return;
        };
        if !result.success {
            let entries = vec![vec![
                copy_error,
                (text(language, "复制 SQL", "Copy SQL"), CopySql, false),
                (
                    text(language, "复制错误和 SQL", "Copy Error and SQL"),
                    CopyErrorAndSql,
                    false,
                ),
            ]];
            self.show_result_menu(entries, position, window, cx);
            return;
        }
        let can_chart = !result.columns.is_empty();
        let can_select = can_chart && !result.rows.is_empty();
        let has_selection = self.state.has_result_selection();
        let showing_chart = self.showing_chart;
        let entries = if showing_chart {
            vec![vec![(
                text(language, "显示表格", "Show Table"),
                ToggleChart,
                false,
            )]]
        } else {
            vec![
                vec![
                    (text(language, "复制", "Copy"), Copy, !has_selection),
                    (
                        text(language, "复制含表头", "Copy with Headers"),
                        CopyWithHeaders,
                        !has_selection,
                    ),
                    (
                        text(language, "复制为 JSON", "Copy as JSON"),
                        CopyAsJson,
                        !has_selection,
                    ),
                ],
                vec![
                    (text(language, "全选", "Select All"), SelectAll, !can_select),
                    (
                        text(language, "取消选择", "Clear Selection"),
                        ClearSelection,
                        !has_selection,
                    ),
                ],
                vec![(
                    text(language, "显示图表", "Show Chart"),
                    ToggleChart,
                    !can_chart,
                )],
            ]
        };
        self.show_result_menu(entries, position, window, cx);
    }

    fn show_result_menu(
        &mut self,
        entries: Vec<Vec<(&'static str, ResultMenuAction, bool)>>,
        position: gpui_kit::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for (index, group) in entries.iter().enumerate() {
                if index > 0 {
                    menu = menu.separator();
                }
                for &(label, action, disabled) in group {
                    let owner = owner.clone();
                    menu = menu.item(ContextMenuEntry::new(label).disabled(disabled).on_click(
                        move |_, window, cx| {
                            owner
                                .update(cx, |item, cx| {
                                    item.run_result_menu_action(action, window, cx)
                                })
                                .ok();
                        },
                    ));
                }
            }
            menu
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |item, menu, _: &gpui_kit::DismissEvent, window, cx| {
                if menu.focus_handle(cx).contains_focused(window, cx) {
                    item.focus_results(window, cx);
                }
                item.context_menu = None;
                cx.notify();
            },
        );
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn run_result_menu_action(
        &mut self,
        action: ResultMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ResultMenuAction::Copy => self.copy_result_selection(false, cx),
            ResultMenuAction::CopyWithHeaders => self.copy_result_selection(true, cx),
            ResultMenuAction::CopyAsJson => self.copy_result_selection_as_json(cx),
            ResultMenuAction::SelectAll => {
                if self.state.select_all_result_rows() {
                    cx.notify();
                }
            }
            ResultMenuAction::ClearSelection => self.clear_result_selection(cx),
            ResultMenuAction::ToggleChart => self.toggle_chart(&ClickEvent::default(), window, cx),
            ResultMenuAction::CopyError
            | ResultMenuAction::CopySql
            | ResultMenuAction::CopyErrorAndSql => {
                if let Some(text) = self.failure_text(action, cx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
        }
    }

    /// The text a failure menu item copies: the error as shown, the failed statement, or both.
    pub(super) fn failure_text(&self, action: ResultMenuAction, cx: &App) -> Option<String> {
        if let Some(error) = self.state.error() {
            return matches!(action, ResultMenuAction::CopyError)
                .then(|| format!("{} ({})", error.message, error.code));
        }
        let result = self
            .state
            .active_result()
            .filter(|result| !result.success)?;
        let error = result.error.clone().unwrap_or_else(|| {
            text(
                self.settings.read(cx).language(),
                "查询失败",
                "Query failed",
            )
            .to_string()
        });
        match action {
            ResultMenuAction::CopyError => Some(error),
            ResultMenuAction::CopySql => Some(result.sql.clone()),
            ResultMenuAction::CopyErrorAndSql => Some(format!("{error}\n\n{}", result.sql)),
            _ => None,
        }
    }

    fn focus_results(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.chart.as_ref().filter(|_| self.showing_chart) {
            Some(chart) => window.focus(&chart.read(cx).focus_handle(cx), cx),
            None => window.focus(&self.result_focus, cx),
        }
    }
}
