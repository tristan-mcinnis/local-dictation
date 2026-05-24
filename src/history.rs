//! Lightweight SQLite-backed dictation history.
//!
//! This is the friendly counterpart to the daemon log at
//! `/tmp/dictate-daemon.log`: the log records every timing, app target and
//! pipeline decision (useful for debugging, far too noisy for browsing),
//! whereas this stores just the *injected text* and *when* — the one thing a
//! user actually wants to skim later.
//!
//! Storage lives next to `settings.json` at
//! `~/.config/local-dictation/history.db`. A single table:
//!
//! ```sql
//! CREATE TABLE dictations (
//!     id         INTEGER PRIMARY KEY AUTOINCREMENT,
//!     text       TEXT    NOT NULL,
//!     created_at INTEGER NOT NULL   -- unix seconds, UTC
//! );
//! ```
//!
//! Writes are best-effort: a failure to persist history must never break the
//! dictation hot path, so `record` logs and swallows errors. Connections are
//! opened per-call — dictation happens at human speed, so the open cost is
//! irrelevant and we sidestep sharing a `Connection` across threads.
//!
//! Local-time formatting (the "MAY 22, 2026" date headers and "05:37 PM" row
//! times in the history window) is done in JavaScript inside the WebView via
//! `Intl`/`Date`, so the Rust side needs no timezone-aware date crate — it
//! just hands the WebView unix-millisecond timestamps.

use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One stored dictation.
#[derive(Debug, Clone)]
pub struct Entry {
    pub text: String,
    /// Creation time in unix seconds (UTC).
    pub created_at: i64,
}

/// `~/.config/local-dictation/history.db` (None if `$HOME` is unset).
fn db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("local-dictation")
            .join("history.db"),
    )
}

/// Open the history DB, creating the directory, file and schema as needed.
fn open() -> eyre::Result<Connection> {
    let path = db_path().ok_or_else(|| eyre::eyre!("cannot resolve history path ($HOME unset)"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| eyre::eyre!("create {}: {e}", dir.display()))?;
    }
    let conn = Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dictations (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             text       TEXT    NOT NULL,
             created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_dictations_created_at
             ON dictations (created_at DESC);",
    )?;
    Ok(conn)
}

/// Persist one dictation. Best-effort: errors are logged, never propagated,
/// so a history hiccup can't interrupt the inject hot path.
pub fn record(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Err(e) = try_record(text) {
        eprintln!("[history] record failed: {e}");
    }
}

fn try_record(text: &str) -> eyre::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let conn = open()?;
    conn.execute(
        "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
        rusqlite::params![text, now],
    )?;
    Ok(())
}

/// Most-recent-first dictations, capped at `limit`. Returns an empty vec on
/// any error (a missing DB just means "no history yet").
pub fn recent(limit: usize) -> Vec<Entry> {
    try_recent(limit).unwrap_or_default()
}

fn try_recent(limit: usize) -> eyre::Result<Vec<Entry>> {
    let conn = open()?;
    let mut stmt = conn.prepare(
        "SELECT text, created_at FROM dictations
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(Entry {
            text: row.get(0)?,
            created_at: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Render the history as a self-contained HTML document for the WebView.
///
/// Entries are embedded as a JSON array of `{t, text}` (t = unix ms); the
/// page's script groups them by local calendar day and renders the cream,
/// card-per-day layout. Doing the date math in the WebView gives correct
/// local-timezone "MAY 22, 2026" / "05:37 PM" formatting for free.
pub fn render_html(entries: &[Entry]) -> String {
    #[derive(serde::Serialize)]
    struct Row {
        t: i64,
        text: String,
    }
    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            t: e.created_at * 1000,
            text: e.text.clone(),
        })
        .collect();
    // serde_json output is safe to embed in <script> once we defang any
    // literal "</" (e.g. inside a transcript) so it can't close the tag early.
    let data = serde_json::to_string(&rows)
        .unwrap_or_else(|_| "[]".to_string())
        .replace("</", "<\\/");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {{
    --bg:      #fbfaf7;
    --card:    #fefdfb;
    --border:  #ececec;
    --rule:    #f1f0ec;
    --time:    #9a9a93;
    --text:    #2c2c2b;
    --header:  #8a8a83;
    --hover:   #f5f4f0;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg:     #1c1c1a;
      --card:   #242422;
      --border: #333330;
      --rule:   #2d2d2a;
      --time:   #807e77;
      --text:   #e8e7e2;
      --header: #8e8c84;
      --hover:  #2c2c29;
    }}
  }}
  * {{ box-sizing: border-box; }}
  html, body {{ margin: 0; padding: 0; }}
  body {{
    background: var(--bg);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
    -webkit-font-smoothing: antialiased;
    padding: 22px 26px 40px;
  }}
  .day {{ margin-bottom: 26px; }}
  .day-header {{
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.09em;
    color: var(--header);
    text-transform: uppercase;
    margin: 0 4px 10px;
  }}
  .card {{
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 14px;
    overflow: hidden;
  }}
  .row {{
    display: flex;
    align-items: baseline;
    gap: 18px;
    padding: 15px 22px;
    border-bottom: 1px solid var(--rule);
    cursor: default;
    transition: background 0.08s ease;
  }}
  .row:last-child {{ border-bottom: none; }}
  .row:hover {{ background: var(--hover); }}
  .time {{
    flex: 0 0 78px;
    color: var(--time);
    font-size: 15px;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }}
  .text {{
    flex: 1 1 auto;
    font-size: 16px;
    line-height: 1.45;
    color: var(--text);
    word-break: break-word;
  }}
  .empty {{
    text-align: center;
    color: var(--time);
    margin-top: 30vh;
    font-size: 15px;
  }}
</style>
</head>
<body>
<div id="app"></div>
<script>
const DATA = {data};

function fmtTime(d) {{
  return d.toLocaleTimeString([], {{ hour: '2-digit', minute: '2-digit' }});
}}
function fmtDay(d) {{
  return d.toLocaleDateString([], {{ month: 'long', day: 'numeric', year: 'numeric' }}).toUpperCase();
}}
function dayKey(d) {{
  return d.getFullYear() + '-' + d.getMonth() + '-' + d.getDate();
}}

function render() {{
  const app = document.getElementById('app');
  if (!DATA.length) {{
    app.innerHTML = '<div class="empty">No dictations yet.<br>Hold your hotkey and speak — they’ll show up here.</div>';
    return;
  }}
  // DATA is already newest-first. Group consecutively by local calendar day.
  const groups = [];
  let current = null;
  for (const item of DATA) {{
    const d = new Date(item.t);
    const key = dayKey(d);
    if (!current || current.key !== key) {{
      current = {{ key, label: fmtDay(d), rows: [] }};
      groups.push(current);
    }}
    current.rows.push({{ time: fmtTime(d), text: item.text }});
  }}

  const esc = (s) => s.replace(/[&<>]/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;'}}[c]));
  let html = '';
  for (const g of groups) {{
    html += '<div class="day"><div class="day-header">' + esc(g.label) + '</div><div class="card">';
    for (const r of g.rows) {{
      html += '<div class="row"><div class="time">' + esc(r.time) +
              '</div><div class="text">' + esc(r.text) + '</div></div>';
    }}
    html += '</div></div>';
  }}
  app.innerHTML = html;
}}
render();
</script>
</body>
</html>"#,
        data = data
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_html_handles_empty() {
        let html = render_html(&[]);
        assert!(html.contains("No dictations yet"));
        assert!(html.contains("const DATA = []"));
    }

    #[test]
    fn render_html_embeds_entries_and_defangs_script() {
        let entries = vec![Entry {
            // A transcript that, naively embedded, would close the <script>.
            text: "end the </script> tag & <b>more</b>".to_string(),
            created_at: 1_716_400_000,
        }];
        let html = render_html(&entries);
        // The literal closing tag must be neutralised inside the data blob.
        assert!(!html.contains("</script> tag"));
        assert!(html.contains("<\\/script>"));
        // Timestamp is emitted in milliseconds for the JS Date constructor.
        assert!(html.contains("1716400000000"));
    }
}
