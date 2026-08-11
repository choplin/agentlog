//! Fresh `SQLite` schema v1 for Agentlog's normalized local catalog.
//!
//! The schema deliberately has no migration path. It accepts only a new empty
//! database or an existing v1 database, and refuses every other state without
//! modifying it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{
    QueryBuilder, Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use thiserror::Error;

use crate::providers::{ProviderScanError, ProviderScanReport, ProviderScanner, SourceOutcome};

pub const SCHEMA_VERSION: i64 = 1;
const TRANSCRIPT_ITEM_INSERT_CHUNK: usize = 500;
const MAX_CATALOG_PATH_COMPONENTS: usize = 128;
const CREATE_SOURCES: &str = "CREATE TABLE sources (\n             id INTEGER PRIMARY KEY,\n             provider TEXT NOT NULL,\n             source_format TEXT NOT NULL,\n             canonical_locator TEXT NOT NULL,\n             last_success_at INTEGER,\n             diagnostic_status TEXT NOT NULL DEFAULT 'unknown' CHECK(diagnostic_status IN ('unknown', 'ok', 'partial', 'error')),\n             diagnostic_message TEXT,\n             diagnostic_recorded_at INTEGER,\n             UNIQUE(provider, source_format, canonical_locator)\n         )";
const CREATE_SESSIONS: &str = "CREATE TABLE sessions (\n             id INTEGER PRIMARY KEY,\n             source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,\n             session_key TEXT NOT NULL,\n             title TEXT,\n             repository TEXT,\n             cwd TEXT,\n             model TEXT,\n             execution_kind TEXT,\n             started_at INTEGER,\n             last_visible_event_at INTEGER,\n             UNIQUE(source_id, session_key)\n         )";
const CREATE_TRANSCRIPT_ITEMS: &str = "CREATE TABLE transcript_items (\n             id INTEGER PRIMARY KEY,\n             session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,\n             ordinal INTEGER NOT NULL CHECK(ordinal >= 0),\n             item_kind TEXT NOT NULL CHECK(item_kind IN ('user_text', 'assistant_text', 'tool_marker')),\n             content TEXT,\n             tool_name TEXT,\n             tool_status TEXT,\n             UNIQUE(session_id, ordinal),\n             CHECK((item_kind IN ('user_text', 'assistant_text') AND content IS NOT NULL AND tool_name IS NULL AND tool_status IS NULL)\n                OR (item_kind = 'tool_marker' AND content IS NULL AND tool_name IS NOT NULL))\n         )";
const CREATE_SESSIONS_RECENT_INDEX: &str =
    "CREATE INDEX sessions_recent ON sessions(last_visible_event_at DESC, started_at DESC)";
/// Durable v1 cannot add a `missing` diagnostic status. Keep the stored state
/// compatible and project this exact marker as `missing` at read boundaries.
const MISSING_SOURCE_DIAGNOSTIC: &str = "source was absent from a completed provider discovery";

/// Agentlog's durable catalog and the use cases that preserve its invariants.
pub struct Catalog {
    pool: SqlitePool,
}

/// Observable progress while one provider's catalog transaction is assembled.
///
/// A source is reported only after its replacement or failure diagnostic has
/// been written to the open catalog transaction. The provider completion is
/// emitted by the application layer after this transaction commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogScanProgress {
    /// Provider discovery has completed and the candidate count is known.
    CandidatesDiscovered { candidate_sources: u64 },
    /// One source outcome has been written to the open transaction.
    SourceStaged {
        processed_sources: u64,
        candidate_sources: u64,
    },
}

/// Counts retained in an existing, validated Agentlog catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogCounts {
    pub sources: u64,
    pub sessions: u64,
    pub transcript_items: u64,
}

/// Constant-size cryptographic digests of content approved by a purge preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogContentToken {
    database: [u8; 32],
    wal: Option<[u8; 32]>,
}

impl CatalogContentToken {
    pub(crate) fn new(database: &[u8], wal: Option<&[u8]>) -> Self {
        Self {
            database: sha256(database),
            wal: wal.filter(|bytes| !bytes.is_empty()).map(sha256),
        }
    }
}

/// Counts and file-set size observed while the purge writer lock is held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CatalogPurgeResult {
    pub counts: CatalogCounts,
    pub approximate_bytes: u64,
}

/// Stable identity and observable metadata for one inspected catalog file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CatalogFileState {
    pub identity: CatalogFileIdentity,
    pub length: u64,
    pub modified: SystemTime,
}

/// Operating-system identity for the exact regular file inspected by Agentlog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CatalogFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    length: u64,
    #[cfg(not(unix))]
    modified: SystemTime,
}

impl CatalogFileState {
    pub(crate) fn from_metadata(
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<Self, StorageError> {
        let modified = metadata
            .modified()
            .map_err(|source| StorageError::CatalogPathIo {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            identity: CatalogFileIdentity::from_metadata(metadata, modified),
            length: metadata.len(),
            modified,
        })
    }
}

impl CatalogFileIdentity {
    fn from_metadata(metadata: &fs::Metadata, modified: SystemTime) -> Self {
        #[cfg(unix)]
        {
            let _ = modified;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                length: metadata.len(),
                modified,
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SchemaObject {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

impl Catalog {
    /// Opens the Agentlog-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened or validated.
    pub async fn open(path: &Path) -> Result<Self, StorageError> {
        Ok(Self {
            pool: open_database(path).await?,
        })
    }

    /// Synchronizes one provider and applies all source outcomes atomically.
    ///
    /// The scanner owns provider-native discovery and interpretation. Catalog
    /// owns persistence, aggregate diagnostics, and the provider-wide
    /// transaction boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when provider discovery or iteration fails, a scanner
    /// reports an inconsistent provider identity, or a catalog write fails.
    pub async fn scan(
        &self,
        scanner: &dyn ProviderScanner,
    ) -> Result<ProviderScanReport, CatalogScanError> {
        self.scan_with_progress(scanner, |_| {}).await
    }

    /// Synchronizes one provider while reporting transaction staging progress.
    ///
    /// The callback never receives provider-owned locators or transcript data.
    /// It is invoked after each source outcome has been staged successfully,
    /// but before the provider-wide transaction commits.
    ///
    /// # Errors
    ///
    /// Returns an error when provider discovery or iteration fails, a scanner
    /// reports an inconsistent provider identity, or a catalog write fails.
    pub async fn scan_with_progress<F>(
        &self,
        scanner: &dyn ProviderScanner,
        mut on_progress: F,
    ) -> Result<ProviderScanReport, CatalogScanError>
    where
        F: FnMut(CatalogScanProgress),
    {
        let provider = scanner.provider_id().as_str();
        let mut scan = scanner.start()?;
        let mut report = ProviderScanReport {
            candidate_sources: scan.candidate_sources(),
            ..ProviderScanReport::default()
        };
        on_progress(CatalogScanProgress::CandidatesDiscovered {
            candidate_sources: report.candidate_sources,
        });
        let mut transaction = self.pool.begin().await.map_err(StorageError::Query)?;
        let mut discovered = BTreeSet::new();

        while let Some(outcome) = scan.next_outcome().await? {
            match outcome {
                SourceOutcome::Accepted(snapshot) => {
                    validate_scanner_identity(provider, &snapshot.identity)?;
                    discovered.insert(source_key(&snapshot.identity));
                    let partial = snapshot.diagnostic_status == "partial";
                    replace_source_snapshot_in_transaction(&mut transaction, &snapshot).await?;
                    report.refreshed_sources += 1;
                    report.partial_sources += u64::from(partial);
                }
                SourceOutcome::Failed { identity, message } => {
                    validate_scanner_identity(provider, &identity)?;
                    discovered.insert(source_key(&identity));
                    record_source_failure_in_transaction(&mut transaction, &identity, message)
                        .await?;
                    report.failed_sources += 1;
                }
            }
            on_progress(CatalogScanProgress::SourceStaged {
                processed_sources: report.refreshed_sources + report.failed_sources,
                candidate_sources: report.candidate_sources,
            });
        }

        report.missing_sources =
            mark_missing_sources_in_transaction(&mut transaction, provider, &discovered).await?;

        transaction.commit().await.map_err(StorageError::Query)?;
        Ok(report)
    }

    /// Returns the number of cataloged sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be queried or the count cannot
    /// be represented by the public result type.
    pub async fn session_count(&self) -> Result<u64, StorageError> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&self.pool)
            .await
            .map_err(StorageError::Query)?;
        u64::try_from(count).map_err(|_| StorageError::CountOutOfRange)
    }

    /// Returns cataloged session counts keyed by provider identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be queried or a count cannot
    /// be represented by the public result type.
    pub async fn provider_session_counts(&self) -> Result<BTreeMap<String, u64>, StorageError> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT sources.provider, COUNT(sessions.id)
             FROM sources
             LEFT JOIN sessions ON sessions.source_id = sources.id
             GROUP BY sources.provider",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Query)?
        .into_iter()
        .map(|(provider, count)| {
            u64::try_from(count)
                .map(|count| (provider, count))
                .map_err(|_| StorageError::CountOutOfRange)
        })
        .collect()
    }

    /// Lists cataloged sessions without scanning provider-owned sources.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be queried.
    pub async fn list_sessions(
        &self,
        limit: u32,
    ) -> Result<Vec<CatalogSessionSummary>, StorageError> {
        list_catalog_sessions(&self.pool, limit).await
    }

    /// Reads one bounded catalog preview.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is unavailable or cannot be queried.
    pub async fn preview(&self, session_id: i64) -> Result<CatalogSessionPreview, StorageError> {
        read_catalog_preview(&self.pool, session_id, 80, 4 * 1024).await
    }

    /// Reads a current preview by the stable source-scoped session identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is unavailable or cannot be queried.
    pub async fn preview_by_identity(
        &self,
        session: &CatalogSessionSummary,
    ) -> Result<CatalogSessionPreview, StorageError> {
        read_catalog_preview_by_identity(&self.pool, session, 80, 4 * 1024).await
    }

    /// Returns every retained source diagnostic without scanning providers.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be queried.
    pub async fn source_diagnostics(&self) -> Result<Vec<CatalogSourceDiagnostic>, StorageError> {
        list_catalog_source_diagnostics(&self.pool).await
    }

    /// Closes the shared catalog connection pool.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

fn source_key(identity: &SourceIdentity) -> (String, String) {
    (
        identity.source_format.to_owned(),
        identity.canonical_locator.clone(),
    )
}

fn validate_scanner_identity(
    expected_provider: &'static str,
    identity: &SourceIdentity,
) -> Result<(), StorageError> {
    if identity.provider == expected_provider {
        Ok(())
    } else {
        Err(StorageError::ProviderIdentityMismatch {
            expected: expected_provider,
            found: identity.provider,
        })
    }
}

/// Runs a scanner through Catalog for legacy collector entry points that still
/// accept a raw pool.
pub(crate) async fn scan_provider_with_pool(
    pool: &SqlitePool,
    scanner: &dyn ProviderScanner,
) -> Result<ProviderScanReport, StorageError> {
    let catalog = Catalog { pool: pool.clone() };
    match catalog.scan(scanner).await {
        Ok(report) => Ok(report),
        Err(CatalogScanError::Storage(error)) => Err(error),
        Err(CatalogScanError::Provider(ProviderScanError::SourceIo(error))) => {
            Err(StorageError::SourceIo(error))
        }
    }
}

/// Failures while coordinating a provider scan with catalog persistence.
#[derive(Debug, Error)]
pub enum CatalogScanError {
    #[error(transparent)]
    Provider(#[from] ProviderScanError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// One provider-owned input with its stable, provider-scoped identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceIdentity {
    pub provider: &'static str,
    pub source_format: &'static str,
    pub canonical_locator: String,
}

/// A normalized transcript entry that is safe to persist in Agentlog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogItem {
    UserText(String),
    AssistantText(String),
    ToolMarker {
        name: String,
        status: Option<String>,
    },
}

/// One bounded, provider-native session in an accepted source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSession {
    pub session_key: String,
    pub title: Option<String>,
    pub repository: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub execution_kind: Option<String>,
    pub started_at: Option<i64>,
    pub last_visible_event_at: Option<i64>,
    pub items: Vec<CatalogItem>,
}

/// The accepted snapshot from one provider-owned input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    pub identity: SourceIdentity,
    pub diagnostic_status: &'static str,
    pub diagnostic_message: Option<&'static str>,
    pub sessions: Vec<CatalogSession>,
}

/// One session row suitable for the shared, read-only catalog list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogSessionSummary {
    pub id: i64,
    pub provider: String,
    pub source_format: String,
    #[serde(skip_serializing)]
    pub source_locator: String,
    pub session_key: String,
    pub title: Option<String>,
    pub repository: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub execution_kind: Option<String>,
    pub started_at: Option<i64>,
    pub last_visible_event_at: Option<i64>,
    pub source_diagnostic_status: String,
    pub source_last_success_at: Option<i64>,
}

/// The safe, bounded transcript stored for one selected catalog session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogSessionPreview {
    pub session: CatalogSessionSummary,
    pub items: Vec<CatalogItemView>,
    pub items_truncated: bool,
}

/// One source-level diagnostic suitable for the interactive catalog browser.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogSourceDiagnostic {
    pub provider: String,
    pub source_format: String,
    #[serde(skip_serializing)]
    pub source_locator: String,
    pub diagnostic_status: String,
    pub diagnostic_message: Option<String>,
    pub diagnostic_recorded_at: Option<i64>,
    pub last_success_at: Option<i64>,
}

/// A display-safe transcript item returned by the catalog preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogItemView {
    UserText {
        content: String,
    },
    AssistantText {
        content: String,
    },
    ToolMarker {
        name: String,
        status: Option<String>,
    },
}

/// Opens an Agentlog-owned database and ensures fresh schema v1 exists.
///
/// The rebuildable catalog uses WAL with `synchronous=NORMAL`: a sudden power
/// loss can lose recent derived catalog updates, but does not corrupt the
/// catalog or affect provider-owned logs. A later scan rebuilds the catalog.
///
/// # Errors
///
/// Returns an error when the database cannot be opened, has an unsupported
/// version, or cannot be initialized as a fresh v1 catalog.
pub async fn open_database(path: &Path) -> Result<SqlitePool, StorageError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(StorageError::Connect)?;

    ensure_schema(&pool).await?;
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await
        .map_err(StorageError::Query)?;
    sqlx::query("PRAGMA synchronous = NORMAL")
        .execute(&pool)
        .await
        .map_err(StorageError::Query)?;
    Ok(pool)
}

/// Inspects an existing catalog snapshot without creating or modifying it.
///
/// Callers must pass a private writable snapshot rather than the live Agentlog
/// catalog. The snapshot may include a `SQLite` WAL; `SQLite` creates a private
/// SHM file as needed without changing the catalog being described.
///
/// # Errors
///
/// Returns an error when the private database cannot be opened, does not contain
/// the current Agentlog schema, or its counts cannot be represented.
pub async fn inspect_catalog_snapshot(path: &Path) -> Result<CatalogCounts, StorageError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(StorageError::Connect)?;

    validate_existing_schema(&pool).await?;
    let sources = count_rows(&pool, "sources").await?;
    let sessions = count_rows(&pool, "sessions").await?;
    let transcript_items = count_rows(&pool, "transcript_items").await?;
    pool.close().await;

    Ok(CatalogCounts {
        sources,
        sessions,
        transcript_items,
    })
}

/// Inspects a regular catalog file through a symlink-free bounded path.
///
/// A missing component means that the requested catalog does not exist. Any
/// symbolic-link component or non-directory ancestor is rejected.
pub(crate) fn inspect_catalog_file_state(
    path: &Path,
) -> Result<Option<CatalogFileState>, StorageError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| StorageError::CatalogPathIo {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let components = absolute.components().collect::<Vec<_>>();
    let normal_components = components
        .iter()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    if normal_components > MAX_CATALOG_PATH_COMPONENTS {
        return Err(StorageError::UnsafeCatalogPath {
            path: path.to_path_buf(),
            reason: "has too many path components",
        });
    }

    let mut inspected = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                inspected.push(component.as_os_str());
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(StorageError::UnsafeCatalogPath {
                    path: path.to_path_buf(),
                    reason: "contains a parent-directory component",
                });
            }
        }
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }

        let metadata = match fs::symlink_metadata(&inspected) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StorageError::CatalogPathIo {
                    path: inspected,
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(StorageError::UnsafeCatalogPath {
                path: inspected,
                reason: "contains a symbolic-link component",
            });
        }

        let is_final = index + 1 == components.len();
        if !is_final && !metadata.is_dir() {
            return Err(StorageError::UnsafeCatalogPath {
                path: inspected,
                reason: "contains a non-directory ancestor",
            });
        }
        if is_final {
            if !metadata.is_file() {
                return Err(StorageError::UnsafeCatalogPath {
                    path: inspected,
                    reason: "does not name a regular file",
                });
            }
            return CatalogFileState::from_metadata(&inspected, &metadata).map(Some);
        }
    }

    Err(StorageError::UnsafeCatalogPath {
        path: path.to_path_buf(),
        reason: "does not name a file",
    })
}

fn verify_catalog_file_identity(
    path: &Path,
    expected: CatalogFileIdentity,
) -> Result<(), StorageError> {
    match inspect_catalog_file_state(path)? {
        Some(actual) if actual.identity == expected => Ok(()),
        Some(_) | None => Err(StorageError::CatalogFileIdentityChanged {
            path: path.to_path_buf(),
        }),
    }
}

/// Acquires `SQLite`'s writer serialization and purges every catalog row.
///
/// Schema validation, retained-count collection, complete main/WAL/SHM size
/// measurement, and row deletion happen while one `BEGIN IMMEDIATE`
/// transaction holds writer serialization. The schema itself remains.
///
/// # Errors
///
/// Returns an error when another writer is active, the database is not the
/// exact Agentlog v1 schema, or the transaction cannot be committed.
pub(crate) async fn purge_existing_catalog(
    path: &Path,
    expected_identity: CatalogFileIdentity,
    expected_content: Option<&CatalogContentToken>,
    max_file_set_bytes: u64,
    after_measurement: impl FnOnce(CatalogPurgeResult),
) -> Result<CatalogPurgeResult, StorageError> {
    purge_existing_catalog_with_hooks(
        path,
        expected_identity,
        expected_content,
        max_file_set_bytes,
        || {},
        after_measurement,
    )
    .await
}

async fn purge_existing_catalog_with_hooks(
    path: &Path,
    expected_identity: CatalogFileIdentity,
    expected_content: Option<&CatalogContentToken>,
    max_file_set_bytes: u64,
    after_open: impl FnOnce(),
    after_measurement: impl FnOnce(CatalogPurgeResult),
) -> Result<CatalogPurgeResult, StorageError> {
    verify_catalog_file_identity(path, expected_identity)?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::ZERO);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(StorageError::Connect)?;

    after_open();
    if let Err(error) = verify_catalog_file_identity(path, expected_identity) {
        pool.close().await;
        return Err(error);
    }

    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(StorageError::Query)?;
    verify_catalog_file_identity(path, expected_identity)?;
    if let Some(expected_content) = expected_content
        && let Err(error) = verify_catalog_content(path, expected_content, max_file_set_bytes)
    {
        transaction.rollback().await.map_err(StorageError::Query)?;
        pool.close().await;
        return Err(error);
    }
    validate_existing_schema_in_transaction(&mut transaction).await?;
    let counts = CatalogCounts {
        sources: count_rows_in_transaction(&mut transaction, "sources").await?,
        sessions: count_rows_in_transaction(&mut transaction, "sessions").await?,
        transcript_items: count_rows_in_transaction(&mut transaction, "transcript_items").await?,
    };
    let approximate_bytes = match inspect_catalog_file_set_size(path, max_file_set_bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            transaction.rollback().await.map_err(StorageError::Query)?;
            pool.close().await;
            return Err(error);
        }
    };
    let result = CatalogPurgeResult {
        counts,
        approximate_bytes,
    };
    after_measurement(result);
    sqlx::query("DELETE FROM sources")
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;
    if let Err(error) = verify_catalog_file_identity(path, expected_identity) {
        transaction.rollback().await.map_err(StorageError::Query)?;
        pool.close().await;
        return Err(error);
    }
    transaction.commit().await.map_err(StorageError::Query)?;
    pool.close().await;
    Ok(result)
}

fn verify_catalog_content(
    path: &Path,
    expected: &CatalogContentToken,
    max_bytes: u64,
) -> Result<(), StorageError> {
    let database = hash_bounded_catalog_component(path, max_bytes)?.ok_or_else(|| {
        StorageError::CatalogPreviewChanged {
            path: path.to_path_buf(),
        }
    })?;
    let remaining = max_bytes.saturating_sub(database.length);
    let wal = normalize_wal_digest(hash_bounded_catalog_component(
        &sqlite_sidecar_path(path, "-wal"),
        remaining,
    )?);
    if database.digest == expected.database && wal == expected.wal {
        Ok(())
    } else {
        Err(StorageError::CatalogPreviewChanged {
            path: path.to_path_buf(),
        })
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HashedCatalogComponent {
    digest: [u8; 32],
    length: u64,
}

fn normalize_wal_digest(wal: Option<HashedCatalogComponent>) -> Option<[u8; 32]> {
    wal.filter(|component| component.length != 0)
        .map(|component| component.digest)
}

fn hash_bounded_catalog_component(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<HashedCatalogComponent>, StorageError> {
    let Some(expected_state) = inspect_catalog_file_state(path)? else {
        return Ok(None);
    };
    if expected_state.length > max_bytes {
        return Err(StorageError::CatalogFileSetTooLarge {
            path: path.to_path_buf(),
            bytes: expected_state.length,
            max_bytes,
        });
    }
    let file = File::open(path).map_err(|source| StorageError::CatalogPathIo {
        path: path.to_path_buf(),
        source,
    })?;
    let opened_state = CatalogFileState::from_metadata(
        path,
        &file
            .metadata()
            .map_err(|source| StorageError::CatalogPathIo {
                path: path.to_path_buf(),
                source,
            })?,
    )?;
    if opened_state != expected_state {
        return Err(StorageError::CatalogPreviewChanged {
            path: path.to_path_buf(),
        });
    }
    let mut file = file;
    let mut hasher = Sha256::new();
    let mut remaining = expected_state.length;
    let mut buffer = [0_u8; 16 * 1024];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            StorageError::CatalogFileSetSizeOverflow {
                path: path.to_path_buf(),
            }
        })?;
        let read =
            file.read(&mut buffer[..limit])
                .map_err(|source| StorageError::CatalogPathIo {
                    path: path.to_path_buf(),
                    source,
                })?;
        if read == 0 {
            return Err(StorageError::CatalogPreviewChanged {
                path: path.to_path_buf(),
            });
        }
        hasher.update(&buffer[..read]);
        let read = u64::try_from(read).map_err(|_| StorageError::CatalogFileSetSizeOverflow {
            path: path.to_path_buf(),
        })?;
        remaining -= read;
    }
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|source| StorageError::CatalogPathIo {
            path: path.to_path_buf(),
            source,
        })?
        != 0
        || inspect_catalog_file_state(path)? != Some(expected_state)
    {
        return Err(StorageError::CatalogPreviewChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(HashedCatalogComponent {
        digest: hasher.finalize().into(),
        length: expected_state.length,
    }))
}

fn inspect_catalog_file_set_size(path: &Path, max_bytes: u64) -> Result<u64, StorageError> {
    let main = inspect_catalog_file_state(path)?.ok_or_else(|| {
        StorageError::CatalogFileIdentityChanged {
            path: path.to_path_buf(),
        }
    })?;
    let mut total = main.length;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(path, suffix);
        if let Some(state) = inspect_catalog_file_state(&sidecar)? {
            total = total.checked_add(state.length).ok_or_else(|| {
                StorageError::CatalogFileSetSizeOverflow {
                    path: path.to_path_buf(),
                }
            })?;
        }
    }
    if total > max_bytes {
        return Err(StorageError::CatalogFileSetTooLarge {
            path: path.to_path_buf(),
            bytes: total,
            max_bytes,
        });
    }
    Ok(total)
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Initializes only a truly empty database, or validates the current v1 marker.
///
/// # Errors
///
/// Returns an error when the schema version is unsupported, a nonempty database
/// has no Agentlog version, or required v1 tables are missing.
pub async fn ensure_schema(pool: &SqlitePool) -> Result<(), StorageError> {
    let version = scalar_i64(pool, "PRAGMA user_version").await?;

    match version {
        SCHEMA_VERSION => validate_existing_schema(pool).await,
        0 if database_has_no_user_objects(pool).await? => create_v1_schema(pool).await,
        0 => Err(StorageError::UnversionedDatabase),
        other => Err(StorageError::UnsupportedSchemaVersion { found: other }),
    }
}

async fn validate_existing_schema(pool: &SqlitePool) -> Result<(), StorageError> {
    let version = scalar_i64(pool, "PRAGMA user_version").await?;
    let objects = schema_objects(pool).await?;
    validate_schema_fingerprint(version, &objects)
}

async fn validate_existing_schema_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), StorageError> {
    let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(&mut **transaction)
        .await
        .map_err(StorageError::Query)?;
    let objects = schema_objects_in_transaction(transaction).await?;
    validate_schema_fingerprint(version, &objects)
}

async fn count_rows(pool: &SqlitePool, table: &'static str) -> Result<u64, StorageError> {
    count_rows_from_pool(pool, table).await
}

async fn count_rows_from_pool(pool: &SqlitePool, table: &'static str) -> Result<u64, StorageError> {
    let query = count_query(table);
    let count = scalar_i64(pool, query).await?;
    u64::try_from(count).map_err(|_| StorageError::CountOutOfRange)
}

async fn count_rows_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
) -> Result<u64, StorageError> {
    let query = count_query(table);
    let count = sqlx::query_scalar::<_, i64>(query)
        .fetch_one(&mut **transaction)
        .await
        .map_err(StorageError::Query)?;
    u64::try_from(count).map_err(|_| StorageError::CountOutOfRange)
}

fn count_query(table: &'static str) -> &'static str {
    match table {
        "sources" => "SELECT COUNT(*) FROM sources",
        "sessions" => "SELECT COUNT(*) FROM sessions",
        "transcript_items" => "SELECT COUNT(*) FROM transcript_items",
        _ => unreachable!("only catalog tables are counted"),
    }
}

async fn schema_objects(pool: &SqlitePool) -> Result<Vec<SchemaObject>, StorageError> {
    let rows = sqlx::query(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger', 'view')
           AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::Query)?;
    schema_objects_from_rows(&rows)
}

async fn schema_objects_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<Vec<SchemaObject>, StorageError> {
    let rows = sqlx::query(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger', 'view')
           AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut **transaction)
    .await
    .map_err(StorageError::Query)?;
    schema_objects_from_rows(&rows)
}

fn schema_objects_from_rows(
    rows: &[sqlx::sqlite::SqliteRow],
) -> Result<Vec<SchemaObject>, StorageError> {
    let mut objects = rows
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: row.try_get("type").map_err(StorageError::Query)?,
                name: row.try_get("name").map_err(StorageError::Query)?,
                table_name: row.try_get("tbl_name").map_err(StorageError::Query)?,
                sql: row.try_get("sql").map_err(StorageError::Query)?,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    objects.sort();
    Ok(objects)
}

fn validate_schema_fingerprint(version: i64, actual: &[SchemaObject]) -> Result<(), StorageError> {
    if version != SCHEMA_VERSION {
        return Err(if version == 0 {
            StorageError::UnversionedDatabase
        } else {
            StorageError::UnsupportedSchemaVersion { found: version }
        });
    }

    if actual == expected_schema_objects() {
        Ok(())
    } else {
        Err(StorageError::SchemaFingerprintMismatch)
    }
}

fn expected_schema_objects() -> Vec<SchemaObject> {
    let mut expected = vec![
        SchemaObject {
            object_type: "table".to_owned(),
            name: "sources".to_owned(),
            table_name: "sources".to_owned(),
            sql: Some(CREATE_SOURCES.to_owned()),
        },
        SchemaObject {
            object_type: "table".to_owned(),
            name: "sessions".to_owned(),
            table_name: "sessions".to_owned(),
            sql: Some(CREATE_SESSIONS.to_owned()),
        },
        SchemaObject {
            object_type: "table".to_owned(),
            name: "transcript_items".to_owned(),
            table_name: "transcript_items".to_owned(),
            sql: Some(CREATE_TRANSCRIPT_ITEMS.to_owned()),
        },
        SchemaObject {
            object_type: "index".to_owned(),
            name: "sessions_recent".to_owned(),
            table_name: "sessions".to_owned(),
            sql: Some(CREATE_SESSIONS_RECENT_INDEX.to_owned()),
        },
    ];
    expected.sort();
    expected
}

/// Atomically replaces the normalized sessions for one successfully read source.
///
/// The replacement is intentionally scoped to the source row. A different
/// source can fail without affecting this snapshot, and failures are recorded
/// through [`record_source_failure`] without deleting a previous good snapshot.
///
/// # Errors
///
/// Returns an error if the snapshot cannot be written as one transaction.
pub async fn replace_source_snapshot(
    pool: &SqlitePool,
    snapshot: &SourceSnapshot,
) -> Result<(), StorageError> {
    let mut transaction = pool.begin().await.map_err(StorageError::Query)?;
    replace_source_snapshot_in_transaction(&mut transaction, snapshot).await?;
    transaction.commit().await.map_err(StorageError::Query)
}

/// Replaces one source snapshot inside an already-open provider scan transaction.
///
/// The caller controls the outer transaction, so every statement that replaces
/// this one source stays atomic while a provider scan can commit many accepted
/// sources together.
pub(crate) async fn replace_source_snapshot_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    snapshot: &SourceSnapshot,
) -> Result<(), StorageError> {
    let now = unix_timestamp()?;

    let source_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO sources (provider, source_format, canonical_locator, last_success_at, diagnostic_status, diagnostic_message, diagnostic_recorded_at)\n         VALUES (?, ?, ?, ?, ?, ?, ?)\n         ON CONFLICT(provider, source_format, canonical_locator) DO UPDATE SET\n           last_success_at = excluded.last_success_at,\n           diagnostic_status = excluded.diagnostic_status,\n           diagnostic_message = excluded.diagnostic_message,\n           diagnostic_recorded_at = excluded.diagnostic_recorded_at\n         RETURNING id",
    )
    .bind(snapshot.identity.provider)
    .bind(snapshot.identity.source_format)
    .bind(&snapshot.identity.canonical_locator)
    .bind(now)
    .bind(snapshot.diagnostic_status)
    .bind(snapshot.diagnostic_message)
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(StorageError::Query)?;

    sqlx::query("DELETE FROM sessions WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut **transaction)
        .await
        .map_err(StorageError::Query)?;

    for session in &snapshot.sessions {
        let session_id = sqlx::query(
            "INSERT INTO sessions (source_id, session_key, title, repository, cwd, model, execution_kind, started_at, last_visible_event_at)\n             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(source_id)
        .bind(&session.session_key)
        .bind(&session.title)
        .bind(&session.repository)
        .bind(&session.cwd)
        .bind(&session.model)
        .bind(&session.execution_kind)
        .bind(session.started_at)
        .bind(session.last_visible_event_at)
        .execute(&mut **transaction)
        .await
        .map_err(StorageError::Query)?
        .last_insert_rowid();

        insert_transcript_items(&mut *transaction, session_id, &session.items).await?;
    }

    Ok(())
}

async fn insert_transcript_items(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: i64,
    items: &[CatalogItem],
) -> Result<(), StorageError> {
    for (chunk_index, chunk) in items.chunks(TRANSCRIPT_ITEM_INSERT_CHUNK).enumerate() {
        let chunk_offset = chunk_index
            .checked_mul(TRANSCRIPT_ITEM_INSERT_CHUNK)
            .ok_or(StorageError::OrdinalOverflow)?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO transcript_items (session_id, ordinal, item_kind, content, tool_name, tool_status) ",
        );
        builder.push_values(chunk.iter().enumerate(), |mut row, (within_chunk, item)| {
            let ordinal = chunk_offset
                .checked_add(within_chunk)
                .and_then(|value| i64::try_from(value).ok())
                .expect("source session item ordinal was checked before insertion");
            row.push_bind(session_id).push_bind(ordinal);
            match item {
                CatalogItem::UserText(content) => {
                    row.push_bind("user_text")
                        .push_bind(Some(content.as_str()))
                        .push_bind(Option::<&str>::None)
                        .push_bind(Option::<&str>::None);
                }
                CatalogItem::AssistantText(content) => {
                    row.push_bind("assistant_text")
                        .push_bind(Some(content.as_str()))
                        .push_bind(Option::<&str>::None)
                        .push_bind(Option::<&str>::None);
                }
                CatalogItem::ToolMarker { name, status } => {
                    row.push_bind("tool_marker")
                        .push_bind(Option::<&str>::None)
                        .push_bind(Some(name.as_str()))
                        .push_bind(status.as_deref());
                }
            }
        });
        builder
            .build()
            .execute(&mut **transaction)
            .await
            .map_err(StorageError::Query)?;
    }
    Ok(())
}

/// Records a source-level failure while preserving its previous session rows.
///
/// # Errors
///
/// Returns an error when the source diagnostic cannot be persisted.
pub async fn record_source_failure(
    pool: &SqlitePool,
    identity: &SourceIdentity,
    diagnostic_message: &'static str,
) -> Result<(), StorageError> {
    record_source_diagnostic(pool, identity, "error", diagnostic_message).await
}

/// Records one failed source inside an already-open provider scan transaction.
pub(crate) async fn record_source_failure_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    identity: &SourceIdentity,
    diagnostic_message: &'static str,
) -> Result<(), StorageError> {
    record_source_diagnostic_in_transaction(transaction, identity, "error", diagnostic_message)
        .await
}

/// Updates a source diagnostic without replacing its last-good snapshot.
///
/// # Errors
///
/// Returns an error when the source diagnostic cannot be persisted.
pub async fn record_source_diagnostic(
    pool: &SqlitePool,
    identity: &SourceIdentity,
    diagnostic_status: &'static str,
    diagnostic_message: &'static str,
) -> Result<(), StorageError> {
    let mut transaction = pool.begin().await.map_err(StorageError::Query)?;
    record_source_diagnostic_in_transaction(
        &mut transaction,
        identity,
        diagnostic_status,
        diagnostic_message,
    )
    .await?;
    transaction.commit().await.map_err(StorageError::Query)
}

async fn record_source_diagnostic_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    identity: &SourceIdentity,
    diagnostic_status: &'static str,
    diagnostic_message: &'static str,
) -> Result<(), StorageError> {
    let now = unix_timestamp()?;
    sqlx::query(
        "INSERT INTO sources (provider, source_format, canonical_locator, diagnostic_status, diagnostic_message, diagnostic_recorded_at)\n         VALUES (?, ?, ?, ?, ?, ?)\n         ON CONFLICT(provider, source_format, canonical_locator) DO UPDATE SET\n           diagnostic_status = excluded.diagnostic_status,\n           diagnostic_message = excluded.diagnostic_message,\n           diagnostic_recorded_at = excluded.diagnostic_recorded_at",
    )
    .bind(identity.provider)
    .bind(identity.source_format)
    .bind(&identity.canonical_locator)
    .bind(diagnostic_status)
    .bind(diagnostic_message)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(StorageError::Query)?;
    Ok(())
}

/// Marks retained snapshots whose source was absent from a completed provider
/// discovery. The source rows and their last-good sessions stay intact.
async fn mark_missing_sources_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    provider: &str,
    discovered: &BTreeSet<(String, String)>,
) -> Result<u64, StorageError> {
    let retained = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id, source_format, canonical_locator
         FROM sources
         WHERE provider = ?",
    )
    .bind(provider)
    .fetch_all(&mut **transaction)
    .await
    .map_err(StorageError::Query)?;

    let missing = retained
        .into_iter()
        .filter(|(_, source_format, locator)| {
            !discovered.contains(&(source_format.clone(), locator.clone()))
        })
        .collect::<Vec<_>>();
    let now = unix_timestamp()?;
    for (source_id, _, _) in &missing {
        sqlx::query(
            "UPDATE sources
             SET diagnostic_status = 'error',
                 diagnostic_message = ?,
                 diagnostic_recorded_at = ?
             WHERE id = ?",
        )
        .bind(MISSING_SOURCE_DIAGNOSTIC)
        .bind(now)
        .bind(source_id)
        .execute(&mut **transaction)
        .await
        .map_err(StorageError::Query)?;
    }

    u64::try_from(missing.len()).map_err(|_| StorageError::CountOutOfRange)
}

/// Lists normalized sessions across every successfully retained provider source.
///
/// # Errors
///
/// Returns an error if the catalog cannot be queried.
pub async fn list_catalog_sessions(
    pool: &SqlitePool,
    limit: u32,
) -> Result<Vec<CatalogSessionSummary>, StorageError> {
    let rows = sqlx::query(
        "SELECT sessions.id,
                sources.provider,
                sources.source_format,
                sources.canonical_locator AS source_locator,
                sessions.session_key,
                sessions.title,
                sessions.repository,
                sessions.cwd,
                sessions.model,
                sessions.execution_kind,
                sessions.started_at,
                sessions.last_visible_event_at,
                CASE
                    WHEN sources.diagnostic_status = 'error'
                     AND sources.diagnostic_message = 'source was absent from a completed provider discovery'
                    THEN 'missing'
                    ELSE sources.diagnostic_status
                END AS diagnostic_status,
                sources.last_success_at
         FROM sessions
         JOIN sources ON sources.id = sessions.source_id
         ORDER BY COALESCE(sessions.last_visible_event_at, sessions.started_at) DESC,
                  sources.provider ASC,
                  sources.source_format ASC,
                  sources.canonical_locator ASC,
                  sessions.session_key ASC
         LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await
    .map_err(StorageError::Query)?;

    rows.iter().map(catalog_session_summary).collect()
}

/// Lists diagnostics for every known source, including sources with no sessions.
///
/// # Errors
///
/// Returns an error when the catalog cannot be queried.
pub async fn list_catalog_source_diagnostics(
    pool: &SqlitePool,
) -> Result<Vec<CatalogSourceDiagnostic>, StorageError> {
    let rows = sqlx::query(
        "SELECT provider,
                source_format,
                canonical_locator AS source_locator,
                CASE
                    WHEN diagnostic_status = 'error'
                     AND diagnostic_message = 'source was absent from a completed provider discovery'
                    THEN 'missing'
                    ELSE diagnostic_status
                END AS diagnostic_status,
                diagnostic_message,
                diagnostic_recorded_at,
                last_success_at
         FROM sources
         ORDER BY provider ASC, source_format ASC, canonical_locator ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::Query)?;

    rows.iter().map(catalog_source_diagnostic).collect()
}

/// Reads one normalized catalog session without scanning any provider sources.
///
/// # Errors
///
/// Returns an error if the session is absent or the catalog cannot be queried.
pub async fn read_catalog_preview(
    pool: &SqlitePool,
    session_id: i64,
    item_limit: u32,
    snippet_bytes: usize,
) -> Result<CatalogSessionPreview, StorageError> {
    let row = sqlx::query(
        "SELECT sessions.id,
                sources.provider,
                sources.source_format,
                sources.canonical_locator AS source_locator,
                sessions.session_key,
                sessions.title,
                sessions.repository,
                sessions.cwd,
                sessions.model,
                sessions.execution_kind,
                sessions.started_at,
                sessions.last_visible_event_at,
                CASE
                    WHEN sources.diagnostic_status = 'error'
                     AND sources.diagnostic_message = 'source was absent from a completed provider discovery'
                    THEN 'missing'
                    ELSE sources.diagnostic_status
                END AS diagnostic_status,
                sources.last_success_at
         FROM sessions
         JOIN sources ON sources.id = sessions.source_id
         WHERE sessions.id = ?",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(StorageError::Query)?
    .ok_or(StorageError::CatalogSessionNotFound { session_id })?;
    read_catalog_preview_from_row(pool, row, item_limit, snippet_bytes).await
}

/// Reads a normalized catalog session by its stable provider/source/session key.
///
/// This is intended for interactive views which can retain a summary while a
/// source refresh atomically replaces the numeric session row ID.
///
/// # Errors
///
/// Returns an error if the session is absent or the catalog cannot be queried.
pub async fn read_catalog_preview_by_identity(
    pool: &SqlitePool,
    session: &CatalogSessionSummary,
    item_limit: u32,
    snippet_bytes: usize,
) -> Result<CatalogSessionPreview, StorageError> {
    let row = sqlx::query(
        "SELECT sessions.id,
                sources.provider,
                sources.source_format,
                sources.canonical_locator AS source_locator,
                sessions.session_key,
                sessions.title,
                sessions.repository,
                sessions.cwd,
                sessions.model,
                sessions.execution_kind,
                sessions.started_at,
                sessions.last_visible_event_at,
                CASE
                    WHEN sources.diagnostic_status = 'error'
                     AND sources.diagnostic_message = 'source was absent from a completed provider discovery'
                    THEN 'missing'
                    ELSE sources.diagnostic_status
                END AS diagnostic_status,
                sources.last_success_at
         FROM sessions
         JOIN sources ON sources.id = sessions.source_id
         WHERE sources.provider = ?
           AND sources.source_format = ?
           AND sources.canonical_locator = ?
           AND sessions.session_key = ?",
    )
    .bind(&session.provider)
    .bind(&session.source_format)
    .bind(&session.source_locator)
    .bind(&session.session_key)
    .fetch_optional(pool)
    .await
    .map_err(StorageError::Query)?
    .ok_or(StorageError::CatalogSessionNotFound {
        session_id: session.id,
    })?;

    read_catalog_preview_from_row(pool, row, item_limit, snippet_bytes).await
}

async fn read_catalog_preview_from_row(
    pool: &SqlitePool,
    row: sqlx::sqlite::SqliteRow,
    item_limit: u32,
    snippet_bytes: usize,
) -> Result<CatalogSessionPreview, StorageError> {
    let session = catalog_session_summary(&row)?;

    let rows = sqlx::query(
        "SELECT item_kind,
                content,
                tool_name,
                tool_status
         FROM transcript_items
         WHERE session_id = ?
         ORDER BY ordinal ASC
         LIMIT ?",
    )
    .bind(session.id)
    .bind(i64::from(item_limit.saturating_add(1)))
    .fetch_all(pool)
    .await
    .map_err(StorageError::Query)?;
    let items_truncated = rows.len() > usize::try_from(item_limit).unwrap_or(usize::MAX);
    let items = rows
        .iter()
        .take(usize::try_from(item_limit).unwrap_or(usize::MAX))
        .map(catalog_item_view)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|item| truncate_item_view(item, snippet_bytes))
        .collect();

    Ok(CatalogSessionPreview {
        session,
        items,
        items_truncated,
    })
}

fn catalog_session_summary(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<CatalogSessionSummary, StorageError> {
    Ok(CatalogSessionSummary {
        id: row.try_get("id").map_err(StorageError::Query)?,
        provider: row.try_get("provider").map_err(StorageError::Query)?,
        source_format: row.try_get("source_format").map_err(StorageError::Query)?,
        source_locator: row.try_get("source_locator").map_err(StorageError::Query)?,
        session_key: row.try_get("session_key").map_err(StorageError::Query)?,
        title: row.try_get("title").map_err(StorageError::Query)?,
        repository: row.try_get("repository").map_err(StorageError::Query)?,
        cwd: row.try_get("cwd").map_err(StorageError::Query)?,
        model: row.try_get("model").map_err(StorageError::Query)?,
        execution_kind: row.try_get("execution_kind").map_err(StorageError::Query)?,
        started_at: row.try_get("started_at").map_err(StorageError::Query)?,
        last_visible_event_at: row
            .try_get("last_visible_event_at")
            .map_err(StorageError::Query)?,
        source_diagnostic_status: row
            .try_get("diagnostic_status")
            .map_err(StorageError::Query)?,
        source_last_success_at: row
            .try_get("last_success_at")
            .map_err(StorageError::Query)?,
    })
}

fn catalog_source_diagnostic(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<CatalogSourceDiagnostic, StorageError> {
    Ok(CatalogSourceDiagnostic {
        provider: row.try_get("provider").map_err(StorageError::Query)?,
        source_format: row.try_get("source_format").map_err(StorageError::Query)?,
        source_locator: row.try_get("source_locator").map_err(StorageError::Query)?,
        diagnostic_status: row
            .try_get("diagnostic_status")
            .map_err(StorageError::Query)?,
        diagnostic_message: row
            .try_get("diagnostic_message")
            .map_err(StorageError::Query)?,
        diagnostic_recorded_at: row
            .try_get("diagnostic_recorded_at")
            .map_err(StorageError::Query)?,
        last_success_at: row
            .try_get("last_success_at")
            .map_err(StorageError::Query)?,
    })
}

fn truncate_item_view(item: CatalogItemView, snippet_bytes: usize) -> CatalogItemView {
    match item {
        CatalogItemView::UserText { content } => CatalogItemView::UserText {
            content: truncate_display_text(content, snippet_bytes),
        },
        CatalogItemView::AssistantText { content } => CatalogItemView::AssistantText {
            content: truncate_display_text(content, snippet_bytes),
        },
        marker @ CatalogItemView::ToolMarker { .. } => marker,
    }
}

fn truncate_display_text(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    if max_bytes == 0 {
        return String::new();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn catalog_item_view(row: &sqlx::sqlite::SqliteRow) -> Result<CatalogItemView, StorageError> {
    let item_kind = row
        .try_get::<String, _>("item_kind")
        .map_err(StorageError::Query)?;
    match item_kind.as_str() {
        "user_text" => Ok(CatalogItemView::UserText {
            content: row.try_get("content").map_err(StorageError::Query)?,
        }),
        "assistant_text" => Ok(CatalogItemView::AssistantText {
            content: row.try_get("content").map_err(StorageError::Query)?,
        }),
        "tool_marker" => Ok(CatalogItemView::ToolMarker {
            name: row.try_get("tool_name").map_err(StorageError::Query)?,
            status: row.try_get("tool_status").map_err(StorageError::Query)?,
        }),
        _ => Err(StorageError::InvalidCatalogItemKind { item_kind }),
    }
}

fn unix_timestamp() -> Result<i64, StorageError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(StorageError::Clock)?;
    i64::try_from(duration.as_secs()).map_err(|_| StorageError::ClockOutOfRange)
}

async fn scalar_i64(pool: &SqlitePool, query: &str) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(query)
        .fetch_one(pool)
        .await
        .map_err(StorageError::Query)
}

async fn database_has_no_user_objects(pool: &SqlitePool) -> Result<bool, StorageError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)\n         FROM sqlite_master\n         WHERE type IN ('table', 'index', 'trigger', 'view')\n           AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .map_err(StorageError::Query)?;

    Ok(count == 0)
}

async fn create_v1_schema(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut transaction = pool.begin().await.map_err(StorageError::Query)?;

    sqlx::query(CREATE_SOURCES)
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;

    sqlx::query(CREATE_SESSIONS)
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;

    sqlx::query(CREATE_TRANSCRIPT_ITEMS)
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;

    sqlx::query(CREATE_SESSIONS_RECENT_INDEX)
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;
    sqlx::query("PRAGMA user_version = 1")
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Query)?;

    transaction.commit().await.map_err(StorageError::Query)
}

/// Failures while opening or validating Agentlog's `SQLite` catalog.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("cannot open Agentlog SQLite database: {0}")]
    Connect(sqlx::Error),
    #[error("SQLite operation failed: {0}")]
    Query(sqlx::Error),
    #[error("database has tables but no Agentlog schema version; refusing to modify it")]
    UnversionedDatabase,
    #[error("unsupported Agentlog schema version {found}; refusing to modify it")]
    UnsupportedSchemaVersion { found: i64 },
    #[error("database does not match the complete Agentlog schema v1 fingerprint")]
    SchemaFingerprintMismatch,
    #[error("cannot inspect Agentlog catalog path {path}: {source}")]
    CatalogPathIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to purge; Agentlog catalog path {path} {reason}")]
    UnsafeCatalogPath { path: PathBuf, reason: &'static str },
    #[error("Agentlog catalog file identity changed while opening {path}; refusing to purge")]
    CatalogFileIdentityChanged { path: PathBuf },
    #[error(
        "Agentlog catalog changed after the purge preview at {path}; no data was deleted; rerun `agentlog purge` to review the updated catalog"
    )]
    CatalogPreviewChanged { path: PathBuf },
    #[error("Agentlog catalog file-set size exceeds the supported range at {path}")]
    CatalogFileSetSizeOverflow { path: PathBuf },
    #[error(
        "refusing to purge; Agentlog SQLite file set at {path} is {bytes} bytes, exceeding the {max_bytes} byte limit"
    )]
    CatalogFileSetTooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
    #[error("catalog transcript ordinal exceeds SQLite integer range")]
    OrdinalOverflow,
    #[error("system clock is before the Unix epoch")]
    Clock(std::time::SystemTimeError),
    #[error("system clock is outside the catalog timestamp range")]
    ClockOutOfRange,
    #[error("catalog session {session_id} was not found")]
    CatalogSessionNotFound { session_id: i64 },
    #[error("catalog contains an invalid transcript item kind {item_kind}")]
    InvalidCatalogItemKind { item_kind: String },
    #[error("catalog count cannot be represented by the public result type")]
    CountOutOfRange,
    #[error("provider scanner identity mismatch: expected {expected}, found {found}")]
    ProviderIdentityMismatch {
        expected: &'static str,
        found: &'static str,
    },
    #[error("cannot inspect a provider-owned source: {0}")]
    SourceIo(std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use sqlx::{
        Row, SqlitePool,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    use tempfile::TempDir;

    use crate::providers::{
        ProviderId, ProviderScan, ProviderScanError, ProviderScanFuture, ProviderScanner,
        SourceOutcome,
    };

    use super::{
        Catalog, CatalogContentToken, CatalogItem, CatalogScanError, CatalogSession,
        SCHEMA_VERSION, SourceIdentity, SourceSnapshot, StorageError, inspect_catalog_file_state,
        list_catalog_sessions, list_catalog_source_diagnostics, open_database,
        purge_existing_catalog_with_hooks, read_catalog_preview, read_catalog_preview_by_identity,
        record_source_failure, replace_source_snapshot, sqlite_sidecar_path,
        verify_catalog_content,
    };

    fn temporary_directory() -> TempDir {
        let platform_temp = std::env::temp_dir()
            .canonicalize()
            .expect("resolve platform temporary directory");
        TempDir::new_in(platform_temp).expect("temporary directory")
    }

    struct MismatchedScanner;

    impl ProviderScanner for MismatchedScanner {
        fn provider_id(&self) -> ProviderId {
            ProviderId::Codex
        }

        fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
            Ok(Box::new(MismatchedScan {
                outcome: Some(SourceOutcome::Accepted(SourceSnapshot {
                    identity: SourceIdentity {
                        provider: "claude",
                        source_format: "test",
                        canonical_locator: "test://source".to_owned(),
                    },
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: Vec::new(),
                })),
            }))
        }
    }

    struct MismatchedScan {
        outcome: Option<SourceOutcome>,
    }

    impl ProviderScan for MismatchedScan {
        fn candidate_sources(&self) -> u64 {
            1
        }

        fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
            let outcome = self.outcome.take();
            Box::pin(async move { Ok(outcome) })
        }
    }

    async fn user_tables(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT name\n             FROM sqlite_master\n             WHERE type = 'table'\n               AND name NOT LIKE 'sqlite_%'\n             ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .expect("list tables")
    }

    async fn open_temporary_database(temporary: &TempDir, name: &str) -> SqlitePool {
        open_database(&temporary.path().join(name))
            .await
            .expect("open database")
    }

    async fn create_foreign_database(path: &Path, user_version: i64, table: bool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .expect("open foreign database");
        if table {
            sqlx::query("CREATE TABLE foreign_data (id INTEGER PRIMARY KEY)")
                .execute(&pool)
                .await
                .expect("create foreign table");
        }
        sqlx::query(&format!("PRAGMA user_version = {user_version}"))
            .execute(&pool)
            .await
            .expect("set foreign version");
        pool.close().await;
    }

    #[derive(Debug, Eq, PartialEq)]
    struct DatabaseFiles {
        database: Vec<u8>,
        wal: Option<Vec<u8>>,
        shm: Option<Vec<u8>>,
    }

    fn database_files(path: &Path) -> DatabaseFiles {
        fn read_optional(path: PathBuf) -> Option<Vec<u8>> {
            match fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("read database sidecar: {error}"),
            }
        }

        DatabaseFiles {
            database: fs::read(path).expect("read database"),
            wal: read_optional(PathBuf::from(format!("{}-wal", path.display()))),
            shm: read_optional(PathBuf::from(format!("{}-shm", path.display()))),
        }
    }

    #[test]
    fn catalog_content_token_matches_unchanged_main_and_rejects_mismatch() {
        let temporary = temporary_directory();
        let database = temporary.path().join("agentlog.sqlite3");
        fs::write(&database, b"previewed-main").expect("write previewed main");
        let token = CatalogContentToken::new(b"previewed-main", None);

        verify_catalog_content(&database, &token, 1024).expect("match unchanged main");
        fs::write(&database, b"different-main").expect("replace main contents");
        assert!(matches!(
            verify_catalog_content(&database, &token, 1024),
            Err(StorageError::CatalogPreviewChanged { .. })
        ));
    }

    #[test]
    fn catalog_content_token_normalizes_empty_wal_and_rejects_wal_changes() {
        let temporary = temporary_directory();
        let database = temporary.path().join("agentlog.sqlite3");
        let wal = sqlite_sidecar_path(&database, "-wal");
        fs::write(&database, b"main").expect("write main");

        let no_wal = CatalogContentToken::new(b"main", None);
        fs::write(&wal, b"").expect("create empty WAL");
        verify_catalog_content(&database, &no_wal, 1024)
            .expect("absent and empty WAL are equivalent");
        fs::write(&wal, b"appeared").expect("write appearing WAL");
        assert!(matches!(
            verify_catalog_content(&database, &no_wal, 1024),
            Err(StorageError::CatalogPreviewChanged { .. })
        ));

        let with_wal = CatalogContentToken::new(b"main", Some(b"previewed-wal"));
        fs::write(&wal, b"previewed-wal").expect("write previewed WAL");
        verify_catalog_content(&database, &with_wal, 1024).expect("match unchanged WAL");
        fs::write(&wal, b"different-wal").expect("change WAL contents");
        assert!(matches!(
            verify_catalog_content(&database, &with_wal, 1024),
            Err(StorageError::CatalogPreviewChanged { .. })
        ));
        fs::remove_file(&wal).expect("remove WAL");
        assert!(matches!(
            verify_catalog_content(&database, &with_wal, 1024),
            Err(StorageError::CatalogPreviewChanged { .. })
        ));
    }

    #[tokio::test]
    async fn fresh_database_creates_only_v1_catalog_tables() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "agentlog.sqlite3").await;

        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&database)
            .await
            .expect("schema version");
        let synchronous = sqlx::query_scalar::<_, i64>("PRAGMA synchronous")
            .fetch_one(&database)
            .await
            .expect("synchronous mode");
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(synchronous, 1, "derived catalog uses WAL/NORMAL");
        assert_eq!(
            user_tables(&database).await,
            vec!["sessions", "sources", "transcript_items",]
        );
        database.close().await;
    }

    #[tokio::test]
    async fn purge_rejects_a_database_identity_replaced_after_sqlite_open() {
        let temporary = temporary_directory();
        let database_path = temporary.path().join("agentlog.sqlite3");
        let original = open_database(&database_path)
            .await
            .expect("open original database");
        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)
             VALUES ('test', 'test', 'test://original')",
        )
        .execute(&original)
        .await
        .expect("insert original row");
        original.close().await;
        let expected_identity = inspect_catalog_file_state(&database_path)
            .expect("inspect original database")
            .expect("original database exists")
            .identity;

        let replacement_path = temporary.path().join("replacement.sqlite3");
        let replacement = open_database(&replacement_path)
            .await
            .expect("open replacement database");
        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)
             VALUES ('test', 'test', 'test://replacement')",
        )
        .execute(&replacement)
        .await
        .expect("insert replacement row");
        replacement.close().await;
        let displaced_path = temporary.path().join("displaced.sqlite3");

        let error = purge_existing_catalog_with_hooks(
            &database_path,
            expected_identity,
            None,
            u64::MAX,
            || {
                fs::rename(&database_path, &displaced_path).expect("displace opened database");
                fs::rename(&replacement_path, &database_path).expect("replace database path");
            },
            |_| {},
        )
        .await
        .expect_err("reject replaced database identity");

        assert!(matches!(
            error,
            StorageError::CatalogFileIdentityChanged { .. }
        ));
        let replacement = open_database(&database_path)
            .await
            .expect("reopen replacement database");
        let retained = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sources")
            .fetch_one(&replacement)
            .await
            .expect("count retained replacement rows");
        assert_eq!(retained, 1);
        replacement.close().await;
    }

    #[tokio::test]
    async fn purge_enforces_the_file_set_bound_while_holding_the_writer_lock() {
        let temporary = temporary_directory();
        let database_path = temporary.path().join("agentlog.sqlite3");
        let database = open_database(&database_path).await.expect("open catalog");
        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)
             VALUES ('test', 'test', 'test://retained')",
        )
        .execute(&database)
        .await
        .expect("insert retained source");
        database.close().await;
        let expected_identity = inspect_catalog_file_state(&database_path)
            .expect("inspect catalog")
            .expect("catalog exists")
            .identity;

        let error = purge_existing_catalog_with_hooks(
            &database_path,
            expected_identity,
            None,
            0,
            || {},
            |_| panic!("oversized file set must fail before the measurement hook"),
        )
        .await
        .expect_err("reject the locked file set above its limit");
        assert!(matches!(
            error,
            StorageError::CatalogFileSetTooLarge {
                bytes,
                max_bytes: 0,
                ..
            } if bytes > 0
        ));

        let database = open_database(&database_path)
            .await
            .expect("reopen retained catalog");
        let retained = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sources")
            .fetch_one(&database)
            .await
            .expect("count retained rows");
        assert_eq!(retained, 1);
        database.close().await;
    }

    #[tokio::test]
    async fn catalog_rejects_scanner_identity_mismatch_without_persisting_it() {
        let temporary = temporary_directory();
        let pool = open_temporary_database(&temporary, "identity.sqlite3").await;
        let catalog = Catalog { pool };

        let error = catalog
            .scan(&MismatchedScanner)
            .await
            .expect_err("identity mismatch");
        assert!(matches!(
            error,
            CatalogScanError::Storage(StorageError::ProviderIdentityMismatch {
                expected: "codex",
                found: "claude",
            })
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sources")
                .fetch_one(&catalog.pool)
                .await
                .expect("source count"),
            0
        );
        catalog.close().await;
    }

    #[tokio::test]
    async fn unknown_version_is_not_changed() {
        let temporary = temporary_directory();
        let path = temporary.path().join("future.sqlite3");
        create_foreign_database(&path, 99, false).await;
        let before = database_files(&path);

        let error = open_database(&path)
            .await
            .expect_err("future version must fail");

        assert!(matches!(
            error,
            StorageError::UnsupportedSchemaVersion { found: 99 }
        ));
        assert_eq!(database_files(&path), before);
    }

    #[tokio::test]
    async fn unversioned_nonempty_database_is_not_adopted() {
        let temporary = temporary_directory();
        let path = temporary.path().join("foreign.sqlite3");
        create_foreign_database(&path, 0, true).await;
        let before = database_files(&path);

        let error = open_database(Path::new(&path))
            .await
            .expect_err("unversioned database must fail");

        assert!(matches!(error, StorageError::UnversionedDatabase));
        assert_eq!(database_files(&path), before);
    }

    #[tokio::test]
    async fn schema_enforces_source_identity_and_cascades_source_snapshots() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "catalog.sqlite3").await;

        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)\n             VALUES ('codex', 'session_jsonl', '/private/tmp/session.jsonl')",
        )
        .execute(&database)
        .await
        .expect("insert source");
        let duplicate = sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)\n             VALUES ('codex', 'session_jsonl', '/private/tmp/session.jsonl')",
        )
        .execute(&database)
        .await;
        assert!(duplicate.is_err());

        sqlx::query(
            "INSERT INTO sessions (source_id, session_key, title)\n             VALUES (1, 'native-session', 'A session')",
        )
        .execute(&database)
        .await
        .expect("insert session");
        sqlx::query(
            "INSERT INTO transcript_items (session_id, ordinal, item_kind, content)\n             VALUES (1, 0, 'user_text', 'visible request')",
        )
        .execute(&database)
        .await
        .expect("insert visible item");

        sqlx::query("DELETE FROM sources WHERE id = 1")
            .execute(&database)
            .await
            .expect("delete source snapshot");
        let sessions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&database)
            .await
            .expect("count sessions");
        let transcript_items =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM transcript_items")
                .fetch_one(&database)
                .await
                .expect("count transcript items");

        assert_eq!(sessions, 0);
        assert_eq!(transcript_items, 0);
        database.close().await;
    }

    #[tokio::test]
    async fn schema_rejects_invalid_visible_transcript_shapes() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "transcript.sqlite3").await;
        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator)\n             VALUES ('claude', 'project_jsonl', '/private/tmp/project.jsonl')",
        )
        .execute(&database)
        .await
        .expect("insert source");
        sqlx::query("INSERT INTO sessions (source_id, session_key) VALUES (1, 'native-session')")
            .execute(&database)
            .await
            .expect("insert session");

        let missing_text = sqlx::query(
            "INSERT INTO transcript_items (session_id, ordinal, item_kind)\n             VALUES (1, 0, 'user_text')",
        )
        .execute(&database)
        .await;
        let tool_with_text = sqlx::query(
            "INSERT INTO transcript_items (session_id, ordinal, item_kind, content, tool_name)\n             VALUES (1, 1, 'tool_marker', 'tool output', 'shell')",
        )
        .execute(&database)
        .await;

        assert!(missing_text.is_err());
        assert!(tool_with_text.is_err());
        database.close().await;
    }

    #[tokio::test]
    async fn shared_catalog_list_and_preview_expose_provider_and_stale_source_status() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "catalog.sqlite3").await;
        for (provider, locator, session_key, text) in [
            (
                "codex",
                "/private/tmp/codex.jsonl",
                "codex-session",
                "Codex visible text",
            ),
            (
                "claude",
                "/private/tmp/claude.jsonl",
                "claude-session",
                "Claude visible text",
            ),
        ] {
            replace_source_snapshot(
                &database,
                &SourceSnapshot {
                    identity: SourceIdentity {
                        provider,
                        source_format: "session_jsonl",
                        canonical_locator: locator.to_owned(),
                    },
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: vec![CatalogSession {
                        session_key: session_key.to_owned(),
                        title: Some(text.to_owned()),
                        repository: None,
                        cwd: None,
                        model: None,
                        execution_kind: None,
                        started_at: Some(1),
                        last_visible_event_at: Some(1),
                        items: vec![CatalogItem::UserText(text.to_owned())],
                    }],
                },
            )
            .await
            .expect("store snapshot");
        }
        let claude_identity = SourceIdentity {
            provider: "claude",
            source_format: "session_jsonl",
            canonical_locator: "/private/tmp/claude.jsonl".to_owned(),
        };
        record_source_failure(&database, &claude_identity, "source became malformed")
            .await
            .expect("record stale failure");

        let sessions = list_catalog_sessions(&database, 50)
            .await
            .expect("list shared catalog");
        assert_eq!(sessions.len(), 2);
        let list_json = serde_json::to_value(&sessions).expect("serialize catalog list");
        assert!(
            list_json
                .as_array()
                .and_then(|sessions| sessions.first())
                .and_then(|session| session.get("source_locator"))
                .is_none(),
            "list JSON must not expose canonical source paths"
        );
        assert!(
            list_json
                .as_array()
                .and_then(|sessions| sessions.first())
                .and_then(|session| session.get("session_key"))
                .is_some(),
            "list JSON retains the stable session key"
        );
        let claude = sessions
            .iter()
            .find(|session| session.provider == "claude")
            .expect("Claude session");
        assert_eq!(claude.source_diagnostic_status, "error");
        assert!(claude.source_last_success_at.is_some());
        let preview = read_catalog_preview(&database, claude.id, 1, 5)
            .await
            .expect("read bounded preview");
        assert_eq!(preview.session.provider, "claude");
        assert_eq!(preview.session.source_diagnostic_status, "error");
        assert!(
            matches!(preview.items.as_slice(), [super::CatalogItemView::UserText { content }] if content == "Claud…")
        );
        assert!(!preview.items_truncated);
        database.close().await;
    }

    #[tokio::test]
    async fn stable_preview_survives_a_replaced_session_row_during_refresh() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "catalog.sqlite3").await;
        let refreshed = SourceIdentity {
            provider: "codex",
            source_format: "session_jsonl",
            canonical_locator: "/private/tmp/refreshed.jsonl".to_owned(),
        };
        let unchanged = SourceIdentity {
            provider: "claude",
            source_format: "session_jsonl",
            canonical_locator: "/private/tmp/unchanged.jsonl".to_owned(),
        };
        for (identity, session_key, text) in [
            (&refreshed, "refresh-key", "before replacement"),
            (
                &unchanged,
                "other-key",
                "keeps the old row ID from being reused",
            ),
        ] {
            replace_source_snapshot(
                &database,
                &SourceSnapshot {
                    identity: identity.clone(),
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: vec![CatalogSession {
                        session_key: session_key.to_owned(),
                        title: Some(text.to_owned()),
                        repository: None,
                        cwd: None,
                        model: None,
                        execution_kind: None,
                        started_at: Some(1),
                        last_visible_event_at: Some(1),
                        items: vec![CatalogItem::UserText(text.to_owned())],
                    }],
                },
            )
            .await
            .expect("store initial source snapshot");
        }
        let stale = list_catalog_sessions(&database, 10)
            .await
            .expect("list initial catalog")
            .into_iter()
            .find(|session| session.provider == "codex")
            .expect("initial refreshed session");

        replace_source_snapshot(
            &database,
            &SourceSnapshot {
                identity: refreshed,
                diagnostic_status: "ok",
                diagnostic_message: None,
                sessions: vec![CatalogSession {
                    session_key: "refresh-key".to_owned(),
                    title: Some("after replacement".to_owned()),
                    repository: None,
                    cwd: None,
                    model: None,
                    execution_kind: None,
                    started_at: Some(2),
                    last_visible_event_at: Some(2),
                    items: vec![CatalogItem::UserText("after replacement".to_owned())],
                }],
            },
        )
        .await
        .expect("replace refreshed source snapshot");

        let current = list_catalog_sessions(&database, 10)
            .await
            .expect("list replaced catalog")
            .into_iter()
            .find(|session| session.provider == "codex")
            .expect("replaced session");
        assert_ne!(current.id, stale.id, "refresh must replace the numeric row");
        assert!(matches!(
            read_catalog_preview(&database, stale.id, 80, 4 * 1024).await,
            Err(StorageError::CatalogSessionNotFound { .. })
        ));

        let preview = read_catalog_preview_by_identity(&database, &stale, 80, 4 * 1024)
            .await
            .expect("resolve a current preview from the stale visible row");
        assert_eq!(preview.session.id, current.id);
        assert_eq!(preview.session.session_key, "refresh-key");
        assert!(matches!(
            preview.items.as_slice(),
            [super::CatalogItemView::UserText { content }] if content == "after replacement"
        ));
        database.close().await;
    }

    #[tokio::test]
    async fn catalog_list_uses_stable_source_and_session_tie_breakers() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "ordered.sqlite3").await;
        for (provider, locator, session_keys) in [
            ("cursor", "/private/tmp/z.jsonl", &["b"][..]),
            ("claude", "/private/tmp/b.jsonl", &["z"][..]),
            ("claude", "/private/tmp/a.jsonl", &["z", "a"][..]),
        ] {
            replace_source_snapshot(
                &database,
                &SourceSnapshot {
                    identity: SourceIdentity {
                        provider,
                        source_format: "session_jsonl",
                        canonical_locator: locator.to_owned(),
                    },
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: session_keys
                        .iter()
                        .map(|session_key| CatalogSession {
                            session_key: (*session_key).to_owned(),
                            title: None,
                            repository: None,
                            cwd: None,
                            model: None,
                            execution_kind: None,
                            started_at: Some(1),
                            last_visible_event_at: Some(1),
                            items: Vec::new(),
                        })
                        .collect(),
                },
            )
            .await
            .expect("store ordered snapshot");
        }

        let sessions = list_catalog_sessions(&database, 50)
            .await
            .expect("list sessions");
        let identities = sessions
            .iter()
            .map(|session| {
                (
                    session.provider.as_str(),
                    session.source_locator.as_str(),
                    session.session_key.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            vec![
                ("claude", "/private/tmp/a.jsonl", "a"),
                ("claude", "/private/tmp/a.jsonl", "z"),
                ("claude", "/private/tmp/b.jsonl", "z"),
                ("cursor", "/private/tmp/z.jsonl", "b"),
            ]
        );
        database.close().await;
    }

    #[tokio::test]
    async fn catalog_list_orders_by_visible_activity_then_stable_identity() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "activity.sqlite3").await;
        replace_source_snapshot(
            &database,
            &SourceSnapshot {
                identity: SourceIdentity {
                    provider: "codex",
                    source_format: "session_jsonl",
                    canonical_locator: "/private/tmp/activity.jsonl".to_owned(),
                },
                diagnostic_status: "ok",
                diagnostic_message: None,
                sessions: vec![
                    CatalogSession {
                        session_key: "started-only".to_owned(),
                        title: None,
                        repository: None,
                        cwd: None,
                        model: None,
                        execution_kind: None,
                        started_at: Some(100),
                        last_visible_event_at: None,
                        items: Vec::new(),
                    },
                    CatalogSession {
                        session_key: "older-visible".to_owned(),
                        title: None,
                        repository: None,
                        cwd: None,
                        model: None,
                        execution_kind: None,
                        started_at: Some(1_000),
                        last_visible_event_at: Some(99),
                        items: Vec::new(),
                    },
                ],
            },
        )
        .await
        .expect("store activity snapshot");

        let sessions = list_catalog_sessions(&database, 10)
            .await
            .expect("list sessions");
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_key.as_str())
                .collect::<Vec<_>>(),
            vec!["started-only", "older-visible"]
        );
        database.close().await;
    }

    #[tokio::test]
    async fn source_diagnostics_include_zero_session_failures_and_hide_locators_from_json() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "diagnostics.sqlite3").await;
        let failed = SourceIdentity {
            provider: "alpha",
            source_format: "session_jsonl",
            canonical_locator: "/private/tmp/failed.jsonl".to_owned(),
        };
        record_source_failure(&database, &failed, "malformed source")
            .await
            .expect("record zero-session failure");
        replace_source_snapshot(
            &database,
            &SourceSnapshot {
                identity: SourceIdentity {
                    provider: "beta",
                    source_format: "session_jsonl",
                    canonical_locator: "/private/tmp/accepted.jsonl".to_owned(),
                },
                diagnostic_status: "ok",
                diagnostic_message: None,
                sessions: Vec::new(),
            },
        )
        .await
        .expect("store empty accepted source");

        let diagnostics = list_catalog_source_diagnostics(&database)
            .await
            .expect("list diagnostics");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].provider, "alpha");
        assert_eq!(diagnostics[0].source_locator, failed.canonical_locator);
        assert_eq!(diagnostics[0].diagnostic_status, "error");
        assert_eq!(
            diagnostics[0].diagnostic_message.as_deref(),
            Some("malformed source")
        );
        assert!(diagnostics[0].diagnostic_recorded_at.is_some());
        assert_eq!(diagnostics[1].provider, "beta");
        assert_eq!(diagnostics[1].diagnostic_status, "ok");
        assert!(diagnostics[1].last_success_at.is_some());
        let json = serde_json::to_value(&diagnostics).expect("serialize diagnostics");
        assert!(json[0].get("source_locator").is_none());
        assert!(!json.to_string().contains("/private/tmp/failed.jsonl"));
        database.close().await;
    }

    #[tokio::test]
    async fn transcript_batch_insert_preserves_all_item_shapes_across_chunk_boundary() {
        let temporary = temporary_directory();
        let database = open_temporary_database(&temporary, "catalog.sqlite3").await;
        let items = (0..501)
            .map(|index| match index % 3 {
                0 => CatalogItem::UserText(format!("user {index}")),
                1 => CatalogItem::AssistantText(format!("assistant {index}")),
                _ => CatalogItem::ToolMarker {
                    name: format!("tool_{index}"),
                    status: Some("requested".to_owned()),
                },
            })
            .collect::<Vec<_>>();
        replace_source_snapshot(
            &database,
            &SourceSnapshot {
                identity: SourceIdentity {
                    provider: "claude",
                    source_format: "project_jsonl",
                    canonical_locator: "/private/tmp/chunk.jsonl".to_owned(),
                },
                diagnostic_status: "ok",
                diagnostic_message: None,
                sessions: vec![CatalogSession {
                    session_key: "chunked".to_owned(),
                    title: None,
                    repository: None,
                    cwd: None,
                    model: None,
                    execution_kind: None,
                    started_at: None,
                    last_visible_event_at: None,
                    items,
                }],
            },
        )
        .await
        .expect("store chunked snapshot");

        let rows = sqlx::query(
            "SELECT ordinal, item_kind, content, tool_name, tool_status
             FROM transcript_items
             ORDER BY ordinal ASC",
        )
        .fetch_all(&database)
        .await
        .expect("read inserted items");
        assert_eq!(rows.len(), 501);
        for (expected_ordinal, row) in rows.iter().enumerate() {
            assert_eq!(
                row.try_get::<i64, _>("ordinal").expect("ordinal"),
                i64::try_from(expected_ordinal).expect("ordinal range")
            );
            assert_batch_item_shape(row, expected_ordinal % 3);
        }
        database.close().await;
    }

    fn assert_batch_item_shape(row: &sqlx::sqlite::SqliteRow, item_type: usize) {
        let kind = row.try_get::<String, _>("item_kind").expect("kind");
        let content = row
            .try_get::<Option<String>, _>("content")
            .expect("content");
        let tool_name = row
            .try_get::<Option<String>, _>("tool_name")
            .expect("tool name");
        let tool_status = row
            .try_get::<Option<String>, _>("tool_status")
            .expect("tool status");
        match item_type {
            0 => {
                assert_eq!(kind, "user_text");
                assert!(content.is_some());
                assert!(tool_name.is_none());
                assert!(tool_status.is_none());
            }
            1 => {
                assert_eq!(kind, "assistant_text");
                assert!(content.is_some());
                assert!(tool_name.is_none());
                assert!(tool_status.is_none());
            }
            _ => {
                assert_eq!(kind, "tool_marker");
                assert!(content.is_none());
                assert!(tool_name.is_some());
                assert_eq!(tool_status, Some("requested".to_owned()));
            }
        }
    }
}
