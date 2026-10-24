use std::sync::{Arc, Weak};

use crate::db::StatementResult;

pub(super) const ROW_NUMBER_WIDTH: f32 = 48.0;
const COLUMN_WIDTH: f32 = 180.0;
const MIN_COLUMN_WIDTH: f32 = 64.0;
const MAX_COLUMN_WIDTH: f32 = 1200.0;

/// Column widths the user dragged for one result set. Widths belong to the result they were
/// set on, so a new execution or another result tab starts from the defaults again.
#[derive(Default)]
pub(super) struct ResultColumnWidths {
    source: Weak<StatementResult>,
    widths: Vec<f32>,
}

#[derive(Clone)]
pub(super) struct ResizeResultColumn(pub usize);

impl gpui_kit::Render for ResizeResultColumn {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        gpui_kit::Empty
    }
}

impl ResultColumnWidths {
    fn applies_to(&self, result: &StatementResult) -> bool {
        std::ptr::eq(self.source.as_ptr(), result)
    }

    pub(super) fn width(&self, result: &StatementResult, column: usize) -> f32 {
        self.applies_to(result)
            .then(|| self.widths.get(column).copied())
            .flatten()
            .unwrap_or(COLUMN_WIDTH)
    }

    /// Distance from the grid's left edge to the start of `column`.
    pub(super) fn start(&self, result: &StatementResult, column: usize) -> f32 {
        ROW_NUMBER_WIDTH
            + (0..column)
                .map(|index| self.width(result, index))
                .sum::<f32>()
    }

    pub(super) fn total(&self, result: &StatementResult) -> f32 {
        self.start(result, result.columns.len())
    }

    pub(super) fn resize(
        &mut self,
        result: &Arc<StatementResult>,
        column: usize,
        width: f32,
    ) -> bool {
        if column >= result.columns.len() {
            return false;
        }
        if !self.applies_to(result) {
            self.source = Arc::downgrade(result);
            self.widths.clear();
        }
        self.widths.resize(result.columns.len(), COLUMN_WIDTH);
        let width = width.clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH);
        if self.widths[column] == width {
            return false;
        }
        self.widths[column] = width;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(columns: usize) -> Arc<StatementResult> {
        Arc::new(StatementResult {
            sql: "SELECT 1".into(),
            success: true,
            error: None,
            columns: (0..columns)
                .map(|index| crate::db::ColumnInfo {
                    name: format!("c{index}"),
                    data_type: "text".into(),
                    nullable: true,
                    is_primary_key: false,
                    default_value: None,
                    comment: None,
                })
                .collect(),
            rows: vec![],
            affected_rows: 0,
            execution_time_ms: 1,
        })
    }

    #[test]
    fn resized_widths_only_apply_to_their_result() {
        let first = result(3);
        let second = result(3);
        let mut widths = ResultColumnWidths::default();

        assert!(widths.resize(&first, 1, 300.0));
        assert_eq!(widths.width(&first, 1), 300.0);
        assert_eq!(widths.start(&first, 2), ROW_NUMBER_WIDTH + 180.0 + 300.0);
        assert_eq!(
            widths.total(&first),
            ROW_NUMBER_WIDTH + 180.0 + 300.0 + 180.0
        );
        assert_eq!(widths.width(&second, 1), COLUMN_WIDTH);

        assert!(widths.resize(&second, 0, 10.0));
        assert_eq!(widths.width(&second, 0), MIN_COLUMN_WIDTH);
        assert_eq!(widths.width(&first, 1), COLUMN_WIDTH);
        assert!(!widths.resize(&second, 3, 200.0));
    }
}
