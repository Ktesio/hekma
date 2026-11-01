use std::fmt::Display;

use console::{measure_text_width, pad_str, style, Alignment, Term};

const CHECK: &str = "✓";
const CROSS: &str = "✗";
const WARN: &str = "!";
const INFO: &str = "•";
const ELLIPSIS: &str = "…";
const TABLE_GAP: &str = "  ";
const MAX_TABLE_WIDTH: usize = 132;
const MIN_TABLE_WIDTH: usize = 40;

pub fn success(message: impl Display) {
    println!("{}", success_text(message));
}

pub fn warning(message: impl Display) {
    eprintln!("{}", warning_text(message));
}

pub fn error(message: impl Display) {
    eprintln!("{}", error_text(message));
}

/// Print an informational NOTICE to stderr (AD-12: results → stdout,
/// diagnostics/notices → stderr). A notice is neither a warning nor an error —
/// it is a neutral, dimmed "note:" line (e.g. the single-lifetime supervision
/// caveat on `hekma agent start`).
pub fn note(message: impl Display) {
    eprintln!("{}", note_text(message));
}

pub fn note_text(message: impl Display) -> String {
    format!("{} {}", style(format!("{INFO} note:")).dim(), message)
}

pub(crate) fn update_notice(current_version: &str, latest_version: &str) {
    eprintln!("{}", update_notice_text(current_version, latest_version));
}

pub(crate) fn update_notice_text(current_version: &str, latest_version: &str) -> String {
    format!(
        "{} Hekma {latest_version} is available (current {current_version}). Update: hekma self-update",
        warning_text("Update available:")
    )
}

pub fn info(message: impl Display) {
    println!("{} {}", style(INFO).cyan(), message);
}

pub fn success_text(message: impl Display) -> String {
    format!("{} {}", style(CHECK).green().bold(), message)
}

pub fn warning_text(message: impl Display) -> String {
    format!("{} {}", style(WARN).yellow().bold(), message)
}

pub fn error_text(message: impl Display) -> String {
    format!("{} {}", style(CROSS).red().bold(), message)
}

pub fn skill_name(name: &str) -> String {
    style(name).cyan().bold().to_string()
}

pub fn table_header(label: &str) -> String {
    style(label).color256(81).bold().to_string()
}

pub fn status_label(status: &str) -> String {
    match status {
        "installed" => format!("{} {}", style(CHECK).green().bold(), style(status).green()),
        "missing" => format!("{} {}", style(CROSS).red().bold(), style(status).red()),
        "not locked" | "not installed" => {
            format!("{} {}", style(WARN).yellow().bold(), style(status).yellow())
        }
        "orphaned" => format!(
            "{} {}",
            style(INFO).magenta().bold(),
            style(status).magenta()
        ),
        other => other.to_string(),
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CellStyle {
    Plain,
    Muted,
    Skill,
    Status,
}

#[derive(Clone, Debug)]
pub(crate) struct TableCell {
    text: String,
    style: CellStyle,
}

impl TableCell {
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self::new(text, CellStyle::Plain)
    }

    pub(crate) fn muted(text: impl Into<String>) -> Self {
        Self::new(text, CellStyle::Muted)
    }

    pub(crate) fn skill(text: impl Into<String>) -> Self {
        Self::new(text, CellStyle::Skill)
    }

    pub(crate) fn status(text: impl Into<String>) -> Self {
        Self::new(text, CellStyle::Status)
    }

    fn new(text: impl Into<String>, style: CellStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn styled_text(&self) -> String {
        match self.style {
            CellStyle::Plain => self.text.clone(),
            CellStyle::Muted => style(&self.text).dim().to_string(),
            CellStyle::Skill => skill_name(&self.text),
            CellStyle::Status => status_label(&self.text),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TableColumn {
    header: &'static str,
    min_width: usize,
    max_width: usize,
    /// Grows only after every capped column reaches its desired width.
    flexible: bool,
    align: Alignment,
}

impl TableColumn {
    /// A column `min_width` to `max_width` display columns wide. The cap is
    /// floored at the header's own display width, so a column never truncates
    /// its header at a width where the table fits; only the shrink pass of a
    /// table too wide for the terminal can clip a header.
    pub(crate) fn new(header: &'static str, min_width: usize, max_width: usize) -> Self {
        Self {
            header,
            min_width,
            max_width: max_width.max(measure_text_width(header)),
            flexible: false,
            align: Alignment::Left,
        }
    }

    /// A column that grows with the terminal: it takes the width the capped
    /// columns leave (up to its longest cell) and shrinks only when the table
    /// does not fit, truncating its cells with `…`.
    pub(crate) fn flexible(header: &'static str, min_width: usize) -> Self {
        Self {
            flexible: true,
            ..Self::new(header, min_width, MAX_TABLE_WIDTH)
        }
    }
}

pub(crate) fn print_table(title: &str, columns: &[TableColumn], rows: &[Vec<TableCell>]) {
    for line in render_table(title, columns, rows, table_display_width()) {
        println!("{line}");
    }
}

fn render_table(
    title: &str,
    columns: &[TableColumn],
    rows: &[Vec<TableCell>],
    display_width: usize,
) -> Vec<String> {
    let widths = table_widths(columns, rows, display_width);
    let table_width =
        widths.iter().sum::<usize>() + TABLE_GAP.len() * columns.len().saturating_sub(1);
    let mut lines = Vec::with_capacity(rows.len() + 3);

    lines.push(format!(
        "{} {} {}",
        style("▸").color256(81).bold(),
        style(title).bold(),
        style(row_count(rows.len())).dim()
    ));

    let headers = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            pad_str(
                &table_header(column.header),
                widths[index],
                column.align,
                Some(ELLIPSIS),
            )
            .into_owned()
        })
        .collect::<Vec<_>>();
    lines.push(headers.join(TABLE_GAP));
    lines.push(style("─".repeat(table_width)).dim().to_string());

    for row in rows {
        lines.push(format_row(columns, row, &widths));
    }

    lines
}

fn format_row(columns: &[TableColumn], row: &[TableCell], widths: &[usize]) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let cell = row
                .get(index)
                .cloned()
                .unwrap_or_else(|| TableCell::plain(""));
            pad_str(
                &cell.styled_text(),
                widths[index],
                column.align,
                Some(ELLIPSIS),
            )
            .into_owned()
        })
        .collect::<Vec<_>>()
        .join(TABLE_GAP)
}

fn table_widths(
    columns: &[TableColumn],
    rows: &[Vec<TableCell>],
    display_width: usize,
) -> Vec<usize> {
    if columns.is_empty() {
        return Vec::new();
    }

    let gap_width = TABLE_GAP.len() * columns.len().saturating_sub(1);
    let available = display_width
        .saturating_sub(gap_width)
        .max(columns.len() * 4);
    let mut widths = columns
        .iter()
        .map(|column| {
            let header_width = measure_text_width(column.header);
            column
                .min_width
                .max(header_width)
                .min(column.max_width.max(4))
        })
        .collect::<Vec<_>>();
    let desired = desired_widths(columns, rows);

    while widths.iter().sum::<usize>() > available {
        let Some(index) = widest_shrinkable_column(&widths) else {
            break;
        };
        widths[index] -= 1;
    }

    // Capped columns grow first, in column order, each up to its desired width;
    // only then the flexible ones, which take the width the capped columns leave
    // (`sort_by_key` is stable, so each group keeps its column order).
    let mut grow_order = (0..columns.len()).collect::<Vec<_>>();
    grow_order.sort_by_key(|&index| columns[index].flexible);

    loop {
        let total = widths.iter().sum::<usize>();
        if total >= available {
            break;
        }

        let Some(index) = grow_order.iter().copied().find(|&index| {
            widths[index] < desired[index] && widths[index] < columns[index].max_width
        }) else {
            break;
        };

        widths[index] += 1;
    }

    widths
}

fn desired_widths(columns: &[TableColumn], rows: &[Vec<TableCell>]) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let cell_width = rows
                .iter()
                .filter_map(|row| row.get(index))
                .map(|cell| measure_text_width(&cell.styled_text()))
                .max()
                .unwrap_or(0);
            column.max_width.min(
                column
                    .min_width
                    .max(measure_text_width(column.header))
                    .max(cell_width),
            )
        })
        .collect()
}

fn widest_shrinkable_column(widths: &[usize]) -> Option<usize> {
    widths
        .iter()
        .enumerate()
        .filter(|(_, width)| **width > 4)
        .max_by_key(|(_, width)| **width)
        .map(|(index, _)| index)
}

fn table_display_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| usize::from(Term::stdout().size().1))
        .clamp(MIN_TABLE_WIDTH, MAX_TABLE_WIDTH)
}

fn row_count(count: usize) -> String {
    let label = if count == 1 { "row" } else { "rows" };
    format!("({count} {label})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_labels_keep_status_text() {
        assert!(status_label("installed").contains("installed"));
        assert!(status_label("missing").contains("missing"));
        assert!(status_label("not locked").contains("not locked"));
        assert!(status_label("orphaned").contains("orphaned"));
        assert_eq!(status_label("custom"), "custom");
    }

    #[test]
    fn test_message_helpers_keep_plain_text() {
        assert!(success_text("Installed docs").contains("Installed docs"));
        assert!(warning_text("Skipped docs").contains("Skipped docs"));
        assert!(error_text("Failed docs").contains("Failed docs"));
    }

    #[test]
    fn test_update_notice_text_points_to_self_update_only() {
        let notice = update_notice_text("0.3.0", "0.4.0");

        assert!(notice.contains("Update available"));
        assert!(notice.contains("0.3.0"));
        assert!(notice.contains("0.4.0"));
        assert!(notice.contains("hekma self-update"));
        assert!(!notice.contains("cargo install"));
        assert!(!notice.contains("brew upgrade"));
        assert!(!notice.contains("github.com"));
    }

    #[test]
    fn test_render_table_respects_display_width() {
        let columns = [
            TableColumn::new("Name", 16, 30),
            TableColumn::new("Repo", 18, 64),
            TableColumn::new("Status", 12, 14),
        ];
        let rows = vec![vec![
            TableCell::skill("bmad-check-implementation-readiness"),
            TableCell::muted(
                "/Users/imagdy/dev/in1t.com/.agents/skills/bmad-check-implementation-readiness",
            ),
            TableCell::status("installed"),
        ]];

        for line in render_table("Skills", &columns, &rows, 60) {
            assert!(measure_text_width(&line) <= 60, "{line}");
        }
    }

    #[test]
    fn test_render_table_handles_missing_cells() {
        let columns = [
            TableColumn::new("Name", 8, 20),
            TableColumn::new("Count", 8, 9),
            TableColumn::new("Detail", 8, 24),
        ];
        let rows = vec![
            vec![
                TableCell::skill("demo"),
                TableCell::muted("42"),
                TableCell::plain("running"),
            ],
            vec![TableCell::plain("local")],
        ];

        let rendered = render_table("Instances", &columns, &rows, 80);

        assert!(rendered[0].contains("Instances"));
        assert!(rendered[0].contains("2 rows"));
        assert!(rendered.iter().any(|line| line.contains("42")));
        assert!(rendered.iter().any(|line| line.contains("local")));
    }

    #[test]
    fn test_print_helpers_do_not_panic() {
        let columns = [TableColumn::new("Name", 6, 12)];
        let rows = [vec![TableCell::plain("docs")]];
        print_table("Skills", &columns, &rows);
        update_notice("0.3.0", "0.4.0");
    }

    #[test]
    fn test_table_width_helpers_handle_empty_and_constrained_tables() {
        assert!(table_widths(&[], &[], 80).is_empty());
        assert_eq!(widest_shrinkable_column(&[4, 4, 4]), None);

        let columns = [
            TableColumn::new("A", 10, 30),
            TableColumn::new("B", 10, 30),
            TableColumn::new("C", 10, 30),
        ];
        let rows = vec![vec![
            TableCell::plain("a very long first value"),
            TableCell::plain("a very long second value"),
            TableCell::plain("a very long third value"),
        ]];
        let widths = table_widths(&columns, &rows, 24);

        assert_eq!(widths.len(), columns.len());
        assert!(widths.iter().sum::<usize>() <= 20);
    }

    #[test]
    fn test_table_column_cap_never_falls_below_its_header() {
        // A cap below the header's display width is floored at the header; a cap
        // at or above it is kept as given.
        let header = "Support (current OS)";
        let header_width = measure_text_width(header);
        assert_eq!(TableColumn::new(header, 14, 16).max_width, header_width);
        assert_eq!(TableColumn::new(header, 14, 24).max_width, 24);

        // So the header renders whole at every width where the table fits.
        let columns = [
            TableColumn::new("Capability", 12, 24),
            TableColumn::new(header, 14, 16),
        ];
        let rows = vec![vec![
            TableCell::skill("pause"),
            TableCell::status("guaranteed"),
        ]];
        for display_width in [MIN_TABLE_WIDTH, 80, 120, MAX_TABLE_WIDTH] {
            let widths = table_widths(&columns, &rows, display_width);
            assert!(widths[1] >= header_width, "{display_width}: {widths:?}");
            let rendered = render_table("Capabilities", &columns, &rows, display_width);
            assert!(rendered[1].contains(header), "{}", rendered[1]);
            assert!(!rendered[1].contains(ELLIPSIS), "{}", rendered[1]);
        }
    }

    #[test]
    fn test_flexible_column_grows_with_the_terminal_up_to_its_longest_cell() {
        // A Field/Value table like `hekma agent show`'s runtime status, with a
        // 78-column value: whole at 120 columns, truncated at 80.
        let value =
            "cumulative in 153990 / out 14410; this run: in 153990 / out 14410 / cached 512";
        let rows = vec![
            vec![TableCell::plain("State"), TableCell::status("paused")],
            vec![TableCell::plain("Usage (tokens)"), TableCell::plain(value)],
        ];
        let usage_row = |lines: &[String]| {
            lines
                .iter()
                .find(|line| line.starts_with("Usage (tokens)"))
                .cloned()
                .expect("the Usage row renders")
        };
        let flexible = [
            TableColumn::new("Field", 14, 20),
            TableColumn::flexible("Value", 14),
        ];

        // Room to spare: the column grows to its longest cell, and no further.
        assert_eq!(
            table_widths(&flexible, &rows, 120)[1],
            measure_text_width(value)
        );
        let wide = render_table("Runtime status", &flexible, &rows, 120);
        assert!(usage_row(&wide).contains(value), "{wide:#?}");

        // Too narrow for the whole row: the cell truncates with the ellipsis and
        // every line still fits the terminal.
        let narrow = render_table("Runtime status", &flexible, &rows, 80);
        let narrow_usage = usage_row(&narrow);
        assert!(narrow_usage.ends_with(ELLIPSIS), "{narrow_usage}");
        assert!(!narrow_usage.contains(value), "{narrow_usage}");
        for line in &narrow {
            assert!(measure_text_width(line) <= 80, "{line}");
        }

        // A fixed cap still stops at its cap with room to spare: growth is opt-in.
        let capped = [
            TableColumn::new("Field", 14, 20),
            TableColumn::new("Value", 14, 48),
        ];
        assert_eq!(table_widths(&capped, &rows, 120)[1], 48);
        let capped_usage = usage_row(&render_table("Runtime status", &capped, &rows, 120));
        assert!(capped_usage.ends_with(ELLIPSIS), "{capped_usage}");
    }

    #[test]
    fn test_flexible_column_still_lays_out_on_a_narrow_terminal() {
        // At the display-width floor and below it, a table with a long header and
        // a flexible column stays within the width without panicking. The header
        // is whole where the table fits (40); below that the shrink pass may clip it.
        let header = "Support (current OS)";
        let columns = [
            TableColumn::new(header, 14, 16),
            TableColumn::flexible("Value", 14),
        ];
        let rows = vec![vec![
            TableCell::status("guaranteed"),
            TableCell::plain("cumulative in 153990 / out 14410; this run: in 153990 / out 14410"),
        ]];
        for display_width in [MIN_TABLE_WIDTH, 24] {
            let rendered = render_table("Narrow", &columns, &rows, display_width);
            assert_eq!(rendered.len(), 4, "{rendered:#?}");
            for line in &rendered {
                assert!(
                    measure_text_width(line) <= display_width,
                    "{display_width}: {line}"
                );
            }
        }
        let floor = render_table("Narrow", &columns, &rows, MIN_TABLE_WIDTH);
        assert!(floor[1].contains(header), "{}", floor[1]);
    }

    #[test]
    fn test_capped_columns_grow_before_a_flexible_one() {
        // A `config get`-shaped table: the flexible Value column sits BEFORE two
        // capped columns, and its 104-column value could take every column the
        // table has. Capped columns grow first, so the Validated marker and the
        // Source label render whole and the value takes the width they leave.
        let value = "You are a careful payments triage assistant; route refunds to billing \
                     and disputes to the disputes queue";
        assert_eq!(measure_text_width(value), 104);
        let columns = [
            TableColumn::new("Key", 12, 40),
            TableColumn::flexible("Value", 12),
            TableColumn::new("Validated", 9, 12),
            TableColumn::new("Source", 12, 20),
        ];
        let rows = vec![vec![
            TableCell::plain("agent.system_prompt"),
            TableCell::plain(value),
            TableCell::plain("unvalidated"),
            TableCell::plain("instance"),
        ]];

        // Left-to-right growth let the value take the room first, leaving the
        // Validated column at its 9-column header ("unvalida…").
        assert_eq!(
            table_widths(&columns, &rows, 80)[2],
            measure_text_width("unvalidated")
        );
        for display_width in [80, 120] {
            // The flexible column takes exactly the rest of the table.
            let widths = table_widths(&columns, &rows, display_width);
            assert_eq!(
                widths.iter().sum::<usize>() + TABLE_GAP.len() * 3,
                display_width,
                "{widths:?}"
            );
            let rendered = render_table("Effective config", &columns, &rows, display_width);
            for line in &rendered {
                assert!(
                    measure_text_width(line) <= display_width,
                    "{display_width}: {line}"
                );
            }
            let row = &rendered[3];
            assert!(
                row.contains(&format!("{ELLIPSIS}  unvalidated  instance")),
                "{display_width}: {row}"
            );
            assert!(!row.contains(value), "{display_width}: {row}");
        }
    }

    #[test]
    fn test_row_count_uses_singular_and_plural() {
        assert_eq!(row_count(1), "(1 row)");
        assert_eq!(row_count(2), "(2 rows)");
    }
}
