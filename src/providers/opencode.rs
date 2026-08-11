//! Read-only `OpenCode` `SQLite` catalog collection.
//!
//! `OpenCode`'s local catalog uses WAL. This module first captures a bounded,
//! stable private copy of the database, WAL, and shared-memory files, then
//! reads that copy in a normal `SQLite` transaction. Only the private copy may
//! be changed by `SQLite`; it never opens or writes the provider-owned files.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use sqlx::{
    Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{
    providers::{
        ProviderId, ProviderRootError, ProviderScan, ProviderScanError, ProviderScanFuture,
        ProviderScanner, SourceOutcome,
    },
    storage::{
        CatalogItem, CatalogSession, SourceIdentity, SourceSnapshot, StorageError,
        scan_provider_with_pool,
    },
};

const DATABASE_FILE: &str = "opencode.db";
const MAX_SESSION_KEY_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ITEMS_PER_SESSION: usize = 2_000;
const MAX_SELECTED_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_JOINED_ROWS: i64 = 100_000;
const MAX_JOINED_JSON_BYTES: i64 = 64 * 1024 * 1024;
const MAX_PROVIDER_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Aggregate result of an `OpenCode` collection pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenCodeScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
}

/// OpenCode-owned source locations and native discovery rules.
#[derive(Clone, Debug)]
pub struct OpenCodeProvider {
    roots: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    MacOs,
    Linux,
}

impl Platform {
    const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

impl OpenCodeProvider {
    /// Resolves `OpenCode` roots using provider-native precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable absolute root can be resolved.
    pub fn resolve(configured: Option<&Path>) -> Result<Self, ProviderRootError> {
        Self::resolve_from(
            configured,
            env::var_os("XDG_DATA_HOME"),
            env::var_os("HOME"),
        )
    }

    pub(crate) fn resolve_from(
        configured: Option<&Path>,
        xdg_data_home: Option<OsString>,
        os_home: Option<OsString>,
    ) -> Result<Self, ProviderRootError> {
        Self::resolve_from_for(configured, xdg_data_home, os_home, Platform::current())
    }

    fn resolve_from_for(
        configured: Option<&Path>,
        xdg_data_home: Option<OsString>,
        os_home: Option<OsString>,
        platform: Platform,
    ) -> Result<Self, ProviderRootError> {
        let roots = if let Some(root) = configured {
            vec![root.to_path_buf()]
        } else if let Some(root) = xdg_data_home
            .map(PathBuf::from)
            .filter(|root| !root.as_os_str().is_empty() && root.is_absolute())
        {
            vec![root.join("opencode")]
        } else {
            let home = os_home
                .map(PathBuf::from)
                .filter(|home| !home.as_os_str().is_empty())
                .ok_or(ProviderRootError::OpenCodeHomeUnavailable)?;
            vec![match platform {
                Platform::MacOs => home.join("Library/Application Support/opencode"),
                Platform::Linux => home.join(".local/share/opencode"),
            }]
        };
        if let Some(path) = roots
            .iter()
            .find(|path| path.as_os_str().is_empty() || !path.is_absolute())
        {
            return Err(ProviderRootError::InvalidOpenCodeRoot { path: path.clone() });
        }
        Ok(Self { roots })
    }

    #[must_use]
    pub fn at_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    #[must_use]
    pub const fn id(&self) -> ProviderId {
        ProviderId::OpenCode
    }

    fn sources(&self) -> std::io::Result<Vec<(PathBuf, SourceIdentity)>> {
        let mut sources = Vec::new();
        for path in database_candidates(&self.roots) {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    let identity = source_identity(&path);
                    sources.push((path, identity));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(sources)
    }
}

/// Agentlog projection of OpenCode-native sources.
#[derive(Clone, Debug)]
pub struct OpenCodeScanner {
    provider: OpenCodeProvider,
}

impl OpenCodeScanner {
    #[must_use]
    pub fn new(provider: OpenCodeProvider) -> Self {
        Self { provider }
    }
}

impl ProviderScanner for OpenCodeScanner {
    fn provider_id(&self) -> ProviderId {
        self.provider.id()
    }

    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
        let sources = self.provider.sources()?;
        let candidate_sources = u64::try_from(sources.len()).unwrap_or(u64::MAX);
        Ok(Box::new(OpenCodeScan {
            candidate_sources,
            sources: sources.into_iter(),
        }))
    }
}

struct OpenCodeScan {
    candidate_sources: u64,
    sources: std::vec::IntoIter<(PathBuf, SourceIdentity)>,
}

impl ProviderScan for OpenCodeScan {
    fn candidate_sources(&self) -> u64 {
        self.candidate_sources
    }

    fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
        let Some((path, identity)) = self.sources.next() else {
            return Box::pin(async { Ok(None) });
        };
        Box::pin(async move {
            let outcome = match read_source(&path, identity.clone()).await {
                Ok(snapshot) => SourceOutcome::Accepted(snapshot),
                Err(message) => SourceOutcome::Failed { identity, message },
            };
            Ok(Some(outcome))
        })
    }
}

#[derive(Default)]
struct SessionBuilder {
    directory: Option<String>,
    model: Option<String>,
    started_at: Option<i64>,
    last_visible_event_at: Option<i64>,
    items: Vec<CatalogItem>,
    selected_text_bytes: usize,
    partial_reason: Option<PartialReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderFiles {
    database: Vec<u8>,
    wal: Option<Vec<u8>>,
    shm: Option<Vec<u8>>,
}

struct ReadSnapshot {
    _directory: tempfile::TempDir,
    database_path: PathBuf,
}

#[derive(Clone, Copy)]
enum PartialReason {
    VisibleTextTruncated,
    ItemCapacity,
    SessionTextCapacity,
}

impl PartialReason {
    const fn message(self) -> &'static str {
        match self {
            Self::VisibleTextTruncated => "visible text exceeded the catalog item bound",
            Self::ItemCapacity => "session exceeded the catalog item bound",
            Self::SessionTextCapacity => "session exceeded the catalog text bound",
        }
    }
}

/// Discovers and imports supported `OpenCode` `SQLite` catalogs from read-only roots.
///
/// Each database is a source-level atomic snapshot. An unsupported schema or
/// inconsistent relational identities updates only that source diagnostic and
/// preserves its last-good sessions.
///
/// # Errors
///
/// Returns an error when Agentlog cannot record source outcomes.
pub async fn collect_opencode(
    roots: &[PathBuf],
    catalog: &SqlitePool,
) -> Result<OpenCodeScanReport, StorageError> {
    let scanner = OpenCodeScanner::new(OpenCodeProvider::at_roots(roots.to_vec()));
    let report = scan_provider_with_pool(catalog, &scanner).await?;
    Ok(OpenCodeScanReport {
        candidate_sources: report.candidate_sources,
        refreshed_sources: report.refreshed_sources,
        partial_sources: report.partial_sources,
        failed_sources: report.failed_sources,
    })
}

fn database_candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = roots
        .iter()
        .map(|root| root.join(DATABASE_FILE))
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    candidates
}

fn source_identity(path: &Path) -> SourceIdentity {
    let canonical_locator = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    SourceIdentity {
        provider: "opencode",
        source_format: "sqlite_catalog",
        canonical_locator,
    }
}

async fn read_source(
    path: &Path,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let snapshot = copy_read_snapshot(path)?;
    let options = SqliteConnectOptions::new()
        .filename(&snapshot.database_path)
        .create_if_missing(false);
    let source = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|_| "could not open private OpenCode read snapshot")?;

    let result = read_source_snapshot(&source, identity).await;
    source.close().await;
    result
}

fn copy_read_snapshot(path: &Path) -> Result<ReadSnapshot, &'static str> {
    let before = read_provider_files(path)?;
    let directory = private_snapshot_directory()?;
    let destination = directory.path().join(DATABASE_FILE);
    fs::write(&destination, &before.database)
        .map_err(|_| "could not write OpenCode read snapshot")?;
    if let Some(wal) = &before.wal {
        fs::write(sidecar_path(&destination, "-wal"), wal)
            .map_err(|_| "could not write OpenCode read snapshot")?;
    }
    // The shared-memory file contains local mmap coordination state. Reusing
    // it at a different path is invalid; SQLite creates a private replacement
    // for this snapshot while the provider-owned SHM remains untouched.
    if read_provider_files(path)? != before {
        return Err("OpenCode source changed while its WAL snapshot was copied");
    }
    Ok(ReadSnapshot {
        _directory: directory,
        database_path: destination,
    })
}

fn private_snapshot_directory() -> Result<tempfile::TempDir, &'static str> {
    tempfile::TempDir::new().map_err(|_| "could not create an OpenCode read snapshot")
}

fn read_provider_files(path: &Path) -> Result<ProviderFiles, &'static str> {
    let database = read_provider_file(path)?.ok_or("could not inspect OpenCode provider file")?;
    let wal = read_provider_file(&sidecar_path(path, "-wal"))?;
    let shm = read_provider_file(&sidecar_path(path, "-shm"))?;
    let total = database
        .len()
        .checked_add(wal.as_ref().map_or(0, Vec::len))
        .and_then(|total| total.checked_add(shm.as_ref().map_or(0, Vec::len)))
        .ok_or("OpenCode provider files exceed the supported size limit")?;
    if total > usize::try_from(MAX_PROVIDER_FILE_BYTES).unwrap_or(usize::MAX) {
        return Err("OpenCode provider files exceed the supported size limit");
    }
    Ok(ProviderFiles { database, wal, shm })
}

fn read_provider_file(path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_PROVIDER_FILE_BYTES => {
            Err("OpenCode provider file exceeds the supported size limit")
        }
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|_| "could not inspect OpenCode provider file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("could not inspect OpenCode provider file"),
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

async fn read_source_snapshot(
    source: &SqlitePool,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let mut transaction = source
        .begin()
        .await
        .map_err(|_| "could not start OpenCode read transaction")?;
    validate_schema(&mut transaction).await?;
    validate_relational_identities(&mut transaction).await?;
    validate_snapshot_bounds(&mut transaction).await?;

    let rows = sqlx::query(
        "SELECT session.id AS session_id,
                session.directory AS session_directory,
                session.model AS session_model,
                session.time_created AS session_created,
                message.data AS message_data,
                message.time_created AS message_created,
                part.data AS part_data,
                part.time_created AS part_created
         FROM session
         LEFT JOIN message ON message.session_id = session.id
         LEFT JOIN part ON part.message_id = message.id
                       AND part.session_id = session.id
         ORDER BY session.time_created ASC,
                  session.id ASC,
                  message.time_created ASC,
                  message.id ASC,
                  part.time_created ASC,
                  part.id ASC",
    )
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| "could not read supported OpenCode transcript tables")?;
    transaction
        .commit()
        .await
        .map_err(|_| "could not finish OpenCode read transaction")?;

    let (sessions, partial_reason) = build_catalog_sessions(rows)?;
    Ok(SourceSnapshot {
        identity,
        diagnostic_status: if partial_reason.is_some() {
            "partial"
        } else {
            "ok"
        },
        diagnostic_message: partial_reason.map(PartialReason::message),
        sessions,
    })
}

fn build_catalog_sessions(
    rows: Vec<sqlx::sqlite::SqliteRow>,
) -> Result<(Vec<CatalogSession>, Option<PartialReason>), &'static str> {
    let mut session_builders = BTreeMap::<String, SessionBuilder>::new();
    for row in rows {
        let session_key = bounded_required_string(&row, "session_id", MAX_SESSION_KEY_BYTES)?;
        let session_created = required_timestamp(&row, "session_created")?;
        let builder = session_builders
            .entry(session_key)
            .or_insert_with(|| SessionBuilder {
                directory: bounded_optional_string(
                    &row,
                    "session_directory",
                    MAX_SESSION_KEY_BYTES,
                ),
                model: safe_model(bounded_optional_string(
                    &row,
                    "session_model",
                    MAX_SESSION_KEY_BYTES,
                )),
                started_at: Some(session_created),
                ..SessionBuilder::default()
            });

        let message_data = row
            .try_get::<Option<String>, _>("message_data")
            .map_err(|_| "OpenCode source contains an invalid message record")?;
        let part_data = row
            .try_get::<Option<String>, _>("part_data")
            .map_err(|_| "OpenCode source contains an invalid part record")?;
        let (message_data, part_data) = match (message_data, part_data) {
            (None | Some(_), None) => continue,
            (Some(message_data), Some(part_data)) => (message_data, part_data),
            _ => return Err("OpenCode source contains an inconsistent transcript record"),
        };
        let message = serde_json::from_str::<Value>(&message_data)
            .map_err(|_| "OpenCode source contains malformed message JSON")?;
        let part = serde_json::from_str::<Value>(&part_data)
            .map_err(|_| "OpenCode source contains malformed part JSON")?;
        let role = message.get("role").and_then(Value::as_str);
        if !matches!(role, Some("user" | "assistant"))
            || synthetic_message(&message)
            || synthetic_part(&part)
        {
            continue;
        }
        let item = match (role, part.get("type").and_then(Value::as_str)) {
            (Some("user"), Some("text")) => visible_text_item(&part, true),
            (Some("assistant"), Some("text")) => visible_text_item(&part, false),
            (Some("assistant"), Some("tool")) => tool_marker(&part),
            _ => None,
        };
        let Some((item, truncated)) = item else {
            continue;
        };
        if truncated {
            note_partial(
                &mut builder.partial_reason,
                PartialReason::VisibleTextTruncated,
            );
        }
        let _message_timestamp = required_timestamp(&row, "message_created")?;
        let item_timestamp = required_timestamp(&row, "part_created")?;
        if push_bounded_item(builder, item) {
            builder.last_visible_event_at =
                max_timestamp(builder.last_visible_event_at, Some(item_timestamp));
        }
    }

    let mut partial_reason = None;
    let mut catalog_sessions = Vec::new();
    for (session_key, builder) in session_builders {
        if let Some(reason) = builder.partial_reason {
            note_partial(&mut partial_reason, reason);
        }
        if let Some(session) = finish_session(session_key, builder) {
            catalog_sessions.push(session);
        }
    }
    Ok((catalog_sessions, partial_reason))
}

async fn validate_schema(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<(), &'static str> {
    for (table, required) in [
        (
            "session",
            ["id", "directory", "model", "time_created", "time_updated"].as_slice(),
        ),
        (
            "message",
            ["id", "session_id", "time_created", "data"].as_slice(),
        ),
        (
            "part",
            ["id", "message_id", "session_id", "time_created", "data"].as_slice(),
        ),
    ] {
        let query = format!("PRAGMA table_info({table})");
        let columns = sqlx::query(&query)
            .fetch_all(&mut **transaction)
            .await
            .map_err(|_| "could not inspect OpenCode schema")?;
        if !required.iter().all(|required| {
            columns.iter().any(|row| {
                row.try_get::<String, _>("name")
                    .is_ok_and(|name| name == *required)
            })
        }) {
            return Err("unsupported OpenCode SQLite schema");
        }
    }
    Ok(())
}

async fn validate_relational_identities(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<(), &'static str> {
    for query in [
        "SELECT EXISTS(SELECT 1 FROM message LEFT JOIN session ON session.id = message.session_id WHERE session.id IS NULL)",
        "SELECT EXISTS(SELECT 1 FROM part LEFT JOIN message ON message.id = part.message_id WHERE message.id IS NULL OR part.session_id != message.session_id)",
    ] {
        let invalid = sqlx::query_scalar::<_, i64>(query)
            .fetch_one(&mut **transaction)
            .await
            .map_err(|_| "could not validate OpenCode transcript identities")?;
        if invalid != 0 {
            return Err("OpenCode transcript contains inconsistent relational identities");
        }
    }
    Ok(())
}

async fn validate_snapshot_bounds(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<(), &'static str> {
    let metadata = sqlx::query(
        "SELECT COALESCE(MAX(LENGTH(directory)), 0) AS max_directory_bytes,
                COALESCE(MAX(LENGTH(model)), 0) AS max_model_bytes
         FROM session",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| "could not bound OpenCode session metadata")?;
    let max_directory = metadata
        .try_get::<i64, _>("max_directory_bytes")
        .map_err(|_| "could not bound OpenCode session metadata")?;
    let max_model = metadata
        .try_get::<i64, _>("max_model_bytes")
        .map_err(|_| "could not bound OpenCode session metadata")?;
    if max_directory > i64::try_from(MAX_SESSION_KEY_BYTES).unwrap_or(i64::MAX)
        || max_model > i64::try_from(MAX_SESSION_KEY_BYTES).unwrap_or(i64::MAX)
    {
        return Err("OpenCode session metadata exceeds the supported size limit");
    }
    let row = sqlx::query(
        "SELECT COUNT(*) AS joined_rows,
                COALESCE(SUM(COALESCE(LENGTH(message.data), 0) + COALESCE(LENGTH(part.data), 0)), 0) AS json_bytes
         FROM session
         LEFT JOIN message ON message.session_id = session.id
         LEFT JOIN part ON part.message_id = message.id
                       AND part.session_id = session.id",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| "could not bound OpenCode source snapshot")?;
    let rows = row
        .try_get::<i64, _>("joined_rows")
        .map_err(|_| "could not bound OpenCode source snapshot")?;
    let bytes = row
        .try_get::<i64, _>("json_bytes")
        .map_err(|_| "could not bound OpenCode source snapshot")?;
    if rows > MAX_JOINED_ROWS || bytes > MAX_JOINED_JSON_BYTES {
        return Err("OpenCode source exceeds the supported snapshot bounds");
    }
    Ok(())
}

fn bounded_required_string(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    max_bytes: usize,
) -> Result<String, &'static str> {
    let value = row
        .try_get::<String, _>(column)
        .map_err(|_| "OpenCode source contains an invalid session identity")?;
    let (value, truncated) = bounded_text(&value, max_bytes);
    if value.is_empty() || truncated {
        return Err("OpenCode source contains an invalid session identity");
    }
    Ok(value)
}

fn bounded_optional_string(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    max_bytes: usize,
) -> Option<String> {
    row.try_get::<Option<String>, _>(column)
        .ok()
        .flatten()
        .map(|value| bounded_text(&value, max_bytes).0)
        .filter(|value| !value.is_empty())
}

fn visible_text_item(part: &Value, user: bool) -> Option<(CatalogItem, bool)> {
    let text = part.get("text").and_then(Value::as_str)?;
    if text.is_empty() || (user && control_text(text)) {
        return None;
    }
    let (content, truncated) = bounded_text(text, MAX_TEXT_BYTES);
    Some((
        if user {
            CatalogItem::UserText(content)
        } else {
            CatalogItem::AssistantText(content)
        },
        truncated,
    ))
}

fn tool_marker(part: &Value) -> Option<(CatalogItem, bool)> {
    let tool = part.get("tool").and_then(Value::as_str)?;
    let (name, truncated) = bounded_text(tool, MAX_TOOL_NAME_BYTES);
    if name.is_empty() {
        return None;
    }
    let status = part
        .get("state")
        .and_then(Value::as_object)
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
        .and_then(allowed_tool_status)
        .map(ToOwned::to_owned);
    Some((CatalogItem::ToolMarker { name, status }, truncated))
}

fn synthetic_message(message: &Value) -> bool {
    ["isMeta", "isSynthetic", "synthetic"]
        .iter()
        .any(|field| message.get(*field).and_then(Value::as_bool) == Some(true))
}

fn synthetic_part(part: &Value) -> bool {
    ["isMeta", "isSynthetic", "synthetic", "ignored"]
        .iter()
        .any(|field| part.get(*field).and_then(Value::as_bool) == Some(true))
}

fn allowed_tool_status(status: &str) -> Option<&'static str> {
    match status {
        "pending" => Some("pending"),
        "running" => Some("running"),
        "completed" => Some("completed"),
        "error" => Some("error"),
        _ => None,
    }
}

fn control_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    [
        "<system-reminder>",
        "<task-notification>",
        "<command-message>",
        "<command-name>",
        "<teammate-message>",
        "<local-command-stdout>",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn push_bounded_item(builder: &mut SessionBuilder, item: CatalogItem) -> bool {
    if builder.items.len() == MAX_ITEMS_PER_SESSION {
        note_partial(&mut builder.partial_reason, PartialReason::ItemCapacity);
        return false;
    }
    if let CatalogItem::UserText(text) | CatalogItem::AssistantText(text) = &item {
        let remaining = MAX_SELECTED_TEXT_BYTES.saturating_sub(builder.selected_text_bytes);
        if remaining == 0 {
            note_partial(
                &mut builder.partial_reason,
                PartialReason::SessionTextCapacity,
            );
            return false;
        }
        if text.len() > remaining {
            let (content, _) = bounded_text(text, remaining);
            if content.is_empty() {
                note_partial(
                    &mut builder.partial_reason,
                    PartialReason::SessionTextCapacity,
                );
                return false;
            }
            builder.items.push(match item {
                CatalogItem::UserText(_) => CatalogItem::UserText(content),
                CatalogItem::AssistantText(_) => CatalogItem::AssistantText(content),
                CatalogItem::ToolMarker { .. } => unreachable!("text item matched above"),
            });
            builder.selected_text_bytes = MAX_SELECTED_TEXT_BYTES;
            note_partial(
                &mut builder.partial_reason,
                PartialReason::SessionTextCapacity,
            );
            return true;
        }
        builder.selected_text_bytes += text.len();
    }
    builder.items.push(item);
    true
}

fn finish_session(session_key: String, builder: SessionBuilder) -> Option<CatalogSession> {
    if !contains_visible_text(&builder.items) {
        return None;
    }
    let repository = builder.directory.as_deref().and_then(repository_from_cwd);
    let title = builder.items.iter().find_map(|item| match item {
        CatalogItem::UserText(text) => Some(short_title(text)),
        CatalogItem::AssistantText(_) | CatalogItem::ToolMarker { .. } => None,
    });
    Some(CatalogSession {
        session_key,
        title,
        repository,
        cwd: builder.directory,
        model: builder.model,
        execution_kind: Some("opencode_session".to_owned()),
        started_at: builder.started_at,
        last_visible_event_at: builder.last_visible_event_at,
        items: builder.items,
    })
}

fn contains_visible_text(items: &[CatalogItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            CatalogItem::UserText(_) | CatalogItem::AssistantText(_)
        )
    })
}

fn short_title(text: &str) -> String {
    const MAX_TITLE_BYTES: usize = 120;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    bounded_text(&normalized, MAX_TITLE_BYTES).0
}

fn repository_from_cwd(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
}

fn bounded_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn required_timestamp(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<i64, &'static str> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|_| "OpenCode source contains an invalid required timestamp")?;
    if value < 0 {
        return Err("OpenCode source contains an invalid required timestamp");
    }
    Ok(if value >= 1_000_000_000_000 {
        value / 1_000
    } else {
        value
    })
}

fn safe_model(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let candidate = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::String(value)) => value,
        Ok(Value::Object(object)) => object
            .get("id")
            .or_else(|| object.get("modelID"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)?,
        Ok(_) => return None,
        Err(_) => raw,
    };
    let (candidate, truncated) = bounded_text(&candidate, MAX_SESSION_KEY_BYTES);
    (!truncated
        && !candidate.is_empty()
        && candidate.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        }))
    .then_some(candidate)
}

fn max_timestamp(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (Some(current), None) => Some(current),
        (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

fn note_partial(current: &mut Option<PartialReason>, reason: PartialReason) {
    if current.is_none() {
        *current = Some(reason);
    }
}

#[cfg(test)]
pub(crate) async fn create_test_database(path: &Path) -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .expect("OpenCode fixture database");
    for statement in [
        "CREATE TABLE session (
             id TEXT PRIMARY KEY,
             directory TEXT NOT NULL,
             model TEXT,
             time_created INTEGER NOT NULL,
             time_updated INTEGER NOT NULL
         )",
        "CREATE TABLE message (
             id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             time_created INTEGER NOT NULL,
             data TEXT NOT NULL
         )",
        "CREATE TABLE part (
             id TEXT PRIMARY KEY,
             message_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             time_created INTEGER NOT NULL,
             data TEXT NOT NULL
         )",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("fixture schema");
    }
    sqlx::query(
        "INSERT INTO session (id, directory, model, time_created, time_updated)
         VALUES ('open-session', '/work', 'model', 1, 1)",
    )
    .execute(&pool)
    .await
    .expect("session");
    sqlx::query(
        "INSERT INTO message (id, session_id, time_created, data)
         VALUES ('message', 'open-session', 1, '{\"role\":\"user\"}')",
    )
    .execute(&pool)
    .await
    .expect("message");
    sqlx::query(
        "INSERT INTO part (id, message_id, session_id, time_created, data)
         VALUES (
             'part',
             'message',
             'open-session',
             1,
             '{\"type\":\"text\",\"text\":\"open visible\"}'
         )",
    )
    .execute(&pool)
    .await
    .expect("part");
    pool
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    use sqlx::{
        SqlitePool, query, query_scalar,
        sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    };
    use tempfile::TempDir;

    use crate::storage::{
        Catalog, CatalogScanError, SourceIdentity, SourceSnapshot, open_database,
        read_catalog_preview, replace_source_snapshot,
    };

    use super::{
        MAX_ITEMS_PER_SESSION, MAX_TEXT_BYTES, OpenCodeProvider, OpenCodeScanner, Platform,
        collect_opencode,
    };

    #[test]
    fn roots_are_target_specific_after_explicit_and_xdg_precedence() {
        struct Case {
            name: &'static str,
            configured: Option<&'static str>,
            xdg: Option<&'static str>,
            home: Option<&'static str>,
            platform: Platform,
            expected: &'static [&'static str],
        }

        let cases = [
            Case {
                name: "explicit config is authoritative",
                configured: Some("/configured/opencode"),
                xdg: Some("/xdg/data"),
                home: Some("/Users/example"),
                platform: Platform::Linux,
                expected: &["/configured/opencode"],
            },
            Case {
                name: "XDG is authoritative on macOS too",
                configured: None,
                xdg: Some("/xdg/data"),
                home: Some("/Users/example"),
                platform: Platform::MacOs,
                expected: &["/xdg/data/opencode"],
            },
            Case {
                name: "macOS fallback is Application Support",
                configured: None,
                xdg: None,
                home: Some("/Users/example"),
                platform: Platform::MacOs,
                expected: &["/Users/example/Library/Application Support/opencode"],
            },
            Case {
                name: "Linux fallback is local share",
                configured: None,
                xdg: None,
                home: Some("/home/example"),
                platform: Platform::Linux,
                expected: &["/home/example/.local/share/opencode"],
            },
            Case {
                name: "empty XDG falls back to the macOS default",
                configured: None,
                xdg: Some(""),
                home: Some("/Users/example"),
                platform: Platform::MacOs,
                expected: &["/Users/example/Library/Application Support/opencode"],
            },
            Case {
                name: "relative XDG falls back to the Linux default",
                configured: None,
                xdg: Some("relative/data"),
                home: Some("/home/example"),
                platform: Platform::Linux,
                expected: &["/home/example/.local/share/opencode"],
            },
        ];

        for case in cases {
            let provider = OpenCodeProvider::resolve_from_for(
                case.configured.map(Path::new),
                case.xdg.map(OsString::from),
                case.home.map(OsString::from),
                case.platform,
            )
            .expect(case.name);
            let expected = case.expected.iter().map(PathBuf::from).collect::<Vec<_>>();
            assert_eq!(provider.roots(), expected, "{}", case.name);
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct SourceFileSnapshot {
        bytes: Vec<u8>,
        size: u64,
        modified: std::time::SystemTime,
    }

    fn snapshot_file(path: &Path) -> Option<SourceFileSnapshot> {
        let metadata = fs::metadata(path).ok()?;
        Some(SourceFileSnapshot {
            bytes: fs::read(path).expect("read source file"),
            size: metadata.len(),
            modified: metadata.modified().expect("source file mtime"),
        })
    }

    fn source_files(path: &Path) -> [Option<SourceFileSnapshot>; 3] {
        [
            snapshot_file(path),
            snapshot_file(&PathBuf::from(format!("{}-wal", path.display()))),
            snapshot_file(&PathBuf::from(format!("{}-shm", path.display()))),
        ]
    }

    #[tokio::test]
    async fn incomplete_multi_root_discovery_preserves_last_good_without_marking_missing() {
        let temporary = TempDir::new().expect("temporary directory");
        let database_path = temporary.path().join("catalog.sqlite3");
        let pool = open_database(&database_path).await.expect("open catalog");
        replace_source_snapshot(
            &pool,
            &SourceSnapshot {
                identity: SourceIdentity {
                    provider: "opencode",
                    source_format: "sqlite_catalog",
                    canonical_locator: "test://last-good".to_owned(),
                },
                diagnostic_status: "ok",
                diagnostic_message: None,
                sessions: Vec::new(),
            },
        )
        .await
        .expect("store last-good source");
        pool.close().await;

        let oversized_component = "x".repeat(5_000);
        let scanner = OpenCodeScanner::new(OpenCodeProvider::at_roots(vec![
            temporary.path().join("absent"),
            temporary.path().join(oversized_component),
        ]));
        let catalog = Catalog::open(&database_path).await.expect("reopen catalog");
        let error = catalog
            .scan(&scanner)
            .await
            .expect_err("metadata failure must abort discovery");
        assert!(matches!(error, CatalogScanError::Provider(_)));
        let diagnostics = catalog
            .source_diagnostics()
            .await
            .expect("last-good diagnostics");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic_status, "ok");
        catalog.close().await;
    }

    async fn active_wal_source(root: &Path) -> (PathBuf, SqlitePool) {
        fs::create_dir_all(root).expect("create source root");
        let path = root.join("opencode.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .expect("open test OpenCode database");
        query("PRAGMA wal_autocheckpoint = 0")
            .execute(&pool)
            .await
            .expect("keep active WAL");
        query(
            "CREATE TABLE session (
                 id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL DEFAULT '',
                 slug TEXT NOT NULL DEFAULT '',
                 directory TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 version TEXT NOT NULL DEFAULT '',
                 model TEXT,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .expect("create session table");
        query(
            "CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .expect("create message table");
        query(
            "CREATE TABLE part (
                 id TEXT PRIMARY KEY,
                 message_id TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .expect("create part table");
        (path, pool)
    }

    async fn insert_session(pool: &SqlitePool, id: &str) {
        query(
            "INSERT INTO session (id, directory, model, time_created, time_updated)
             VALUES (?, '/work/repository', 'opencode-test', 1760000000000, 1760000009000)",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("insert session");
    }

    async fn insert_message_and_part(
        pool: &SqlitePool,
        message_id: &str,
        session_id: &str,
        part_id: &str,
        message_data: &str,
        part_data: &str,
        time: i64,
    ) {
        query(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(message_id)
        .bind(session_id)
        .bind(time)
        .bind(time)
        .bind(message_data)
        .execute(pool)
        .await
        .expect("insert message");
        query(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(part_id)
        .bind(message_id)
        .bind(session_id)
        .bind(time)
        .bind(time)
        .bind(part_data)
        .execute(pool)
        .await
        .expect("insert part");
    }

    async fn populate_private_fixture(writer: &SqlitePool) {
        insert_session(writer, "session-one").await;
        insert_message_and_part(
            writer,
            "message-user",
            "session-one",
            "part-user",
            r#"{"role":"user"}"#,
            r#"{"type":"text","text":"visible user request"}"#,
            1_760_000_001_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-assistant",
            "session-one",
            "part-assistant",
            r#"{"role":"assistant"}"#,
            r#"{"type":"text","text":"visible assistant answer"}"#,
            1_760_000_002_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-tool",
            "session-one",
            "part-tool",
            r#"{"role":"assistant"}"#,
            r#"{"type":"tool","tool":"Bash","callID":"PRIVATE_CALL","state":{"status":"completed","input":{"command":"PRIVATE_TOOL_INPUT"},"output":"PRIVATE_TOOL_OUTPUT","raw":"PRIVATE_RAW","error":"PRIVATE_ERROR","title":"PRIVATE_TITLE","metadata":{"secret":"PRIVATE_METADATA"}}}"#,
            1_760_000_003_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-assistant-literal",
            "session-one",
            "part-assistant-literal",
            r#"{"role":"assistant"}"#,
            r#"{"type":"text","text":"<system-reminder>assistant literal</system-reminder>"}"#,
            1_760_000_003_500,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-control",
            "session-one",
            "part-control",
            r#"{"role":"user","isMeta":true}"#,
            r#"{"type":"text","text":"<system-reminder>PRIVATE_SYSTEM</system-reminder>"}"#,
            1_760_000_004_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-reasoning",
            "session-one",
            "part-reasoning",
            r#"{"role":"assistant"}"#,
            r#"{"type":"reasoning","text":"PRIVATE_REASONING"}"#,
            1_760_000_005_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-synthetic-part",
            "session-one",
            "part-synthetic-part",
            r#"{"role":"user"}"#,
            r#"{"type":"text","text":"PRIVATE_SYNTHETIC_PART","isSynthetic":true}"#,
            1_760_000_006_000,
        )
        .await;
        insert_message_and_part(
            writer,
            "message-unknown-status",
            "session-one",
            "part-unknown-status",
            r#"{"role":"assistant"}"#,
            r#"{"type":"tool","tool":"UnknownStatus","state":{"status":"PRIVATE_STATUS"}}"#,
            1_760_000_007_000,
        )
        .await;
    }

    #[tokio::test]
    async fn reads_active_wal_without_changing_provider_files_or_storing_private_tool_data() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("opencode");
        let (source_path, writer) = active_wal_source(&root).await;
        populate_private_fixture(&writer).await;

        let before = source_files(&source_path);
        assert!(before[1].is_some(), "fixture must retain a WAL file");
        assert!(before[2].is_some(), "fixture must retain a SHM file");
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_opencode(&[root], &catalog)
            .await
            .expect("collect OpenCode");
        assert_eq!(report.failed_sources, 0);
        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.partial_sources, 0);
        let after = source_files(&source_path);
        assert!(
            after == before,
            "provider files changed: main={}, wal={}, shm={}",
            after[0] == before[0],
            after[1] == before[1],
            after[2] == before[2]
        );

        let session_id = query_scalar::<_, i64>("SELECT id FROM sessions")
            .fetch_one(&catalog)
            .await
            .expect("catalog session");
        let preview = read_catalog_preview(&catalog, session_id, 80, 4 * 1024)
            .await
            .expect("catalog preview");
        assert_eq!(preview.session.started_at, Some(1_760_000_000));
        assert_eq!(preview.session.last_visible_event_at, Some(1_760_000_007));
        let stored = query_scalar::<_, String>(
            "SELECT group_concat(COALESCE(content, tool_name || ':' || tool_status), '|')
             FROM transcript_items",
        )
        .fetch_one(&catalog)
        .await
        .expect("stored allowlist items");
        assert!(stored.contains("visible user request"));
        assert!(stored.contains("visible assistant answer"));
        assert!(stored.contains("assistant literal"));
        assert!(stored.contains("Bash:completed"));
        assert!(!stored.contains("PRIVATE_"));
        let unknown_status = query_scalar::<_, Option<String>>(
            "SELECT tool_status FROM transcript_items WHERE tool_name = 'UnknownStatus'",
        )
        .fetch_one(&catalog)
        .await
        .expect("unknown tool marker");
        assert_eq!(unknown_status, None);
        catalog.close().await;
        writer.close().await;
    }

    #[tokio::test]
    async fn unsupported_schema_and_inconsistent_rows_preserve_last_good_snapshot() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("opencode");
        let (_source_path, writer) = active_wal_source(&root).await;
        insert_session(&writer, "session-one").await;
        insert_message_and_part(
            &writer,
            "message-one",
            "session-one",
            "part-one",
            r#"{"role":"user"}"#,
            r#"{"type":"text","text":"last good request"}"#,
            1_760_000_001_000,
        )
        .await;
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        collect_opencode(std::slice::from_ref(&root), &catalog)
            .await
            .expect("initial scan");

        query(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES ('part-inconsistent', 'message-one', 'another-session', 1760000002000, 1760000002000, '{\"type\":\"text\",\"text\":\"PRIVATE_INCONSISTENT\"}')",
        )
        .execute(&writer)
        .await
        .expect("make identities inconsistent");
        let inconsistent = collect_opencode(std::slice::from_ref(&root), &catalog)
            .await
            .expect("inconsistent identity result");
        assert_eq!(inconsistent.failed_sources, 1);
        let retained_after_inconsistent =
            query_scalar::<_, String>("SELECT content FROM transcript_items")
                .fetch_one(&catalog)
                .await
                .expect("retained inconsistent last-good item");
        assert_eq!(retained_after_inconsistent, "last good request");
        query("DELETE FROM part WHERE id = 'part-inconsistent'")
            .execute(&writer)
            .await
            .expect("restore identities");

        query("UPDATE message SET data = '{bad json}' WHERE id = 'message-one'")
            .execute(&writer)
            .await
            .expect("make record malformed");
        let malformed = collect_opencode(std::slice::from_ref(&root), &catalog)
            .await
            .expect("malformed record result");
        assert_eq!(malformed.failed_sources, 1);
        let retained_after_malformed =
            query_scalar::<_, String>("SELECT content FROM transcript_items")
                .fetch_one(&catalog)
                .await
                .expect("retained malformed last-good item");
        assert_eq!(retained_after_malformed, "last good request");

        query("DROP TABLE part")
            .execute(&writer)
            .await
            .expect("make schema unsupported");
        let report = collect_opencode(&[root], &catalog)
            .await
            .expect("unsupported schema result");
        assert_eq!(report.failed_sources, 1);
        let retained = query_scalar::<_, String>("SELECT content FROM transcript_items")
            .fetch_one(&catalog)
            .await
            .expect("retained last-good item");
        assert_eq!(retained, "last good request");
        let status = query_scalar::<_, String>("SELECT diagnostic_status FROM sources")
            .fetch_one(&catalog)
            .await
            .expect("failure diagnostic");
        assert_eq!(status, "error");
        catalog.close().await;
        writer.close().await;
    }

    #[tokio::test]
    async fn bounds_are_partial_without_exposing_hidden_data() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("opencode");
        let (_source_path, writer) = active_wal_source(&root).await;
        insert_session(&writer, "session-one").await;
        let long = "x".repeat(128 * 1024 + 1);
        insert_message_and_part(
            &writer,
            "message-long",
            "session-one",
            "part-long",
            r#"{"role":"user"}"#,
            &serde_json::json!({ "type": "text", "text": long }).to_string(),
            1_760_000_001_000,
        )
        .await;
        for number in 0..MAX_ITEMS_PER_SESSION {
            insert_message_and_part(
                &writer,
                &format!("message-{number}"),
                "session-one",
                &format!("part-{number}"),
                r#"{"role":"assistant"}"#,
                r#"{"type":"tool","tool":"Read","state":{"status":"completed","input":"PRIVATE_INPUT"}}"#,
                1_760_000_003_000 + i64::try_from(number).expect("timestamp"),
            )
            .await;
        }
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_opencode(&[root], &catalog)
            .await
            .expect("bounded scan");
        assert_eq!(report.failed_sources, 0);
        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.partial_sources, 1);
        let count = query_scalar::<_, i64>("SELECT COUNT(*) FROM transcript_items")
            .fetch_one(&catalog)
            .await
            .expect("bounded items");
        assert_eq!(count, i64::try_from(MAX_ITEMS_PER_SESSION).expect("count"));
        let text_length = query_scalar::<_, i64>(
            "SELECT length(content) FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&catalog)
        .await
        .expect("bounded visible text");
        assert_eq!(text_length, i64::try_from(MAX_TEXT_BYTES).expect("length"));
        let stored = query_scalar::<_, String>(
            "SELECT group_concat(COALESCE(content, tool_name), '|') FROM transcript_items",
        )
        .fetch_one(&catalog)
        .await
        .expect("stored items");
        assert!(!stored.contains("PRIVATE_"));
        catalog.close().await;
        writer.close().await;
    }
}
