//! Running an arbitrary SQL statement and rendering the rows as text.
//!
//! Statements go through the text protocol (`sqlx::raw_sql`), so every value
//! arrives as its textual form and no column type needs to be known ahead of
//! time. Output mimics `mysql --batch`: a header row, tab-separated columns,
//! `NULL` for nulls, and tabs/newlines/backslashes escaped.

use sqlx::mysql::MySqlConnection;
use sqlx::{AssertSqlSafe, Column, Row};

use crate::Result;

/// Longest output returned by [`query`], so one careless `SELECT *` does not
/// flood a model's context.
pub const MAX_OUTPUT_CHARS: usize = 100_000;

/// Runs `sql` and renders the result; see the module docs for the format.
pub async fn query(conn: &mut MySqlConnection, sql: &str) -> Result<String> {
    let rows = sqlx::raw_sql(AssertSqlSafe(sql.to_owned()))
        .fetch_all(conn)
        .await?;
    let Some(first) = rows.first() else {
        return Ok("(no rows)".to_owned());
    };
    let columns: Vec<&str> = first.columns().iter().map(Column::name).collect();
    let cells = rows.iter().map(|row| {
        (0..row.len())
            .map(|i| {
                row.try_get_unchecked::<Option<&[u8]>, _>(i)
                    .ok()
                    .flatten()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            })
            .collect::<Vec<_>>()
    });
    Ok(truncate(format_table(&columns, cells), MAX_OUTPUT_CHARS))
}

/// Header plus one line per row, `mysql --batch` style.
pub fn format_table<I>(columns: &[&str], rows: I) -> String
where
    I: IntoIterator<Item = Vec<Option<String>>>,
{
    let mut out = columns
        .iter()
        .map(|c| escape(c))
        .collect::<Vec<_>>()
        .join("\t");
    for row in rows {
        out.push('\n');
        let line = row
            .iter()
            .map(|cell| cell.as_deref().map_or_else(|| "NULL".to_owned(), escape))
            .collect::<Vec<_>>()
            .join("\t");
        out.push_str(&line);
    }
    out
}

/// Cuts `text` to at most `max` characters, saying so when it does.
#[must_use]
pub fn truncate(mut text: String, max: usize) -> String {
    if let Some((cut, _)) = text.char_indices().nth(max) {
        text.truncate(cut);
        text.push_str("\n... (truncated; add a LIMIT or aggregate)");
    }
    text
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_a_header_nulls_and_escapes() {
        let rows = vec![
            vec![Some("Alina".to_owned()), Some("1265".to_owned())],
            vec![None, Some("a\tb\nc\\".to_owned())],
        ];
        assert_eq!(
            format_table(&["rep", "deals"], rows),
            "rep\tdeals\nAlina\t1265\nNULL\ta\\tb\\nc\\\\"
        );
    }

    #[test]
    fn long_output_is_truncated_on_a_char_boundary() {
        assert_eq!(truncate("ab".to_owned(), 5), "ab");
        let cut = truncate("ééé".to_owned(), 2);
        assert!(cut.starts_with("éé\n... (truncated"), "{cut}");
    }
}
