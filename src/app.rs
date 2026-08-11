//! Application workflows for Agentlog's local catalog and six read-only collectors.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    future::Future,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Serialize;

use crate::{
    paths::AppPaths,
    providers::{ProviderScanReport, ProviderScanner, installed, source_descriptions},
    storage::{
        Catalog, CatalogContentToken, CatalogFileIdentity, CatalogFileState, CatalogScanError,
        CatalogScanProgress, CatalogSessionPreview, CatalogSessionSummary, CatalogSourceDiagnostic,
        SCHEMA_VERSION, StorageError, inspect_catalog_file_state, inspect_catalog_snapshot,
        purge_existing_catalog,
    },
};

const PURGE_SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Observable result of one read-only provider synchronization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SyncSummary {
    pub schema_version: i64,
    pub collectors_installed: u8,
    pub providers_failed: u8,
    pub sources_refreshed: u64,
    pub sources_partial: u64,
    pub sources_failed: u64,
    pub sources_missing: u64,
    pub sessions_available: u64,
    pub provider_summaries: Vec<ProviderSyncSummary>,
    pub message: String,
}

/// Aggregate diagnostic counts for one explicit provider collector.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderSyncSummary {
    pub provider: String,
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
    pub missing_sources: u64,
    pub sessions_available: u64,
    pub failure: Option<String>,
}

#[derive(Debug)]
struct ProviderSyncResult {
    provider: String,
    report: ProviderScanReport,
    failure: Option<String>,
}

/// Bounded synchronization progress that is safe to display to users.
///
/// Progress intentionally exposes provider names and aggregate counts only;
/// provider-owned source paths and transcript content remain private.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncProgress {
    /// Provider discovery is in progress and its candidate count is not known.
    ProviderDiscovering { provider: String },
    /// A provider's source discovery has completed.
    ProviderCandidates {
        provider: String,
        candidate_sources: u64,
    },
    /// A source outcome has been staged in the provider transaction.
    SourceStaged {
        provider: String,
        processed_sources: u64,
        candidate_sources: u64,
    },
    /// A provider transaction has committed, or provider setup failed.
    ProviderCompleted {
        provider: String,
        report: ProviderScanReport,
        failure: Option<String>,
    },
}

/// Initializes the durable catalog and collects read-only provider sources.
///
/// # Errors
///
/// Returns an error when Agentlog-owned storage cannot be created, opened, or
/// queried.
pub async fn sync_shell(paths: &AppPaths) -> anyhow::Result<SyncSummary> {
    sync_shell_with_progress(paths, |_| {}).await
}

/// Synchronizes provider history while exposing bounded aggregate progress.
///
/// The callback observes source staging only after the catalog write succeeds.
/// Provider completion is emitted only after the provider transaction commits.
///
/// # Errors
///
/// Returns an error when Agentlog-owned storage cannot be created, opened, or
/// queried.
pub async fn sync_shell_with_progress<F>(
    paths: &AppPaths,
    mut on_progress: F,
) -> anyhow::Result<SyncSummary>
where
    F: FnMut(SyncProgress),
{
    paths.ensure_data_dir()?;
    let catalog = Catalog::open(&paths.database_path).await?;
    let mut sync_results = Vec::with_capacity(6);
    for scanner in installed(paths.provider_roots()) {
        match scanner {
            Ok(scanner) => {
                let result =
                    sync_scanner_with_progress(&catalog, scanner, &mut on_progress).await?;
                sync_results.push(result);
            }
            Err(error) => {
                on_progress(SyncProgress::ProviderDiscovering {
                    provider: error.provider_id().as_str().to_owned(),
                });
                let result = ProviderSyncResult {
                    provider: error.provider_id().as_str().to_owned(),
                    report: ProviderScanReport::default(),
                    failure: Some(error.to_string()),
                };
                on_progress(SyncProgress::ProviderCompleted {
                    provider: result.provider.clone(),
                    report: result.report.clone(),
                    failure: result.failure.clone(),
                });
                sync_results.push(result);
            }
        }
    }
    let sessions_available = catalog.session_count().await?;
    let provider_session_counts = catalog.provider_session_counts().await?;
    catalog.close().await;

    Ok(build_sync_summary(
        &sync_results,
        sessions_available,
        &provider_session_counts,
    ))
}

fn build_sync_summary(
    sync_results: &[ProviderSyncResult],
    sessions_available: u64,
    provider_session_counts: &BTreeMap<String, u64>,
) -> SyncSummary {
    let provider_summaries = sync_results
        .iter()
        .map(|result| ProviderSyncSummary {
            provider: result.provider.clone(),
            candidate_sources: result.report.candidate_sources,
            refreshed_sources: result.report.refreshed_sources,
            partial_sources: result.report.partial_sources,
            failed_sources: result.report.failed_sources,
            missing_sources: result.report.missing_sources,
            sessions_available: provider_sessions(provider_session_counts, &result.provider),
            failure: result.failure.clone(),
        })
        .collect::<Vec<_>>();

    SyncSummary {
        schema_version: SCHEMA_VERSION,
        collectors_installed: u8::try_from(sync_results.len()).unwrap_or(u8::MAX),
        providers_failed: u8::try_from(
            sync_results
                .iter()
                .filter(|result| result.failure.is_some())
                .count(),
        )
        .unwrap_or(u8::MAX),
        sources_refreshed: sync_results
            .iter()
            .map(|result| result.report.refreshed_sources)
            .sum(),
        sources_partial: sync_results
            .iter()
            .map(|result| result.report.partial_sources)
            .sum(),
        sources_failed: sync_results
            .iter()
            .map(|result| result.report.failed_sources)
            .sum(),
        sources_missing: sync_results
            .iter()
            .map(|result| result.report.missing_sources)
            .sum(),
        sessions_available,
        provider_summaries,
        message: "Provider sources were read without modifying provider-owned files".to_owned(),
    }
}

#[cfg(test)]
async fn sync_scanner(
    catalog: &Catalog,
    scanner: Box<dyn ProviderScanner>,
) -> Result<ProviderSyncResult, StorageError> {
    sync_scanner_with_progress(catalog, scanner, &mut |_| {}).await
}

async fn sync_scanner_with_progress<F>(
    catalog: &Catalog,
    scanner: Box<dyn ProviderScanner>,
    on_progress: &mut F,
) -> Result<ProviderSyncResult, StorageError>
where
    F: FnMut(SyncProgress),
{
    let provider = scanner.provider_id().as_str().to_owned();
    on_progress(SyncProgress::ProviderDiscovering {
        provider: provider.clone(),
    });
    match catalog
        .scan_with_progress(&*scanner, |progress| match progress {
            CatalogScanProgress::CandidatesDiscovered { candidate_sources } => {
                on_progress(SyncProgress::ProviderCandidates {
                    provider: provider.clone(),
                    candidate_sources,
                });
            }
            CatalogScanProgress::SourceStaged {
                processed_sources,
                candidate_sources,
            } => {
                on_progress(SyncProgress::SourceStaged {
                    provider: provider.clone(),
                    processed_sources,
                    candidate_sources,
                });
            }
        })
        .await
    {
        Ok(report) => {
            let result = ProviderSyncResult {
                provider,
                report,
                failure: None,
            };
            on_progress(SyncProgress::ProviderCompleted {
                provider: result.provider.clone(),
                report: result.report.clone(),
                failure: None,
            });
            Ok(result)
        }
        Err(CatalogScanError::Provider(error)) => {
            let result = ProviderSyncResult {
                provider,
                report: ProviderScanReport::default(),
                failure: Some(error.to_string()),
            };
            on_progress(SyncProgress::ProviderCompleted {
                provider: result.provider.clone(),
                report: result.report.clone(),
                failure: result.failure.clone(),
            });
            Ok(result)
        }
        Err(CatalogScanError::Storage(error)) => Err(error),
    }
}

fn provider_sessions(counts: &BTreeMap<String, u64>, provider: &str) -> u64 {
    counts.get(provider).copied().unwrap_or(0)
}

/// Reads catalog session summaries without starting a provider scan.
///
/// # Errors
///
/// Returns an error when Agentlog-owned storage cannot be opened or queried.
pub async fn list_shell(
    paths: &AppPaths,
    limit: u32,
) -> anyhow::Result<Vec<CatalogSessionSummary>> {
    paths.ensure_data_dir()?;
    let catalog = Catalog::open(&paths.database_path).await?;
    let sessions = catalog.list_sessions(limit).await?;
    catalog.close().await;
    Ok(sessions)
}

/// Reads one catalog preview without starting a provider scan.
///
/// # Errors
///
/// Returns an error when the selected session is unavailable or the Agentlog
/// catalog cannot be opened.
pub async fn show_shell(
    paths: &AppPaths,
    session_id: i64,
) -> anyhow::Result<CatalogSessionPreview> {
    paths.ensure_data_dir()?;
    let catalog = Catalog::open(&paths.database_path).await?;
    let session = catalog.preview(session_id).await?;
    catalog.close().await;
    Ok(session)
}

/// Reads a current session preview by the stable identity retained in a list row.
///
/// # Errors
///
/// Returns an error when the session is unavailable or the Agentlog catalog
/// cannot be opened.
pub async fn show_shell_by_identity(
    paths: &AppPaths,
    session: &CatalogSessionSummary,
) -> anyhow::Result<CatalogSessionPreview> {
    paths.ensure_data_dir()?;
    let catalog = Catalog::open(&paths.database_path).await?;
    let preview = catalog.preview_by_identity(session).await?;
    catalog.close().await;
    Ok(preview)
}

/// Reads all retained source diagnostics without starting a provider scan.
///
/// # Errors
///
/// Returns an error when the Agentlog catalog cannot be opened or queried.
pub async fn diagnostics_shell(paths: &AppPaths) -> anyhow::Result<Vec<CatalogSourceDiagnostic>> {
    paths.ensure_data_dir()?;
    let catalog = Catalog::open(&paths.database_path).await?;
    let diagnostics = catalog.source_diagnostics().await?;
    catalog.close().await;
    Ok(diagnostics)
}

/// Paths shown by the diagnostic command. All paths except provider sources are
/// owned by Agentlog. Collectors will report provider source paths later.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PathsReport {
    pub agentlog_home: PathBuf,
    pub config_file: PathBuf,
    pub data_directory: PathBuf,
    pub database: PathBuf,
    pub provider_sources: String,
}

#[must_use]
pub fn paths_report(paths: &AppPaths) -> PathsReport {
    PathsReport {
        agentlog_home: paths.home().to_path_buf(),
        config_file: paths.config_path.clone(),
        data_directory: paths.data_dir.clone(),
        database: paths.database_path.clone(),
        provider_sources: source_descriptions(paths.provider_roots()),
    }
}

/// The exact Agentlog-owned catalog target described before a purge operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PurgeSummary {
    pub database: PathBuf,
    pub sources: u64,
    pub sessions: u64,
    pub transcript_items: u64,
    pub approximate_bytes: u64,
    pub cleared: bool,
}

/// A purge preview whose private token binds confirmation to exact content.
pub struct PurgePreview {
    pub summary: PurgeSummary,
    token: PurgePreviewToken,
}

enum PurgePreviewToken {
    Absent,
    Present {
        database_identity: CatalogFileIdentity,
        content: CatalogContentToken,
    },
}

/// Builds a read-only purge preview and retains its exact-content token.
///
/// # Errors
///
/// Returns an error when the catalog cannot be copied and validated safely.
pub async fn preview_purge_catalog(paths: &AppPaths) -> anyhow::Result<PurgePreview> {
    let Some(target) = inspect_purge_target(&paths.database_path)? else {
        return Ok(PurgePreview {
            summary: empty_purge_summary(paths),
            token: PurgePreviewToken::Absent,
        });
    };
    let counts = inspect_catalog_snapshot(&target.snapshot_database)
        .await
        .with_context(|| {
            format!(
                "validate private snapshot of Agentlog-owned database {}",
                paths.database_path.display()
            )
        })?;
    Ok(PurgePreview {
        summary: PurgeSummary {
            database: paths.database_path.clone(),
            sources: counts.sources,
            sessions: counts.sessions,
            transcript_items: counts.transcript_items,
            approximate_bytes: target.approximate_bytes,
            cleared: false,
        },
        token: PurgePreviewToken::Present {
            database_identity: target.database_identity,
            content: target.content_token,
        },
    })
}

/// Purges only when the locked catalog still matches an interactive preview.
///
/// # Errors
///
/// Returns an error without deleting data when the catalog changed after the
/// preview or when the purge cannot acquire writer serialization.
pub async fn purge_previewed_catalog(
    paths: &AppPaths,
    preview: PurgePreview,
) -> anyhow::Result<PurgeSummary> {
    match preview.token {
        PurgePreviewToken::Absent => {
            if inspect_catalog_file_state(&paths.database_path)?.is_some() {
                anyhow::bail!(
                    "Agentlog catalog changed after the purge preview; no data was deleted; rerun `agentlog purge` to review the updated catalog"
                );
            }
            Ok(empty_purge_summary(paths))
        }
        PurgePreviewToken::Present {
            database_identity,
            content,
        } => {
            let result = purge_existing_catalog(
                &paths.database_path,
                database_identity,
                Some(&content),
                PURGE_SNAPSHOT_MAX_BYTES,
                |_| {},
            )
            .await
            .with_context(|| {
                format!(
                    "purge previewed Agentlog-owned database {}",
                    paths.database_path.display()
                )
            })?;
            Ok(purge_result_summary(paths, result))
        }
    }
}

/// Reports, and when explicitly confirmed purges, only the Agentlog-owned
/// catalog contents.
///
/// The database path is fixed by [`AppPaths`]; configuration and provider-owned
/// source paths are neither opened for writing nor selected for deletion.
///
/// # Errors
///
/// Previewing reads a bounded private copy of the `SQLite` main/WAL contents,
/// while fingerprinting the complete main/WAL/SHM file set, so it never opens
/// or writes the live catalog. Confirmation obtains `SQLite` writer
/// serialization, then measures the complete main/WAL/SHM file set, counts
/// rows, and purges them while preserving the schema and database files.
///
/// Returns an error when the database is not a regular Agentlog v1 catalog, a
/// file changes while the private preview snapshot is taken, or an explicitly
/// requested purge cannot acquire `SQLite`'s writer lock.
pub async fn purge_catalog_shell(
    paths: &AppPaths,
    confirmed: bool,
) -> anyhow::Result<PurgeSummary> {
    purge_catalog_shell_with_hooks(paths, confirmed, |_| async {}, |_| {}).await
}

async fn purge_catalog_shell_with_hooks<BeforeLock, BeforeLockFuture, AfterMeasurement>(
    paths: &AppPaths,
    confirmed: bool,
    before_lock: BeforeLock,
    after_measurement: AfterMeasurement,
) -> anyhow::Result<PurgeSummary>
where
    BeforeLock: FnOnce(u64) -> BeforeLockFuture,
    BeforeLockFuture: Future<Output = ()>,
    AfterMeasurement: FnOnce(crate::storage::CatalogPurgeResult),
{
    if confirmed {
        let Some(target) = inspect_confirmed_purge_target(&paths.database_path)? else {
            return Ok(empty_purge_summary(paths));
        };
        before_lock(target.approximate_bytes).await;
        let result = purge_existing_catalog(
            &paths.database_path,
            target.database_identity,
            None,
            PURGE_SNAPSHOT_MAX_BYTES,
            after_measurement,
        )
        .await
        .with_context(|| {
            format!(
                "purge Agentlog-owned database {}",
                paths.database_path.display()
            )
        })?;
        return Ok(purge_result_summary(paths, result));
    }

    let Some(target) = inspect_purge_target(&paths.database_path)? else {
        return Ok(empty_purge_summary(paths));
    };
    let counts = inspect_catalog_snapshot(&target.snapshot_database)
        .await
        .with_context(|| {
            format!(
                "validate private snapshot of Agentlog-owned database {}",
                paths.database_path.display()
            )
        })?;
    Ok(PurgeSummary {
        database: paths.database_path.clone(),
        sources: counts.sources,
        sessions: counts.sessions,
        transcript_items: counts.transcript_items,
        approximate_bytes: target.approximate_bytes,
        cleared: false,
    })
}

fn empty_purge_summary(paths: &AppPaths) -> PurgeSummary {
    PurgeSummary {
        database: paths.database_path.clone(),
        sources: 0,
        sessions: 0,
        transcript_items: 0,
        approximate_bytes: 0,
        cleared: false,
    }
}

fn purge_result_summary(
    paths: &AppPaths,
    result: crate::storage::CatalogPurgeResult,
) -> PurgeSummary {
    PurgeSummary {
        database: paths.database_path.clone(),
        sources: result.counts.sources,
        sessions: result.counts.sessions,
        transcript_items: result.counts.transcript_items,
        approximate_bytes: result.approximate_bytes,
        cleared: true,
    }
}

#[derive(Debug)]
struct PurgeTarget {
    approximate_bytes: u64,
    database_identity: CatalogFileIdentity,
    content_token: CatalogContentToken,
    _snapshot_directory: tempfile::TempDir,
    snapshot_database: PathBuf,
}

struct ConfirmedPurgeTarget {
    approximate_bytes: u64,
    database_identity: CatalogFileIdentity,
}

fn inspect_confirmed_purge_target(database: &Path) -> anyhow::Result<Option<ConfirmedPurgeTarget>> {
    let Some(files) = inspect_catalog_file_set(database)? else {
        return Ok(None);
    };
    let approximate_bytes =
        catalog_file_set_size(database, files.iter().map(|file| file.state.length))?;
    let database_identity = files
        .first()
        .context("catalog file set is missing its main database")?
        .state
        .identity;
    Ok(Some(ConfirmedPurgeTarget {
        approximate_bytes,
        database_identity,
    }))
}

fn inspect_purge_target(database: &Path) -> anyhow::Result<Option<PurgeTarget>> {
    inspect_purge_target_with_hook(database, || {})
}

fn inspect_purge_target_with_hook(
    database: &Path,
    after_copy: impl FnOnce(),
) -> anyhow::Result<Option<PurgeTarget>> {
    let Some(files) = read_catalog_file_set(database)? else {
        return Ok(None);
    };
    let approximate_bytes =
        catalog_file_set_size(database, files.iter().map(|file| file.state.length))?;
    let snapshot_directory = tempfile::Builder::new()
        .prefix("agentlog-purge-")
        .tempdir()
        .context("create private catalog inspection snapshot")?;
    let snapshot_database = snapshot_directory.path().join("agentlog.sqlite3");
    for file in &files {
        if file.suffix == Some("-shm") {
            continue;
        }
        let snapshot_path = if file.is_main {
            snapshot_database.clone()
        } else {
            sqlite_sidecar_path(&snapshot_database, file.suffix.expect("sidecar suffix"))
        };
        fs::write(&snapshot_path, &file.bytes)
            .with_context(|| format!("write private snapshot {}", snapshot_path.display()))?;
    }
    after_copy();
    validate_catalog_file_set(database, &files)?;

    let main = files
        .iter()
        .find(|file| file.is_main)
        .context("catalog file set is missing its main database")?;
    let wal = files
        .iter()
        .find(|file| file.suffix == Some("-wal"))
        .map(|file| file.bytes.as_slice());

    Ok(Some(PurgeTarget {
        approximate_bytes,
        database_identity: main.state.identity,
        content_token: CatalogContentToken::new(&main.bytes, wal),
        _snapshot_directory: snapshot_directory,
        snapshot_database,
    }))
}

fn catalog_file_set_size(
    database: &Path,
    lengths: impl IntoIterator<Item = u64>,
) -> anyhow::Result<u64> {
    let approximate_bytes = lengths.into_iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(length)
            .context("database size exceeds supported range")
    })?;
    if approximate_bytes > PURGE_SNAPSHOT_MAX_BYTES {
        anyhow::bail!(
            "refusing to inspect {}; the SQLite file set exceeds the {} byte snapshot limit",
            database.display(),
            PURGE_SNAPSHOT_MAX_BYTES
        );
    }
    Ok(approximate_bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CatalogFile {
    path: PathBuf,
    suffix: Option<&'static str>,
    is_main: bool,
    state: CatalogFileState,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CatalogFileDescriptor {
    path: PathBuf,
    suffix: Option<&'static str>,
    is_main: bool,
    state: CatalogFileState,
}

fn inspect_catalog_file_set(database: &Path) -> anyhow::Result<Option<Vec<CatalogFileDescriptor>>> {
    let Some(main) = inspect_catalog_file_state(database)? else {
        return Ok(None);
    };
    let mut files = vec![CatalogFileDescriptor {
        path: database.to_path_buf(),
        suffix: None,
        is_main: true,
        state: main,
    }];
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(database, suffix);
        if let Some(state) = inspect_catalog_file_state(&sidecar)? {
            files.push(CatalogFileDescriptor {
                path: sidecar,
                state,
                is_main: false,
                suffix: Some(suffix),
            });
        }
    }
    Ok(Some(files))
}

fn read_catalog_file_set(database: &Path) -> anyhow::Result<Option<Vec<CatalogFile>>> {
    let Some(descriptors) = inspect_catalog_file_set(database)? else {
        return Ok(None);
    };
    catalog_file_set_size(
        database,
        descriptors.iter().map(|descriptor| descriptor.state.length),
    )?;
    descriptors
        .iter()
        .map(read_catalog_file)
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

fn read_catalog_file(descriptor: &CatalogFileDescriptor) -> anyhow::Result<CatalogFile> {
    let file = File::open(&descriptor.path)
        .with_context(|| format!("read Agentlog database file {}", descriptor.path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("inspect opened database file {}", descriptor.path.display()))?;
    let opened_state = CatalogFileState::from_metadata(&descriptor.path, &opened_metadata)?;
    if opened_state != descriptor.state {
        return catalog_changed();
    }

    let read_limit = descriptor
        .state
        .length
        .checked_add(1)
        .context("database size exceeds supported range")?;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read Agentlog database file {}", descriptor.path.display()))?;
    if u64::try_from(bytes.len()).ok() != Some(descriptor.state.length) {
        return catalog_changed();
    }

    Ok(CatalogFile {
        path: descriptor.path.clone(),
        suffix: descriptor.suffix,
        is_main: descriptor.is_main,
        state: descriptor.state,
        bytes,
    })
}

fn validate_catalog_file_set(database: &Path, expected: &[CatalogFile]) -> anyhow::Result<()> {
    let Some(current) = inspect_catalog_file_set(database)? else {
        return catalog_changed();
    };
    catalog_file_set_size(
        database,
        current.iter().map(|descriptor| descriptor.state.length),
    )?;
    if !catalog_file_descriptors_match(&current, expected) {
        return catalog_changed();
    }

    for (descriptor, expected) in current.iter().zip(expected) {
        if read_catalog_file(descriptor)?.bytes != expected.bytes {
            return catalog_changed();
        }
    }
    let Some(final_state) = inspect_catalog_file_set(database)? else {
        return catalog_changed();
    };
    if !catalog_file_descriptors_match(&final_state, expected) {
        return catalog_changed();
    }
    Ok(())
}

fn catalog_file_descriptors_match(
    current: &[CatalogFileDescriptor],
    expected: &[CatalogFile],
) -> bool {
    current.len() == expected.len()
        && current.iter().zip(expected).all(|(current, expected)| {
            current.path == expected.path
                && current.suffix == expected.suffix
                && current.is_main == expected.is_main
                && current.state == expected.state
        })
}

fn catalog_changed<T>() -> anyhow::Result<T> {
    Err(anyhow::anyhow!(
        "Agentlog database file set changed while preparing a read-only purge preview; retry the command"
    ))
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use sqlx::{
        SqlitePool,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };

    use crate::{
        paths::AppPaths,
        providers::{
            ProviderId, ProviderScan, ProviderScanError, ProviderScanFuture, ProviderScanReport,
            ProviderScanner, SourceOutcome, opencode::create_test_database,
        },
        storage::{
            Catalog, SCHEMA_VERSION, SourceIdentity, SourceSnapshot, StorageError, open_database,
        },
    };

    fn temporary_directory() -> TempDir {
        let platform_temp = std::env::temp_dir()
            .canonicalize()
            .expect("resolve platform temporary directory");
        TempDir::new_in(platform_temp).expect("temporary directory")
    }

    use super::{
        PURGE_SNAPSHOT_MAX_BYTES, ProviderSyncResult, SyncProgress, build_sync_summary,
        catalog_file_set_size, diagnostics_shell, inspect_catalog_file_set, inspect_purge_target,
        inspect_purge_target_with_hook, list_shell, preview_purge_catalog, purge_catalog_shell,
        purge_catalog_shell_with_hooks, purge_previewed_catalog, sqlite_sidecar_path, sync_scanner,
        sync_scanner_with_progress, sync_shell,
    };

    struct ProviderFailureScanner;

    impl ProviderScanner for ProviderFailureScanner {
        fn provider_id(&self) -> ProviderId {
            ProviderId::Codex
        }

        fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "provider discovery denied",
            )
            .into())
        }
    }

    struct StorageFailureScanner;

    impl ProviderScanner for StorageFailureScanner {
        fn provider_id(&self) -> ProviderId {
            ProviderId::Codex
        }

        fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
            Ok(Box::new(StorageFailureScan {
                outcome: Some(SourceOutcome::Accepted(SourceSnapshot {
                    identity: SourceIdentity {
                        provider: "claude",
                        source_format: "test",
                        canonical_locator: "test://mismatch".to_owned(),
                    },
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: Vec::new(),
                })),
            }))
        }
    }

    struct StorageFailureScan {
        outcome: Option<SourceOutcome>,
    }

    impl ProviderScan for StorageFailureScan {
        fn candidate_sources(&self) -> u64 {
            1
        }

        fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
            let outcome = self.outcome.take();
            Box::pin(async move { Ok(outcome) })
        }
    }

    struct ProgressScanner;

    impl ProviderScanner for ProgressScanner {
        fn provider_id(&self) -> ProviderId {
            ProviderId::Codex
        }

        fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
            Ok(Box::new(ProgressScan {
                outcome: Some(SourceOutcome::Accepted(SourceSnapshot {
                    identity: SourceIdentity {
                        provider: "codex",
                        source_format: "test",
                        canonical_locator: "test://progress".to_owned(),
                    },
                    diagnostic_status: "ok",
                    diagnostic_message: None,
                    sessions: Vec::new(),
                })),
            }))
        }
    }

    struct ProgressScan {
        outcome: Option<SourceOutcome>,
    }

    impl ProviderScan for ProgressScan {
        fn candidate_sources(&self) -> u64 {
            1
        }

        fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
            let outcome = self.outcome.take();
            Box::pin(async move { Ok(outcome) })
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct FileFingerprint {
        bytes: Vec<u8>,
        length: u64,
        modified: std::time::SystemTime,
    }

    fn fingerprint(path: &std::path::Path) -> FileFingerprint {
        let metadata = fs::metadata(path).expect("file metadata");
        FileFingerprint {
            bytes: fs::read(path).expect("file bytes"),
            length: metadata.len(),
            modified: metadata.modified().expect("file modification time"),
        }
    }

    fn directory_fingerprint(directory: &std::path::Path) -> Vec<(String, FileFingerprint)> {
        let mut files = fs::read_dir(directory)
            .expect("read Agentlog data directory")
            .map(|entry| {
                let entry = entry.expect("directory entry");
                (
                    entry
                        .file_name()
                        .into_string()
                        .expect("test filenames are UTF-8"),
                    fingerprint(&entry.path()),
                )
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    async fn scanned_purge_fixture() -> (TempDir, AppPaths, std::path::PathBuf) {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        let gemini_root = temporary.path().join("gemini");
        let source = gemini_root.join("tmp/session.jsonl");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        fs::create_dir_all(source.parent().expect("Gemini source parent"))
            .expect("create Gemini directory");
        fs::write(
            &source,
            "{\"sessionId\":\"clear-session\"}\n{\"type\":\"user\",\"content\":\"retained request\"}\n",
        )
        .expect("write provider source");
        let config = format!(
            "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
            temporary.path().join("empty-codex").display(),
            temporary.path().join("empty-claude").display(),
            temporary.path().join("empty-opencode").display(),
            gemini_root.display(),
            temporary.path().join("empty-cursor").display(),
            temporary.path().join("empty-kimi").display(),
        );
        fs::write(agentlog_home.join("config.toml"), config).expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");
        let scan = sync_shell(&paths).await.expect("sync fixture source");
        assert_eq!(scan.sessions_available, 1);

        (temporary, paths, source)
    }

    #[tokio::test]
    async fn diagnostics_shell_opens_the_catalog_without_scanning_sources() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        let diagnostics = diagnostics_shell(&paths)
            .await
            .expect("read empty diagnostics");

        assert!(diagnostics.is_empty());
        assert!(paths.database_path.is_file());
    }

    #[tokio::test]
    async fn provider_failure_is_reported_without_becoming_a_source_failure() {
        let temporary = temporary_directory();
        let catalog = Catalog::open(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        let failed = sync_scanner(&catalog, Box::new(ProviderFailureScanner))
            .await
            .expect("provider failure is an observable sync result");
        assert!(failed.failure.is_some());
        assert_eq!(failed.report.failed_sources, 0);

        let mut results = vec![failed];
        for provider in ["claude", "opencode", "gemini", "cursor", "kimi"] {
            results.push(ProviderSyncResult {
                provider: provider.to_owned(),
                report: ProviderScanReport::default(),
                failure: None,
            });
        }
        let summary = build_sync_summary(&results, 0, &std::collections::BTreeMap::new());
        assert_eq!(summary.collectors_installed, 6);
        assert_eq!(summary.providers_failed, 1);
        assert_eq!(summary.sources_failed, 0);
        let json = serde_json::to_value(&summary).expect("serialize provider failure summary");
        assert_eq!(json["providers_failed"], 1);
        catalog.close().await;
    }

    #[tokio::test]
    async fn storage_failure_is_propagated_instead_of_reported_as_provider_completion() {
        let temporary = temporary_directory();
        let catalog = Catalog::open(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        let error = sync_scanner(&catalog, Box::new(StorageFailureScanner))
            .await
            .expect_err("storage failure must abort sync");
        assert!(matches!(
            error,
            StorageError::ProviderIdentityMismatch {
                expected: "codex",
                found: "claude"
            }
        ));
        catalog.close().await;
    }

    #[tokio::test]
    async fn sync_progress_reports_staged_sources_before_committed_provider_completion() {
        let temporary = temporary_directory();
        let catalog = Catalog::open(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        let mut progress = Vec::new();

        let result =
            sync_scanner_with_progress(&catalog, Box::new(ProgressScanner), &mut |event| {
                progress.push(event);
            })
            .await
            .expect("synchronize progress fixture");

        assert_eq!(result.report.refreshed_sources, 1);
        assert_eq!(
            progress,
            vec![
                SyncProgress::ProviderDiscovering {
                    provider: "codex".to_owned(),
                },
                SyncProgress::ProviderCandidates {
                    provider: "codex".to_owned(),
                    candidate_sources: 1,
                },
                SyncProgress::SourceStaged {
                    provider: "codex".to_owned(),
                    processed_sources: 1,
                    candidate_sources: 1,
                },
                SyncProgress::ProviderCompleted {
                    provider: "codex".to_owned(),
                    report: ProviderScanReport {
                        candidate_sources: 1,
                        refreshed_sources: 1,
                        partial_sources: 0,
                        failed_sources: 0,
                        missing_sources: 0,
                    },
                    failure: None,
                },
            ]
        );
        catalog.close().await;
    }

    #[tokio::test]
    async fn purge_preview_and_missing_confirmation_leave_all_inputs_unchanged() {
        let (_temporary, paths, source) = scanned_purge_fixture().await;
        let writer = open_database(&paths.database_path)
            .await
            .expect("open catalog to retain live WAL sidecars");
        sqlx::query(
            "INSERT INTO sources (provider, source_format, canonical_locator) VALUES ('test', 'test', 'test://sidecar')",
        )
        .execute(&writer)
        .await
        .expect("write catalog WAL frame");
        let live_shm = sqlite_sidecar_path(&paths.database_path, "-shm");
        assert!(live_shm.is_file(), "live WAL database has an SHM sidecar");
        let private_target = inspect_purge_target(&paths.database_path)
            .expect("copy private purge target")
            .expect("catalog target exists");
        assert!(
            !sqlite_sidecar_path(&private_target.snapshot_database, "-shm").exists(),
            "live SHM is not copied into the private snapshot"
        );
        drop(private_target);
        let database_before = directory_fingerprint(&paths.data_dir);
        let config_before = fingerprint(&paths.config_path);
        let source_before = fingerprint(&source);

        let preview = purge_catalog_shell(&paths, false)
            .await
            .expect("preview purge");
        assert_eq!(preview.sessions, 1);
        assert_eq!(preview.transcript_items, 1);
        assert!(preview.approximate_bytes > 0);
        assert!(!preview.cleared);
        assert_eq!(directory_fingerprint(&paths.data_dir), database_before);

        let without_yes = purge_catalog_shell(&paths, false)
            .await
            .expect("reject unconfirmed purge");
        assert!(!without_yes.cleared);
        assert!(paths.database_path.is_file());
        assert_eq!(directory_fingerprint(&paths.data_dir), database_before);
        assert_eq!(fingerprint(&paths.config_path), config_before);
        assert_eq!(fingerprint(&source), source_before);
        writer.close().await;
    }

    #[tokio::test]
    async fn previewed_purge_rejects_an_intervening_sync_commit_without_deleting_it() {
        let (_temporary, paths, source) = scanned_purge_fixture().await;
        let preview = preview_purge_catalog(&paths)
            .await
            .expect("preview catalog before sync");

        fs::write(
            source
                .parent()
                .expect("Gemini source directory")
                .join("intervening.jsonl"),
            "{\"sessionId\":\"intervening-session\"}\n{\"type\":\"user\",\"content\":\"new request\"}\n",
        )
        .expect("write provider source added after preview");
        sync_shell(&paths).await.expect("commit intervening sync");
        let error = purge_previewed_catalog(&paths, preview)
            .await
            .expect_err("reject catalog changed since preview");

        assert!(format!("{error:#}").contains("rerun `agentlog purge`"));
        let sessions = list_shell(&paths, 10)
            .await
            .expect("read catalog after rejected purge");
        assert_eq!(
            sessions.len(),
            2,
            "intervening sync result remains cataloged"
        );
    }

    #[tokio::test]
    async fn previewed_purge_clears_the_unchanged_exact_catalog() {
        let (_temporary, paths, _source) = scanned_purge_fixture().await;
        let preview = preview_purge_catalog(&paths)
            .await
            .expect("preview unchanged catalog");

        let cleared = purge_previewed_catalog(&paths, preview)
            .await
            .expect("purge exact previewed catalog");

        assert_eq!(cleared.sources, 1);
        assert_eq!(cleared.sessions, 1);
        assert_eq!(cleared.transcript_items, 1);
        assert!(cleared.cleared);
    }

    #[tokio::test]
    async fn purge_preview_counts_zero_session_diagnostic_sources() {
        let (_temporary, paths, _source) = scanned_purge_fixture().await;
        let writer = open_database(&paths.database_path)
            .await
            .expect("open catalog for diagnostic source");
        sqlx::query(
            "INSERT INTO sources (
                 provider, source_format, canonical_locator, diagnostic_status, diagnostic_message
             ) VALUES ('test', 'test', 'test://zero-session', 'error', 'unreadable')",
        )
        .execute(&writer)
        .await
        .expect("insert zero-session diagnostic source");
        writer.close().await;

        let preview = preview_purge_catalog(&paths)
            .await
            .expect("preview diagnostic source");

        assert_eq!(preview.summary.sources, 2);
        assert_eq!(preview.summary.sessions, 1);
        assert_eq!(preview.summary.transcript_items, 1);
    }

    #[tokio::test]
    async fn purge_preview_rejects_a_file_set_presence_change_during_snapshot() {
        let (temporary, paths, _source) = scanned_purge_fixture().await;
        let shm = sqlite_sidecar_path(&paths.database_path, "-shm");
        let moved_shm = temporary.path().join("moved-agentlog-shm");
        let shm_was_present = shm.exists();

        let error = inspect_purge_target_with_hook(&paths.database_path, || {
            if shm_was_present {
                fs::rename(&shm, &moved_shm).expect("move existing SHM during snapshot");
            } else {
                fs::write(&shm, b"appeared during snapshot").expect("create SHM during snapshot");
            }
        })
        .expect_err("reject changed SQLite file-set presence");

        assert!(format!("{error:#}").contains("file set changed"));
    }

    #[test]
    fn purge_snapshot_rejects_an_oversized_sparse_file_before_reading_it() {
        let temporary = temporary_directory();
        let database = temporary.path().join("agentlog.sqlite3");
        let file = fs::File::create(&database).expect("create sparse database");
        file.set_len(PURGE_SNAPSHOT_MAX_BYTES + 1)
            .expect("size sparse database");

        let error =
            inspect_purge_target(&database).expect_err("reject oversized metadata before reading");

        assert!(format!("{error:#}").contains("exceeds the 536870912 byte snapshot limit"));
    }

    #[tokio::test]
    async fn confirmed_purge_reports_locked_counts_and_size_after_an_intervening_commit() {
        let (_temporary, paths, _source) = scanned_purge_fixture().await;
        let writer = open_database(&paths.database_path)
            .await
            .expect("open catalog for intervening write");
        let preflight_size = Arc::new(Mutex::new(None));
        let observed_preflight_size = Arc::clone(&preflight_size);
        let locked_size = Arc::new(Mutex::new(None));
        let measured_size = Arc::clone(&locked_size);
        let measured_path = paths.database_path.clone();

        let cleared = purge_catalog_shell_with_hooks(
            &paths,
            true,
            move |approximate_bytes| async move {
                *observed_preflight_size.lock().expect("lock preflight size") =
                    Some(approximate_bytes);
                let source_id = sqlx::query(
                    "INSERT INTO sources (provider, source_format, canonical_locator)
                     VALUES ('test', 'test', 'test://transaction-counts')",
                )
                .execute(&writer)
                .await
                .expect("insert intervening source")
                .last_insert_rowid();
                let session_id = sqlx::query(
                    "INSERT INTO sessions (source_id, session_key, title)
                     VALUES (?, 'transaction-counts', 'intervening session')",
                )
                .bind(source_id)
                .execute(&writer)
                .await
                .expect("insert intervening session")
                .last_insert_rowid();
                sqlx::query(
                    "INSERT INTO transcript_items (session_id, ordinal, item_kind, content)
                     VALUES (?, 0, 'user_text', 'intervening item')",
                )
                .bind(session_id)
                .execute(&writer)
                .await
                .expect("insert intervening transcript item");
                writer.close().await;
            },
            move |result| {
                let files = inspect_catalog_file_set(&measured_path)
                    .expect("inspect locked catalog file set")
                    .expect("locked catalog exists");
                let current_size = catalog_file_set_size(
                    &measured_path,
                    files.iter().map(|file| file.state.length),
                )
                .expect("measure locked catalog file set");
                assert_eq!(result.approximate_bytes, current_size);
                *measured_size.lock().expect("lock measured size") = Some(current_size);
            },
        )
        .await
        .expect("purge current locked transaction contents");
        assert_eq!(cleared.sessions, 2);
        assert_eq!(cleared.transcript_items, 2);
        assert_eq!(
            Some(cleared.approximate_bytes),
            *locked_size.lock().expect("read measured size")
        );
        assert!(
            preflight_size
                .lock()
                .expect("read preflight size")
                .is_some(),
            "confirmed purge must preflight the file set before the hook"
        );
        assert!(cleared.cleared);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirmed_purge_rejects_a_final_component_symlink() {
        let (temporary, paths, _source) = scanned_purge_fixture().await;
        let real_database = temporary.path().join("real-agentlog.sqlite3");
        fs::rename(&paths.database_path, &real_database).expect("move real database");
        symlink(&real_database, &paths.database_path).expect("link database path");
        let real_before = fingerprint(&real_database);

        let error = purge_catalog_shell(&paths, true)
            .await
            .expect_err("reject final database symlink during purge");

        assert!(format!("{error:#}").contains("symbolic-link component"));
        assert_eq!(fingerprint(&real_database), real_before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirmed_purge_rejects_an_ancestor_symlink() {
        let (temporary, paths, _source) = scanned_purge_fixture().await;
        let alias = temporary.path().join("agentlog-alias");
        symlink(&paths.data_dir, &alias).expect("link Agentlog home");
        let aliased_paths = AppPaths::resolve(Some(alias)).expect("resolve aliased paths");
        let before = directory_fingerprint(&paths.data_dir);

        let error = purge_catalog_shell(&aliased_paths, true)
            .await
            .expect_err("reject Agentlog home symlink during purge");

        assert!(format!("{error:#}").contains("symbolic-link component"));
        assert_eq!(directory_fingerprint(&paths.data_dir), before);
    }

    #[tokio::test]
    async fn confirmed_purge_empties_only_catalog_and_sync_rebuilds_available_fixture() {
        let (_temporary, paths, source) = scanned_purge_fixture().await;
        let config_before = fingerprint(&paths.config_path);
        let source_before = fingerprint(&source);

        let cleared = purge_catalog_shell(&paths, true)
            .await
            .expect("confirm purge");
        assert!(cleared.cleared);
        assert_eq!(cleared.sessions, 1);
        assert_eq!(cleared.transcript_items, 1);
        assert!(paths.database_path.is_file());
        assert_eq!(fingerprint(&paths.config_path), config_before);
        assert_eq!(fingerprint(&source), source_before);

        assert!(
            list_shell(&paths, 10)
                .await
                .expect("list purged catalog")
                .is_empty()
        );
        assert!(
            diagnostics_shell(&paths)
                .await
                .expect("list purged diagnostics")
                .is_empty()
        );

        let rebuilt = sync_shell(&paths)
            .await
            .expect("sync rebuilds available fixture");
        assert_eq!(rebuilt.sessions_available, 1);
        let sessions = list_shell(&paths, 10).await.expect("list rebuilt catalog");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("retained request"));
    }

    #[tokio::test]
    async fn purge_rejects_a_lookalike_v1_database_without_changing_it() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&paths.database_path)
                    .create_if_missing(true),
            )
            .await
            .expect("create lookalike database");
        for statement in [
            "CREATE TABLE sources (id INTEGER PRIMARY KEY)",
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY)",
            "CREATE TABLE transcript_items (id INTEGER PRIMARY KEY)",
            "PRAGMA user_version = 1",
        ] {
            sqlx::query(statement)
                .execute(&pool)
                .await
                .expect("create lookalike schema");
        }
        pool.close().await;
        let before = directory_fingerprint(&paths.data_dir);

        let error = purge_catalog_shell(&paths, false)
            .await
            .expect_err("reject lookalike database");
        assert!(format!("{error:#}").contains("schema v1 fingerprint"));
        assert_eq!(directory_fingerprint(&paths.data_dir), before);
    }

    #[tokio::test]
    async fn confirmed_purge_fails_when_another_sqlite_writer_is_active() {
        let (_temporary, paths, _source) = scanned_purge_fixture().await;
        let writer: SqlitePool = open_database(&paths.database_path)
            .await
            .expect("open competing writer");
        let transaction = writer
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("hold SQLite writer lock");

        assert!(purge_catalog_shell(&paths, true).await.is_err());
        transaction.rollback().await.expect("release writer lock");
        writer.close().await;
        assert_eq!(
            list_shell(&paths, 10)
                .await
                .expect("list retained catalog")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn shell_sync_creates_catalog_and_reports_empty_codex_collection() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        fs::write(
            agentlog_home.join("config.toml"),
            format!(
                "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
                temporary.path().join("empty-codex").display(),
                temporary.path().join("empty-claude").display(),
                temporary.path().join("empty-opencode").display(),
                temporary.path().join("empty-gemini").display(),
                temporary.path().join("empty-cursor").display(),
                temporary.path().join("empty-kimi").display(),
            ),
        )
        .expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        let summary = sync_shell(&paths).await.expect("run shell sync");

        assert_eq!(summary.schema_version, SCHEMA_VERSION);
        assert_eq!(summary.collectors_installed, 6);
        assert_eq!(summary.sources_refreshed, 0);
        assert_eq!(summary.sources_partial, 0);
        assert_eq!(summary.sources_failed, 0);
        assert_eq!(summary.sources_missing, 0);
        assert_eq!(summary.sessions_available, 0);
        assert_eq!(summary.provider_summaries.len(), 6);
        assert!(
            summary
                .provider_summaries
                .iter()
                .all(|summary| summary.candidate_sources == 0)
        );
        assert!(paths.database_path.is_file());
    }

    #[tokio::test]
    async fn a_gemini_failure_does_not_block_a_cursor_source() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        let gemini_root = temporary.path().join("gemini");
        let cursor_root = temporary.path().join("cursor");
        fs::create_dir_all(gemini_root.join("tmp")).expect("Gemini directory");
        fs::create_dir_all(cursor_root.join("projects/project/session")).expect("Cursor directory");
        fs::write(gemini_root.join("tmp/broken.jsonl"), "not JSON\n").expect("Gemini source");
        fs::write(
            cursor_root.join("projects/project/session/capture.jsonl"),
            "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"visible\"}]}}\n",
        )
        .expect("Cursor source");
        fs::write(
            agentlog_home.join("config.toml"),
            format!(
                "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
                temporary.path().join("empty-codex").display(),
                temporary.path().join("empty-claude").display(),
                temporary.path().join("empty-opencode").display(),
                gemini_root.display(),
                cursor_root.display(),
                temporary.path().join("empty-kimi").display(),
            ),
        )
        .expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        let summary = sync_shell(&paths).await.expect("run shell sync");

        assert_eq!(summary.sources_refreshed, 1);
        assert_eq!(summary.sources_failed, 1);
        assert_eq!(summary.sessions_available, 1);
        assert_eq!(summary.provider_summaries.len(), 6);
        let json = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(json["provider_summaries"].as_array().map(Vec::len), Some(6));
    }

    #[tokio::test]
    async fn a_failed_sync_keeps_a_source_last_good_session_in_the_catalog() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        let gemini_root = temporary.path().join("gemini");
        let source = gemini_root.join("tmp/session.jsonl");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        fs::create_dir_all(source.parent().expect("Gemini source parent"))
            .expect("create Gemini directory");
        fs::write(
            &source,
            "{\"sessionId\":\"session\"}\n{\"type\":\"user\",\"content\":\"last good request\"}\n",
        )
        .expect("write valid Gemini source");
        fs::write(
            agentlog_home.join("config.toml"),
            format!(
                "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
                temporary.path().join("empty-codex").display(),
                temporary.path().join("empty-claude").display(),
                temporary.path().join("empty-opencode").display(),
                gemini_root.display(),
                temporary.path().join("empty-cursor").display(),
                temporary.path().join("empty-kimi").display(),
            ),
        )
        .expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        let first = sync_shell(&paths).await.expect("initial sync");
        assert_eq!(first.sessions_available, 1);
        fs::write(&source, "not JSON\n").expect("make source malformed");

        let refresh = sync_shell(&paths).await.expect("failing sync");
        assert_eq!(refresh.sources_failed, 1);
        assert_eq!(refresh.sessions_available, 1);
        let sessions = super::list_shell(&paths, 10)
            .await
            .expect("list last-good session");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_diagnostic_status, "error");
        assert_eq!(sessions[0].title.as_deref(), Some("last good request"));
    }

    #[tokio::test]
    async fn a_disappeared_source_is_retained_and_diagnosed_missing() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        let gemini_root = temporary.path().join("gemini");
        let source = gemini_root.join("tmp/session.jsonl");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        fs::create_dir_all(source.parent().expect("Gemini source parent"))
            .expect("create Gemini directory");
        fs::write(
            &source,
            "{\"sessionId\":\"session\"}\n{\"type\":\"user\",\"content\":\"retained request\"}\n",
        )
        .expect("write valid Gemini source");
        fs::write(
            agentlog_home.join("config.toml"),
            format!(
                "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
                temporary.path().join("empty-codex").display(),
                temporary.path().join("empty-claude").display(),
                temporary.path().join("empty-opencode").display(),
                gemini_root.display(),
                temporary.path().join("empty-cursor").display(),
                temporary.path().join("empty-kimi").display(),
            ),
        )
        .expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        assert_eq!(
            sync_shell(&paths)
                .await
                .expect("initial sync")
                .sessions_available,
            1
        );
        fs::remove_file(&source).expect("remove provider-owned fixture");

        let summary = sync_shell(&paths).await.expect("sync after disappearance");
        assert_eq!(summary.sources_missing, 1);
        assert_eq!(summary.sources_failed, 0);
        assert_eq!(summary.sessions_available, 1);
        let json = serde_json::to_value(&summary).expect("serialize sync summary");
        assert_eq!(json["sources_missing"], 1);

        let sessions = list_shell(&paths, 10).await.expect("list retained session");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_diagnostic_status, "missing");
        assert_eq!(sessions[0].title.as_deref(), Some("retained request"));
        let diagnostics = diagnostics_shell(&paths).await.expect("read diagnostics");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.provider == "gemini"
                && diagnostic.diagnostic_status == "missing"
                && diagnostic.diagnostic_message.as_deref()
                    == Some("source was absent from a completed provider discovery")
        }));
    }

    #[tokio::test]
    async fn kimi_and_cursor_keep_identical_native_session_keys_in_distinct_sources() {
        let temporary = temporary_directory();
        let agentlog_home = temporary.path().join("agentlog");
        let cursor_root = temporary.path().join("cursor");
        let kimi_root = temporary.path().join("kimi");
        fs::create_dir(&agentlog_home).expect("create Agentlog home");
        fs::create_dir_all(cursor_root.join("projects/project/collision"))
            .expect("Cursor directory");
        fs::write(
            cursor_root.join("projects/project/collision/capture.jsonl"),
            "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"cursor visible\"}]}}\n",
        )
        .expect("Cursor source");
        let kimi_session = kimi_root.join("sessions/work/collision");
        fs::create_dir_all(kimi_session.join("agents/main")).expect("Kimi directory");
        fs::write(
            kimi_session.join("agents/main/wire.jsonl"),
            "{\"type\":\"turn.prompt\",\"input\":[{\"text\":\"kimi visible\"}]}\n",
        )
        .expect("Kimi wire");
        fs::write(
            kimi_session.join("state.json"),
            "{\"agents\":{\"main\":{}}}",
        )
        .expect("Kimi state");
        fs::write(
            kimi_root.join("session_index.jsonl"),
            "{\"sessionId\":\"collision\",\"sessionDir\":\"sessions/work/collision\"}\n",
        )
        .expect("Kimi index");
        fs::write(
            agentlog_home.join("config.toml"),
            format!(
                "[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n",
                temporary.path().join("empty-codex").display(),
                temporary.path().join("empty-claude").display(),
                temporary.path().join("empty-opencode").display(),
                temporary.path().join("empty-gemini").display(),
                cursor_root.display(),
                kimi_root.display(),
            ),
        )
        .expect("write config");
        let paths = AppPaths::resolve(Some(agentlog_home)).expect("resolve paths");

        let summary = sync_shell(&paths).await.expect("run sync");

        assert_eq!(summary.sessions_available, 2);
        assert_eq!(
            summary
                .provider_summaries
                .iter()
                .find(|provider| provider.provider == "cursor")
                .map(|provider| provider.sessions_available),
            Some(1)
        );
        assert_eq!(
            summary
                .provider_summaries
                .iter()
                .find(|provider| provider.provider == "kimi")
                .map(|provider| provider.sessions_available),
            Some(1)
        );
    }

    #[tokio::test]
    async fn one_sync_accepts_one_representative_source_from_each_provider() {
        let temporary = temporary_directory();
        let home = temporary.path().join("agentlog");
        let codex = temporary.path().join("codex");
        let claude = temporary.path().join("claude");
        let opencode = temporary.path().join("opencode");
        let gemini = temporary.path().join("gemini");
        let cursor = temporary.path().join("cursor");
        let kimi = temporary.path().join("kimi");
        fs::create_dir(&home).expect("home");
        let codex_source = codex.join("sessions/a/source.jsonl");
        fs::create_dir_all(codex_source.parent().expect("parent")).expect("Codex dirs");
        fs::write(&codex_source, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-session\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"codex visible\"}}\n").expect("Codex source");
        let claude_source = claude.join("projects/project/source.jsonl");
        fs::create_dir_all(claude_source.parent().expect("parent")).expect("Claude dirs");
        fs::write(&claude_source, "{\"type\":\"user\",\"sessionId\":\"claude-session\",\"message\":{\"role\":\"user\",\"content\":\"claude visible\"}}\n").expect("Claude source");
        fs::create_dir_all(&opencode).expect("OpenCode directory");
        let opencode_source = opencode.join("opencode.db");
        let opencode_pool = create_test_database(&opencode_source).await;
        let gemini_source = gemini.join("tmp/source.jsonl");
        fs::create_dir_all(gemini_source.parent().expect("parent")).expect("Gemini dirs");
        fs::write(&gemini_source, "{\"sessionId\":\"gemini-session\"}\n{\"type\":\"user\",\"content\":\"gemini visible\"}\n").expect("Gemini source");
        let cursor_source = cursor.join("projects/project/cursor-session/capture.jsonl");
        fs::create_dir_all(cursor_source.parent().expect("parent")).expect("Cursor dirs");
        fs::write(&cursor_source, "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"cursor visible\"}]}}\n").expect("Cursor source");
        let kimi_session = kimi.join("sessions/work/kimi-session");
        fs::create_dir_all(kimi_session.join("agents/main")).expect("Kimi dirs");
        let kimi_wire = kimi_session.join("agents/main/wire.jsonl");
        fs::write(
            &kimi_wire,
            "{\"type\":\"turn.prompt\",\"input\":[{\"text\":\"kimi visible\"}]}\n",
        )
        .expect("Kimi wire");
        let kimi_state = kimi_session.join("state.json");
        fs::write(&kimi_state, "{\"agents\":{\"main\":{}}}").expect("Kimi state");
        let kimi_index = kimi.join("session_index.jsonl");
        fs::write(
            &kimi_index,
            "{\"sessionId\":\"kimi-session\",\"sessionDir\":\"sessions/work/kimi-session\"}\n",
        )
        .expect("Kimi index");
        fs::write(home.join("config.toml"), format!("[providers]\ncodex_root = \"{}\"\nclaude_root = \"{}\"\nopencode_root = \"{}\"\ngemini_root = \"{}\"\ncursor_root = \"{}\"\nkimi_root = \"{}\"\n", codex.display(), claude.display(), opencode.display(), gemini.display(), cursor.display(), kimi.display())).expect("config");
        opencode_pool.close().await;
        let mut inputs = vec![
            codex_source,
            claude_source,
            opencode_source.clone(),
            gemini_source,
            cursor_source,
            kimi_wire,
            kimi_state,
            kimi_index,
        ];
        for suffix in ["-wal", "-shm"] {
            let sidecar =
                std::path::PathBuf::from(format!("{}{suffix}", opencode_source.display()));
            if sidecar.is_file() {
                inputs.push(sidecar);
            }
        }
        let before = inputs
            .iter()
            .map(|path| {
                (
                    fs::read(path).expect("bytes"),
                    fs::metadata(path).expect("metadata").len(),
                    fs::metadata(path).expect("metadata").modified().ok(),
                )
            })
            .collect::<Vec<_>>();
        let paths = AppPaths::resolve(Some(home)).expect("paths");
        let summary = sync_shell(&paths).await.expect("sync");
        assert_eq!(summary.sources_refreshed, 6);
        assert_eq!(summary.sources_failed, 0);
        assert_eq!(summary.sessions_available, 6);
        assert!(
            summary
                .provider_summaries
                .iter()
                .all(|provider| provider.candidate_sources == 1
                    && provider.refreshed_sources == 1
                    && provider.failed_sources == 0
                    && provider.sessions_available == 1)
        );
        for (path, (bytes, length, modified)) in inputs.iter().zip(before) {
            assert_eq!(fs::read(path).expect("bytes"), bytes);
            let metadata = fs::metadata(path).expect("metadata");
            assert_eq!(metadata.len(), length);
            assert_eq!(metadata.modified().ok(), modified);
        }
    }
}
