//! Bounded, local-only terminal download history.
//!
//! History is deliberately best-effort.  A corrupt, locked, or otherwise
//! unavailable database produces a fixed warning and never changes the
//! download result.  The database contains sanitized source identifiers and
//! filenames only; it is not a download resume store or an application log.

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, types::ValueRef, Connection, Row, TransactionBehavior};
use serde::Serialize;
use tauri::Manager;
use url::Url;

use crate::{
    manager::{DownloadStatus, JobState},
    security::{validate_url_syntax, DownloadMode, Platform},
};

const HISTORY_DB_FILE_NAME: &str = "download-history.sqlite3";
const HISTORY_SCHEMA_VERSION: i64 = 1;
const HISTORY_RETENTION_LIMIT: i64 = 1000;
const HISTORY_DISPLAY_LIMIT: usize = 100;
const HISTORY_MAX_TEXT_CHARS: usize = 256;
const HISTORY_MAX_SOURCE_CHARS: usize = 1024;
const HISTORY_MAX_TIMESTAMP_MS: i64 = 4_102_444_800_000; // 2100-01-01 UTC
const HISTORY_MAX_ELAPSED_MS: i64 = 315_360_000_000; // ten years
const HISTORY_UNAVAILABLE_WARNING: &str = "下載紀錄目前無法使用；下載與已完成媒體不受影響";
const HISTORY_DATA_WARNING: &str =
    "下載紀錄含有不受支援資料，部分欄位已安全遮罩；下載與已完成媒體不受影響";
const REDACTED_SOURCE: &str = "[來源已遮罩]";
const REDACTED_TEXT: &str = "[資料無法使用]";

const CREATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS download_history (
    job_id TEXT PRIMARY KEY NOT NULL,
    platform TEXT NOT NULL,
    mode TEXT NOT NULL,
    probe_title TEXT,
    source_link TEXT NOT NULL,
    terminal_status TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    output_filename TEXT,
    error_code TEXT,
    error_summary TEXT
)
"#;

#[derive(Debug, Clone, Serialize)]
pub struct HistoryRecord {
    pub job_id: String,
    pub platform: String,
    pub mode: String,
    pub probe_title: Option<String>,
    pub source_link: String,
    pub terminal_status: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub elapsed_ms: i64,
    pub output_filename: Option<String>,
    pub error_code: Option<String>,
    pub error_summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HistoryStats {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub cumulative_elapsed_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryList {
    pub records: Vec<HistoryRecord>,
    pub stats: HistoryStats,
    pub warning: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryClearResult {
    pub removed: usize,
    pub warning: Option<String>,
}

enum HistoryState {
    Uninitialized,
    Ready {
        connection: Connection,
        warning: Option<String>,
    },
    Unavailable,
}

#[derive(Clone)]
pub struct HistoryStore {
    state: Arc<Mutex<HistoryState>>,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(HistoryState::Uninitialized)),
        }
    }
}

impl HistoryStore {
    /// Initialize once during app setup.  All failures are converted to the
    /// fixed warning returned by list/clear; callers do not need to handle a
    /// database failure as an application startup error.
    pub fn initialize(&self, app: &tauri::AppHandle) {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(HISTORY_DB_FILE_NAME));
        if let Some(path) = path {
            self.initialize_at(&path);
        } else {
            self.mark_unavailable();
        }
    }

    fn initialize_at(&self, path: &Path) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !matches!(*state, HistoryState::Uninitialized) {
            return;
        }
        *state = match open_database(path) {
            Ok(connection) => HistoryState::Ready {
                connection,
                warning: None,
            },
            Err(_) => unavailable_state(),
        };
    }

    fn mark_unavailable(&self) {
        if let Ok(mut state) = self.state.lock() {
            if matches!(*state, HistoryState::Uninitialized) {
                *state = unavailable_state();
            }
        }
    }

    pub fn record_terminal(
        &self,
        status: &DownloadStatus,
        probe_title: Option<&str>,
        started_at_ms: i64,
    ) {
        let Some(input) = HistoryInput::from_status(status, probe_title, started_at_ms) else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let HistoryState::Ready {
            connection,
            warning,
        } = &mut *state
        {
            if insert_record(connection, &input).is_err() {
                set_warning_once(warning, HISTORY_UNAVAILABLE_WARNING);
            }
        }
    }

    pub fn list(&self) -> HistoryList {
        let Ok(mut state) = self.state.lock() else {
            return unavailable_list();
        };
        let HistoryState::Ready {
            connection,
            warning,
        } = &mut *state
        else {
            return unavailable_list();
        };
        match read_history(connection) {
            Ok((records, stats, malformed)) => {
                if malformed {
                    set_warning_once(warning, HISTORY_DATA_WARNING);
                }
                HistoryList {
                    records,
                    stats,
                    warning: warning.clone(),
                    limit: HISTORY_DISPLAY_LIMIT,
                }
            }
            Err(_) => {
                set_warning_once(warning, HISTORY_UNAVAILABLE_WARNING);
                unavailable_list()
            }
        }
    }

    pub fn clear(&self) -> HistoryClearResult {
        let Ok(mut state) = self.state.lock() else {
            return unavailable_clear();
        };
        let HistoryState::Ready {
            connection,
            warning,
        } = &mut *state
        else {
            return unavailable_clear();
        };
        let result = (|| -> rusqlite::Result<usize> {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let removed = tx.execute("DELETE FROM download_history", [])?;
            tx.commit()?;
            Ok(removed)
        })();
        match result {
            Ok(removed) => {
                // A successful clear is also the explicit recovery action for
                // a prior transient write/read warning.
                *warning = None;
                HistoryClearResult {
                    removed,
                    warning: None,
                }
            }
            Err(_) => {
                set_warning_once(warning, HISTORY_UNAVAILABLE_WARNING);
                unavailable_clear()
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_path(path: &Path) -> Self {
        let store = Self::default();
        store.initialize_at(path);
        store
    }
}

fn open_database(path: &Path) -> rusqlite::Result<Connection> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(rusqlite::Error::InvalidPath(path.to_path_buf()));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    }
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_millis(250))?;
    migrate(&mut connection)?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> rusqlite::Result<()> {
    let tx = connection.transaction()?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > HISTORY_SCHEMA_VERSION {
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::other("unsupported history schema"),
        )));
    }
    tx.execute_batch(CREATE_TABLE_SQL)?;
    if version == 0 {
        tx.execute(
            &format!("PRAGMA user_version = {HISTORY_SCHEMA_VERSION}"),
            [],
        )?;
    }
    tx.commit()
}

#[derive(Debug)]
struct HistoryInput {
    job_id: String,
    platform: String,
    mode: String,
    probe_title: Option<String>,
    source_link: String,
    terminal_status: String,
    started_at_ms: i64,
    finished_at_ms: i64,
    elapsed_ms: i64,
    output_filename: Option<String>,
    error_code: Option<String>,
    error_summary: Option<String>,
}

impl HistoryInput {
    fn from_status(
        status: &DownloadStatus,
        probe_title: Option<&str>,
        started_at_ms: i64,
    ) -> Option<Self> {
        let (terminal_status, error_code, error_summary) = match status.state {
            JobState::Completed => ("completed", None, None),
            JobState::Failed => {
                let (code, summary) = classify_error(status.error.as_deref());
                ("failed", Some(code), Some(summary))
            }
            JobState::Cancelled => ("cancelled", Some("cancelled"), Some("工作已取消")),
            JobState::Queued | JobState::Running => return None,
        };
        let finished_at_ms = now_ms();
        let started_at_ms = started_at_ms.max(0);
        Some(Self {
            job_id: truncate_chars(&status.id, HISTORY_MAX_TEXT_CHARS),
            platform: status.platform.as_str().to_string(),
            mode: mode_name(status.mode).to_string(),
            probe_title: probe_title.map(|title| truncate_chars(title, HISTORY_MAX_TEXT_CHARS)),
            source_link: sanitize_source_link(status.platform, &status.url),
            terminal_status: terminal_status.to_string(),
            started_at_ms,
            finished_at_ms,
            elapsed_ms: finished_at_ms.saturating_sub(started_at_ms),
            output_filename: sanitize_filename(status.output_path.as_deref()),
            error_code: error_code.map(str::to_string),
            error_summary: error_summary.map(str::to_string),
        })
    }
}

fn insert_record(connection: &mut Connection, input: &HistoryInput) -> rusqlite::Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        r#"INSERT INTO download_history
           (job_id, platform, mode, probe_title, source_link, terminal_status,
            started_at_ms, finished_at_ms, elapsed_ms, output_filename,
            error_code, error_summary)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
           ON CONFLICT(job_id) DO NOTHING"#,
        params![
            input.job_id,
            input.platform,
            input.mode,
            input.probe_title,
            input.source_link,
            input.terminal_status,
            input.started_at_ms,
            input.finished_at_ms,
            input.elapsed_ms,
            input.output_filename,
            input.error_code,
            input.error_summary,
        ],
    )?;
    tx.execute(
        r#"DELETE FROM download_history
           WHERE job_id NOT IN (
             SELECT job_id FROM download_history
             ORDER BY finished_at_ms DESC, job_id DESC
             LIMIT ?1
           )"#,
        params![HISTORY_RETENTION_LIMIT],
    )?;
    tx.commit()
}

fn read_history(
    connection: &mut Connection,
) -> rusqlite::Result<(Vec<HistoryRecord>, HistoryStats, bool)> {
    let mut statement = connection.prepare(
        r#"SELECT job_id, platform, mode, probe_title, source_link,
                  terminal_status, started_at_ms, finished_at_ms, elapsed_ms,
                  output_filename, error_code, error_summary
           FROM download_history
           ORDER BY finished_at_ms DESC, job_id DESC
           LIMIT ?1"#,
    )?;
    let mut records = Vec::with_capacity(HISTORY_DISPLAY_LIMIT);
    let mut stats = HistoryStats::default();
    let mut malformed = false;
    let rows = statement.query_map(params![HISTORY_RETENTION_LIMIT], sanitize_record)?;
    for row in rows {
        let (record, row_malformed) = row?;
        malformed |= row_malformed;
        stats.total = stats.total.saturating_add(1);
        match record.terminal_status.as_str() {
            "completed" => stats.completed = stats.completed.saturating_add(1),
            "failed" => stats.failed = stats.failed.saturating_add(1),
            "cancelled" => stats.cancelled = stats.cancelled.saturating_add(1),
            _ => {}
        }
        stats.cumulative_elapsed_ms = stats
            .cumulative_elapsed_ms
            .saturating_add(record.elapsed_ms.min(HISTORY_MAX_ELAPSED_MS));
        if records.len() < HISTORY_DISPLAY_LIMIT {
            records.push(record);
        }
    }
    drop(statement);
    Ok((records, stats, malformed))
}

fn sanitize_record(row: &Row<'_>) -> rusqlite::Result<(HistoryRecord, bool)> {
    let mut malformed = false;
    let (raw_job_id, job_id_malformed) = bounded_text(row.get_ref(0)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= job_id_malformed;
    let job_id = if raw_job_id.is_empty() {
        malformed = true;
        "[未知工作]".to_string()
    } else {
        raw_job_id
    };
    let (raw_platform, platform_text_malformed) =
        bounded_text(row.get_ref(1)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= platform_text_malformed;
    let (platform, parsed_platform) = sanitize_platform(&raw_platform);
    malformed |= parsed_platform.is_none();
    let (raw_mode, mode_text_malformed) = bounded_text(row.get_ref(2)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= mode_text_malformed;
    let (mode, parsed_mode) = sanitize_mode(&raw_mode);
    malformed |= !parsed_mode;
    let (probe_title, title_malformed) =
        bounded_optional_text(row.get_ref(3)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= title_malformed;
    let (raw_source, source_text_malformed) =
        bounded_text(row.get_ref(4)?, HISTORY_MAX_SOURCE_CHARS);
    malformed |= source_text_malformed;
    let source_link = parsed_platform.map_or_else(
        || REDACTED_SOURCE.to_string(),
        |platform| sanitize_source_link(platform, &raw_source),
    );
    malformed |= source_link == REDACTED_SOURCE;
    let (raw_status, status_text_malformed) = bounded_text(row.get_ref(5)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= status_text_malformed;
    let (terminal_status, parsed_status) = sanitize_terminal_status(&raw_status);
    malformed |= !parsed_status;
    let (started_raw, started_text_malformed) = read_i64(row.get_ref(6)?);
    let (finished_raw, finished_text_malformed) = read_i64(row.get_ref(7)?);
    let (elapsed_raw, elapsed_text_malformed) = read_i64(row.get_ref(8)?);
    let (started_at_ms, started_malformed) = safe_timestamp(started_raw);
    let (finished_at_ms, finished_malformed) = safe_timestamp(finished_raw);
    let (elapsed_ms, elapsed_malformed) = safe_elapsed(elapsed_raw);
    malformed |= started_text_malformed
        || finished_text_malformed
        || elapsed_text_malformed
        || started_malformed
        || finished_malformed
        || elapsed_malformed;
    let (raw_filename, filename_malformed) =
        bounded_optional_text(row.get_ref(9)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= filename_malformed;
    let output_filename = raw_filename
        .as_deref()
        .and_then(|path| sanitize_filename(Some(path)));
    let (raw_error_code, error_code_text_malformed) =
        bounded_optional_text(row.get_ref(10)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= error_code_text_malformed;
    let (error_code, parsed_error_code) = sanitize_error_code(raw_error_code.as_deref());
    malformed |= !parsed_error_code;
    let (raw_error_summary, error_summary_text_malformed) =
        bounded_optional_text(row.get_ref(11)?, HISTORY_MAX_TEXT_CHARS);
    malformed |= error_summary_text_malformed;
    let (error_summary, parsed_error_summary) =
        sanitize_error_summary(raw_error_summary.as_deref());
    malformed |= !parsed_error_summary;
    Ok((
        HistoryRecord {
            job_id: truncate_chars(&job_id, HISTORY_MAX_TEXT_CHARS),
            platform,
            mode,
            probe_title,
            source_link,
            terminal_status,
            started_at_ms,
            finished_at_ms,
            elapsed_ms,
            output_filename,
            error_code,
            error_summary,
        },
        malformed,
    ))
}

fn bounded_text(value: ValueRef<'_>, max_chars: usize) -> (String, bool) {
    let max_bytes = max_chars.saturating_mul(4).saturating_add(4);
    match value {
        ValueRef::Text(bytes) => {
            let capped = &bytes[..bytes.len().min(max_bytes)];
            let invalid_utf8 = std::str::from_utf8(capped).is_err();
            let text = String::from_utf8_lossy(capped);
            let malformed =
                invalid_utf8 || bytes.len() > max_bytes || text.chars().count() > max_chars;
            (truncate_chars(&text, max_chars), malformed)
        }
        ValueRef::Integer(value) => (value.to_string(), true),
        ValueRef::Real(value) if value.is_finite() => (value.to_string(), true),
        ValueRef::Real(_) => (REDACTED_TEXT.to_string(), true),
        ValueRef::Null => (String::new(), true),
        ValueRef::Blob(_) => (REDACTED_TEXT.to_string(), true),
    }
}

fn bounded_optional_text(value: ValueRef<'_>, max_chars: usize) -> (Option<String>, bool) {
    if matches!(value, ValueRef::Null) {
        return (None, false);
    }
    let (text, malformed) = bounded_text(value, max_chars);
    if text.is_empty() {
        (None, malformed)
    } else {
        (Some(text), malformed)
    }
}

fn read_i64(value: ValueRef<'_>) -> (i64, bool) {
    match value {
        ValueRef::Integer(value) => (value, false),
        ValueRef::Real(value) if value.is_finite() => {
            if value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                (value as i64, true)
            } else {
                (0, true)
            }
        }
        ValueRef::Text(value) => match std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
        {
            Some(value) => (value, true),
            None => (0, true),
        },
        ValueRef::Null | ValueRef::Blob(_) => (0, true),
        ValueRef::Real(_) => (0, true),
    }
}

fn safe_timestamp(value: i64) -> (i64, bool) {
    if (0..=HISTORY_MAX_TIMESTAMP_MS).contains(&value) {
        (value, false)
    } else {
        (0, true)
    }
}

fn safe_elapsed(value: i64) -> (i64, bool) {
    if (0..=HISTORY_MAX_ELAPSED_MS).contains(&value) {
        (value, false)
    } else {
        (0, true)
    }
}

fn sanitize_platform(value: &str) -> (String, Option<Platform>) {
    match value {
        "youtube" => ("youtube".to_string(), Some(Platform::Youtube)),
        "youtube-music" => ("youtube-music".to_string(), Some(Platform::YoutubeMusic)),
        "little-duck" => ("little-duck".to_string(), Some(Platform::LittleDuck)),
        "olevod" => ("olevod".to_string(), Some(Platform::Olevod)),
        "facebook" => ("facebook".to_string(), Some(Platform::Facebook)),
        _ => ("unknown".to_string(), None),
    }
}

fn sanitize_mode(value: &str) -> (String, bool) {
    match value {
        "audio" => ("audio".to_string(), true),
        "video" => ("video".to_string(), true),
        _ => ("unknown".to_string(), false),
    }
}

fn sanitize_terminal_status(value: &str) -> (String, bool) {
    match value {
        "completed" => ("completed".to_string(), true),
        "failed" => ("failed".to_string(), true),
        "cancelled" => ("cancelled".to_string(), true),
        _ => ("unknown".to_string(), false),
    }
}

fn sanitize_error_code(value: Option<&str>) -> (Option<String>, bool) {
    let Some(value) = value else {
        return (None, true);
    };
    match value {
        "cancelled"
        | "sidecar_unavailable"
        | "probe_failed"
        | "security_rejected"
        | "timeout"
        | "download_failed" => (Some(value.to_string()), true),
        _ => (Some("unknown".to_string()), false),
    }
}

fn sanitize_error_summary(value: Option<&str>) -> (Option<String>, bool) {
    let Some(value) = value else {
        return (None, true);
    };
    match value {
        "下載工具未就緒"
        | "啟動前檢查失敗"
        | "安全檢查拒絕"
        | "工作逾時"
        | "工作已取消"
        | "下載工作失敗" => (Some(value.to_string()), true),
        _ => (Some("歷史錯誤摘要不可用".to_string()), false),
    }
}

fn set_warning_once(warning: &mut Option<String>, message: &str) {
    if warning.is_none() {
        eprintln!("[history] {message}");
        *warning = Some(message.to_string());
    }
}

fn unavailable_state() -> HistoryState {
    eprintln!("[history] {HISTORY_UNAVAILABLE_WARNING}");
    HistoryState::Unavailable
}

fn unavailable_list() -> HistoryList {
    HistoryList {
        records: Vec::new(),
        stats: HistoryStats::default(),
        warning: Some(HISTORY_UNAVAILABLE_WARNING.to_string()),
        limit: HISTORY_DISPLAY_LIMIT,
    }
}

fn unavailable_clear() -> HistoryClearResult {
    HistoryClearResult {
        removed: 0,
        warning: Some(HISTORY_UNAVAILABLE_WARNING.to_string()),
    }
}

fn mode_name(mode: DownloadMode) -> &'static str {
    match mode {
        DownloadMode::Audio => "audio",
        DownloadMode::Video => "video",
    }
}

fn classify_error(error: Option<&str>) -> (&'static str, &'static str) {
    let error = error.unwrap_or("").to_ascii_lowercase();
    if error.contains("sidecar") {
        ("sidecar_unavailable", "下載工具未就緒")
    } else if error.contains("probe") {
        ("probe_failed", "啟動前檢查失敗")
    } else if error.contains("security") || error.contains("安全") {
        ("security_rejected", "安全檢查拒絕")
    } else if error.contains("timeout") || error.contains("逾時") {
        ("timeout", "工作逾時")
    } else if error.contains("cancel") || error.contains("取消") {
        ("cancelled", "工作已取消")
    } else {
        ("download_failed", "下載工作失敗")
    }
}

pub fn sanitize_source_link(platform: Platform, raw: &str) -> String {
    let Ok(validated) = validate_url_syntax(platform, raw) else {
        return REDACTED_SOURCE.to_string();
    };
    let Ok(mut parsed) = Url::parse(&validated.url) else {
        return REDACTED_SOURCE.to_string();
    };
    parsed.set_fragment(None);
    let host = validated.host;
    let sanitized = if matches!(host.as_str(), "youtu.be") {
        let id = parsed
            .path_segments()
            .and_then(|mut segments| segments.next())
            .filter(|value| safe_token(value));
        id.map_or_else(
            || format!("https://{host}/"),
            |id| format!("https://{host}/{id}"),
        )
    } else if matches!(parsed.path(), "/watch")
        && matches!(platform, Platform::Youtube | Platform::YoutubeMusic)
    {
        let id = parsed
            .query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
            .filter(|value| safe_token(value));
        id.map_or_else(
            || format!("https://{host}/watch"),
            |id| format!("https://{host}/watch?v={id}"),
        )
    } else if platform == Platform::Facebook {
        validated.url
    } else {
        let path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        format!("https://{host}{path}")
    };
    truncate_chars(&sanitized, 1024)
}

fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn sanitize_filename(path: Option<&str>) -> Option<String> {
    Path::new(path?)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| truncate_chars(name, HISTORY_MAX_TEXT_CHARS))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wmd-history-{label}-{}.sqlite3", Uuid::new_v4()))
    }

    fn status(id: &str, state: JobState, url: &str) -> DownloadStatus {
        DownloadStatus {
            id: id.to_string(),
            platform: Platform::Youtube,
            mode: DownloadMode::Video,
            url: url.to_string(),
            state,
            progress_percent: 100.0,
            speed_bps: None,
            eta_seconds: None,
            output_path: Some(r"C:\Users\private\Downloads\finished.mp4".to_string()),
            error: Some("raw stderr https://www.youtube.com/watch?v=secret".to_string()),
        }
    }

    fn record(store: &HistoryStore, id: &str, state: JobState, url: &str) {
        store.record_terminal(
            &status(id, state, url),
            Some("標題🙂"),
            now_ms().saturating_sub(1000),
        );
    }

    #[test]
    fn schema_is_created_and_migrated_to_version_one() {
        let path = test_path("schema");
        let store = HistoryStore::for_path(&path);
        let list = store.list();
        assert!(list.warning.is_none());
        let connection = Connection::open(&path).expect("database");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, HISTORY_SCHEMA_VERSION);
        let _: i64 = connection
            .query_row("SELECT COUNT(*) FROM download_history", [], |row| {
                row.get(0)
            })
            .expect("table");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn elapsed_limit_is_exactly_ten_years_in_milliseconds() {
        assert_eq!(HISTORY_MAX_ELAPSED_MS, 315_360_000_000);
        assert_eq!(
            safe_elapsed(HISTORY_MAX_ELAPSED_MS),
            (HISTORY_MAX_ELAPSED_MS, false)
        );
        assert_eq!(safe_elapsed(HISTORY_MAX_ELAPSED_MS + 1), (0, true));
    }

    #[test]
    fn newer_schema_is_fail_soft_without_overwriting_database() {
        let path = test_path("newer");
        let store = HistoryStore::for_path(&path);
        assert!(store.list().warning.is_none());
        drop(store);
        let connection = Connection::open(&path).expect("database");
        connection
            .execute("PRAGMA user_version = 99", [])
            .expect("version");
        drop(connection);
        let store = HistoryStore::for_path(&path);
        assert!(store.list().warning.is_some());
        let connection = Connection::open(&path).expect("database remains");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version remains");
        assert_eq!(version, 99);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sanitizer_keeps_safe_youtube_id_and_drops_secret_query_and_fragment() {
        assert_eq!(
            sanitize_source_link(
                Platform::Youtube,
                "  https://WWW.YouTube.com/watch?v=abc_DEF-123&token=secret#frag  "
            ),
            "https://www.youtube.com/watch?v=abc_DEF-123"
        );
        assert_eq!(
            sanitize_source_link(
                Platform::Youtube,
                "https://www.youtube.com/watch?v=abc%26bad&token=secret#frag"
            ),
            "https://www.youtube.com/watch"
        );
    }

    #[test]
    fn sanitizer_handles_youtu_be_and_hls_pages_without_query() {
        assert_eq!(
            sanitize_source_link(
                Platform::Youtube,
                "https://youtu.be/abc_DEF-123/ignored?si=secret#frag"
            ),
            "https://youtu.be/abc_DEF-123"
        );
        assert_eq!(
            sanitize_source_link(
                Platform::LittleDuck,
                "https://play.777tv.ai/v/episode?token=secret#frag"
            ),
            "https://play.777tv.ai/v/episode"
        );
        assert_eq!(
            sanitize_source_link(
                Platform::Olevod,
                "https://www.olevod.com/v/episode?token=secret#frag"
            ),
            "https://www.olevod.com/v/episode"
        );
    }

    #[test]
    fn sanitizer_keeps_only_facebook_public_ids() {
        assert_eq!(
            sanitize_source_link(
                Platform::Facebook,
                "https://www.facebook.com/100064322940906/videos/pcb.1529534909200593/2093187601588241#fragment",
            ),
            "https://www.facebook.com/100064322940906/videos/2093187601588241"
        );
        assert_eq!(
            sanitize_source_link(
                Platform::Facebook,
                "https://video.xx.fbcdn.example/secret.m3u8?token=secret",
            ),
            REDACTED_SOURCE
        );
    }

    #[test]
    fn sanitizer_rejects_invalid_source_without_returning_raw_url() {
        let raw = "https://evil.example/private?secret=sentinel";
        let sanitized = sanitize_source_link(Platform::Youtube, raw);
        assert_eq!(sanitized, REDACTED_SOURCE);
        assert!(!sanitized.contains(raw));
    }

    #[test]
    fn unicode_title_filename_and_error_are_bounded_and_deidentified() {
        let path = test_path("bounded");
        let store = HistoryStore::for_path(&path);
        let mut item = status(
            "bounded",
            JobState::Failed,
            "https://www.youtube.com/watch?v=safe",
        );
        item.output_path = Some(r"C:\Users\private\finished.mp4".to_string());
        let title = "字🙂".repeat(200);
        store.record_terminal(&item, Some(&title), 0);
        let listed = store.list();
        assert_eq!(listed.records.len(), 1);
        let record = &listed.records[0];
        assert_eq!(
            record.probe_title.as_ref().expect("title").chars().count(),
            256
        );
        assert_eq!(record.output_filename.as_deref(), Some("finished.mp4"));
        assert!(!record.source_link.contains("secret"));
        assert!(!record.error_summary.as_deref().unwrap().contains("stderr"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn completed_record_has_no_error_code_or_summary() {
        let path = test_path("completed-fields");
        let store = HistoryStore::for_path(&path);
        record(
            &store,
            "completed-fields",
            JobState::Completed,
            "https://www.youtube.com/watch?v=completed",
        );
        let listed = store.list();
        assert_eq!(listed.records[0].terminal_status, "completed");
        assert!(listed.records[0].error_code.is_none());
        assert!(listed.records[0].error_summary.is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn insert_is_idempotent_and_terminal_only() {
        let path = test_path("idempotent");
        let store = HistoryStore::for_path(&path);
        let active = status(
            "active",
            JobState::Running,
            "https://www.youtube.com/watch?v=active",
        );
        store.record_terminal(&active, None, 0);
        assert_eq!(store.list().stats.total, 0);
        record(
            &store,
            "one",
            JobState::Completed,
            "https://www.youtube.com/watch?v=one",
        );
        record(
            &store,
            "one",
            JobState::Failed,
            "https://www.youtube.com/watch?v=one",
        );
        assert_eq!(store.list().stats.total, 1);
        assert_eq!(store.list().records[0].terminal_status, "completed");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn list_is_latest_one_hundred_and_stats_cover_retained_set() {
        let path = test_path("latest");
        let store = HistoryStore::for_path(&path);
        for index in 0..120 {
            record(
                &store,
                &format!("job-{index:03}"),
                if index % 2 == 0 {
                    JobState::Completed
                } else {
                    JobState::Cancelled
                },
                "https://www.youtube.com/watch?v=latest",
            );
        }
        let listed = store.list();
        assert_eq!(listed.records.len(), 100);
        assert_eq!(listed.stats.total, 120);
        assert_eq!(listed.stats.completed, 60);
        assert_eq!(listed.stats.cancelled, 60);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn retention_is_bounded_to_one_thousand_and_clear_removes_only_history() {
        let path = test_path("retention");
        let store = HistoryStore::for_path(&path);
        for index in 0..1010 {
            record(
                &store,
                &format!("retained-{index:04}"),
                JobState::Completed,
                "https://www.youtube.com/watch?v=retained",
            );
        }
        let connection = Connection::open(&path).expect("database");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM download_history", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, HISTORY_RETENTION_LIMIT);
        drop(connection);
        let cleared = store.clear();
        assert_eq!(cleared.removed, HISTORY_RETENTION_LIMIT as usize);
        assert!(cleared.warning.is_none());
        assert_eq!(store.list().stats.total, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_database_is_fail_soft() {
        let path = test_path("corrupt");
        fs::write(&path, b"not a sqlite database").expect("corrupt database");
        let store = HistoryStore::for_path(&path);
        assert!(store.list().warning.is_some());
        assert!(store.clear().warning.is_some());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_rows_are_bounded_and_safely_fallback() {
        let path = test_path("malformed-row");
        let store = HistoryStore::for_path(&path);
        drop(store);
        let connection = Connection::open(&path).expect("database");
        connection
            .execute(
                r#"INSERT INTO download_history
                   (job_id, platform, mode, probe_title, source_link, terminal_status,
                    started_at_ms, finished_at_ms, elapsed_ms, output_filename,
                    error_code, error_summary)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
                params![
                    "job-".repeat(2000),
                    "future-platform",
                    "future-mode",
                    "標題🙂".repeat(2000),
                    "https://www.youtube.com/watch?v=safe&secret=sentinel#fragment",
                    "future-state",
                    i64::MIN,
                    i64::MAX,
                    i64::MAX,
                    format!(r#"C:\\private\\{}.mp4"#, "filename".repeat(2000)),
                    "unknown-error-code",
                    "raw stderr https://private.example/secret ".repeat(2000),
                ],
            )
            .expect("malformed row");
        drop(connection);

        let store = HistoryStore::for_path(&path);
        let listed = store.list();
        assert_eq!(listed.stats.total, 1);
        assert_eq!(listed.records.len(), 1);
        let record = &listed.records[0];
        assert_eq!(record.platform, "unknown");
        assert_eq!(record.mode, "unknown");
        assert_eq!(record.terminal_status, "unknown");
        assert!(record.job_id.chars().count() <= HISTORY_MAX_TEXT_CHARS);
        assert!(
            record.probe_title.as_ref().expect("title").chars().count() <= HISTORY_MAX_TEXT_CHARS
        );
        assert!(record.source_link.chars().count() <= HISTORY_MAX_SOURCE_CHARS);
        assert!(!record.source_link.contains("sentinel"));
        assert_eq!(record.started_at_ms, 0);
        assert_eq!(record.finished_at_ms, 0);
        assert_eq!(record.elapsed_ms, 0);
        assert!(
            record
                .output_filename
                .as_ref()
                .expect("filename")
                .chars()
                .count()
                <= HISTORY_MAX_TEXT_CHARS
        );
        assert_eq!(record.error_code.as_deref(), Some("unknown"));
        assert_eq!(record.error_summary.as_deref(), Some("歷史錯誤摘要不可用"));
        assert!(listed.warning.is_some());
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_utf8_value_is_marked_malformed() {
        let (text, malformed) = bounded_text(ValueRef::Text(&[0xff, b'x']), 256);
        assert!(malformed);
        assert!(text.contains('\u{fffd}'));
    }

    #[test]
    fn write_warning_is_fixed_and_cleared_after_successful_clear() {
        let path = test_path("warning-recovery");
        let store = HistoryStore::for_path(&path);
        let locker = Connection::open(&path).expect("database locker");
        locker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("exclusive lock");
        record(
            &store,
            "warning-first",
            JobState::Failed,
            "https://www.youtube.com/watch?v=warning-first",
        );
        let first_warning = store.list().warning.expect("fixed warning");
        assert_eq!(first_warning, HISTORY_UNAVAILABLE_WARNING);
        record(
            &store,
            "warning-second",
            JobState::Failed,
            "https://www.youtube.com/watch?v=warning-second",
        );
        assert_eq!(
            store.list().warning.as_deref(),
            Some(HISTORY_UNAVAILABLE_WARNING)
        );
        drop(locker);
        let cleared = store.clear();
        assert_eq!(cleared.removed, 0);
        assert!(cleared.warning.is_none());
        assert!(store.list().warning.is_none());
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn records_survive_store_drop_and_reopen() {
        let path = test_path("reopen");
        let store = HistoryStore::for_path(&path);
        record(
            &store,
            "reopen-job",
            JobState::Cancelled,
            "https://www.youtube.com/watch?v=reopen",
        );
        drop(store);
        let reopened = HistoryStore::for_path(&path);
        let listed = reopened.list();
        assert_eq!(listed.stats.total, 1);
        assert_eq!(listed.records[0].job_id, "reopen-job");
        assert_eq!(listed.records[0].terminal_status, "cancelled");
        drop(reopened);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn locked_database_is_fail_soft_after_bounded_busy_timeout() {
        let path = test_path("locked");
        let store = HistoryStore::for_path(&path);
        drop(store);
        let connection = Connection::open(&path).expect("database");
        connection
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("exclusive lock");
        let store = HistoryStore::for_path(&path);
        assert!(store.list().warning.is_some());
        drop(connection);
        let _ = fs::remove_file(path);
    }
}
