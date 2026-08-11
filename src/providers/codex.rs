//! Read-only Codex history collection for the first Agentlog provider slice.
//!
//! This module deliberately recognizes only the current session JSONL shape and
//! Codex's legacy `history.jsonl`. It reads provider-owned files without
//! following symlinks and persists only visible user/assistant text plus
//! bounded tool markers.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use serde_json::Value;
use sqlx::SqlitePool;

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

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JSONL_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SESSION_KEY_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_ITEMS_PER_SESSION: usize = 2_000;
const MAX_SELECTED_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CANDIDATE_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SESSIONS_PER_LEGACY_SOURCE: usize = 1_000;
const MAX_ITEMS_PER_LEGACY_SOURCE: usize = 4_000;
const MAX_TOOL_NAME_BYTES: usize = 256;

/// Aggregate result of one Codex-only collection pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodexScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
}

/// Codex-owned source locations and native discovery rules.
#[derive(Clone, Debug)]
pub struct CodexProvider {
    root: PathBuf,
}

impl CodexProvider {
    /// Resolves the Codex root using provider-native precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable absolute root can be resolved.
    pub fn resolve(configured: Option<&Path>) -> Result<Self, ProviderRootError> {
        Self::resolve_from(configured, env::var_os("CODEX_HOME"), env::var_os("HOME"))
    }

    pub(crate) fn resolve_from(
        configured: Option<&Path>,
        codex_home: Option<OsString>,
        os_home: Option<OsString>,
    ) -> Result<Self, ProviderRootError> {
        let root = if let Some(root) = configured {
            root.to_path_buf()
        } else if let Some(root) = codex_home {
            PathBuf::from(root)
        } else {
            os_home
                .map(PathBuf::from)
                .filter(|home| !home.as_os_str().is_empty())
                .ok_or(ProviderRootError::CodexHomeUnavailable)?
                .join(".codex")
        };
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(ProviderRootError::InvalidCodexRoot { path: root });
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn at_root(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn sources(&self) -> std::io::Result<Vec<(CodexSource, SourceIdentity)>> {
        discover_sources(&self.root)
    }
}

/// Agentlog projection of Codex-native sources.
#[derive(Clone, Debug)]
pub struct CodexScanner {
    provider: CodexProvider,
}

impl CodexScanner {
    #[must_use]
    pub fn new(provider: CodexProvider) -> Self {
        Self { provider }
    }
}

impl ProviderScanner for CodexScanner {
    fn provider_id(&self) -> ProviderId {
        self.provider.id()
    }

    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
        let sources = self.provider.sources()?;
        let candidate_sources = u64::try_from(sources.len()).unwrap_or(u64::MAX);
        Ok(Box::new(CodexScan {
            candidate_sources,
            sources: sources.into_iter(),
        }))
    }
}

struct CodexScan {
    candidate_sources: u64,
    sources: std::vec::IntoIter<(CodexSource, SourceIdentity)>,
}

impl ProviderScan for CodexScan {
    fn candidate_sources(&self) -> u64 {
        self.candidate_sources
    }

    fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
        let outcome = self.sources.next().map(|(source, identity)| {
            match parse_source(&source, identity.clone()) {
                Ok(snapshot) => SourceOutcome::Accepted(snapshot),
                Err(message) => SourceOutcome::Failed { identity, message },
            }
        });
        Box::pin(async move { Ok(outcome) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexSourceFormat {
    CurrentSessionJsonl,
    LegacyHistoryJsonl,
}

impl CodexSourceFormat {
    const fn name(self) -> &'static str {
        match self {
            Self::CurrentSessionJsonl => "session_jsonl",
            Self::LegacyHistoryJsonl => "history_jsonl",
        }
    }
}

#[derive(Clone, Debug)]
struct CodexSource {
    path: PathBuf,
    format: CodexSourceFormat,
}

#[derive(Default)]
struct LegacySession {
    items: Vec<CatalogItem>,
    started_at: Option<i64>,
    last_visible_event_at: Option<i64>,
}

struct ExtractedItem {
    item: CatalogItem,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum PartialReason {
    VisibleTextBound,
    CurrentItemBound,
    CurrentCandidateBound,
    SelectedTextBound,
    LegacyMissingRequiredFields,
    LegacySessionBound,
    LegacyItemBound,
    LegacyTextBound,
}

impl PartialReason {
    const fn message(self) -> &'static str {
        match self {
            Self::VisibleTextBound => "visible text exceeded the catalog bound",
            Self::CurrentItemBound => "current session exceeded the catalog item bound",
            Self::CurrentCandidateBound => {
                "current session exceeded the bounded collection candidate limit"
            }
            Self::SelectedTextBound => "current session exceeded the catalog text bound",
            Self::LegacyMissingRequiredFields => {
                "legacy history record is missing a required visible field"
            }
            Self::LegacySessionBound => "legacy history exceeded the catalog session bound",
            Self::LegacyItemBound => "legacy history exceeded the catalog item bound",
            Self::LegacyTextBound => "legacy history exceeded the catalog text bound",
        }
    }
}

fn note_partial(reason: &mut Option<PartialReason>, new_reason: PartialReason) {
    if reason.is_none() {
        *reason = Some(new_reason);
    }
}

/// Discovers and imports the current Codex session logs and legacy history.
///
/// An individual source failure is diagnosed in the catalog and does not stop
/// the remaining source files. Only Agentlog's `SQLite` catalog is modified.
///
/// # Errors
///
/// Returns an error when discovery itself cannot inspect the configured root or
/// when Agentlog cannot persist a source result.
pub async fn collect_codex(
    root: &Path,
    pool: &SqlitePool,
) -> Result<CodexScanReport, StorageError> {
    let scanner = CodexScanner::new(CodexProvider::at_root(root.to_path_buf()));
    let report = scan_provider_with_pool(pool, &scanner).await?;
    Ok(CodexScanReport {
        candidate_sources: report.candidate_sources,
        refreshed_sources: report.refreshed_sources,
        partial_sources: report.partial_sources,
        failed_sources: report.failed_sources,
    })
}

fn discover_sources(root: &Path) -> std::io::Result<Vec<(CodexSource, SourceIdentity)>> {
    let mut sources = Vec::new();
    visit_jsonl_files(&root.join("sessions"), &mut |path| {
        let source = CodexSource {
            path: path.to_path_buf(),
            format: CodexSourceFormat::CurrentSessionJsonl,
        };
        let identity = source_identity(&source);
        sources.push((source, identity));
        Ok(())
    })?;

    let legacy = root.join("history.jsonl");
    if is_regular_file(&legacy)? {
        let source = CodexSource {
            path: legacy,
            format: CodexSourceFormat::LegacyHistoryJsonl,
        };
        let identity = source_identity(&source);
        sources.push((source, identity));
    }
    sources.sort_by(|left, right| left.0.path.cmp(&right.0.path));
    Ok(sources)
}

fn visit_jsonl_files(
    root: &Path,
    visit: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if root.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            visit(root)?;
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            visit_jsonl_files(&path, visit)?;
        } else if metadata.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        {
            visit(&path)?;
        }
    }
    Ok(())
}

fn is_regular_file(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn source_identity(source: &CodexSource) -> SourceIdentity {
    let canonical_locator = source
        .path
        .canonicalize()
        .unwrap_or_else(|_| source.path.clone())
        .to_string_lossy()
        .into_owned();
    SourceIdentity {
        provider: "codex",
        source_format: source.format.name(),
        canonical_locator,
    }
}

fn parse_source(
    source: &CodexSource,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let metadata = fs::metadata(&source.path).map_err(|_| "could not read provider source")?;
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err("source exceeds the supported size limit");
    }
    match source.format {
        CodexSourceFormat::CurrentSessionJsonl => parse_current_source(&source.path, identity),
        CodexSourceFormat::LegacyHistoryJsonl => parse_legacy_source(&source.path, identity),
    }
}

#[allow(clippy::too_many_lines)]
fn parse_current_source(
    path: &Path,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let mut session_key = None;
    let mut cwd = None;
    let mut model = None;
    let mut repository = None;
    let mut started_at = None;
    let mut first_record_at = None;
    let mut response_items = Vec::new();
    let mut response_text_bytes = 0;
    let mut response_dropped = false;
    let mut response_has_assistant_text = false;
    let mut event_user_items = Vec::new();
    let mut event_user_text_bytes = 0;
    let mut event_user_dropped = false;
    let mut event_assistant_items = Vec::new();
    let mut event_assistant_text_bytes = 0;
    let mut event_assistant_dropped = false;
    let mut item_order = 0_usize;
    let partial_reason = None;

    let malformed = stream_jsonl(path, &mut |value| {
        first_record_at = first_record_at.or(timestamp(value.get("timestamp")));
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            if session_key.is_none() {
                session_key = bounded_string(payload.get("id"), MAX_SESSION_KEY_BYTES);
            }
            if cwd.is_none() {
                cwd = bounded_string(payload.get("cwd"), MAX_TEXT_BYTES);
            }
            if repository.is_none() {
                repository = repository_name(payload.pointer("/git/repository_url"));
            }
            started_at = timestamp(value.get("timestamp").or_else(|| payload.get("timestamp")))
                .or(started_at);
            return;
        }
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            if cwd.is_none() {
                cwd = bounded_string(payload.get("cwd"), MAX_TEXT_BYTES);
            }
            if model.is_none() {
                model = bounded_string(payload.get("model"), MAX_TEXT_BYTES);
            }
            return;
        }
        let record_timestamp = timestamp(value.get("timestamp"));
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                let items = response_items_from_record(&value);
                for extracted in items {
                    let is_assistant_text = matches!(extracted.item, CatalogItem::AssistantText(_));
                    let retained = push_candidate_item(
                        &mut response_items,
                        &mut response_text_bytes,
                        item_order,
                        record_timestamp,
                        extracted,
                    );
                    response_dropped |= !retained;
                    response_has_assistant_text |= retained && is_assistant_text;
                    item_order += 1;
                }
            }
            Some("event_msg") => {
                let items = event_items_from_record(&value);
                for extracted in items {
                    let is_user_text = matches!(extracted.item, CatalogItem::UserText(_));
                    let retained = if is_user_text {
                        push_candidate_item(
                            &mut event_user_items,
                            &mut event_user_text_bytes,
                            item_order,
                            record_timestamp,
                            extracted,
                        )
                    } else {
                        push_candidate_item(
                            &mut event_assistant_items,
                            &mut event_assistant_text_bytes,
                            item_order,
                            record_timestamp,
                            extracted,
                        )
                    };
                    if !retained {
                        if is_user_text {
                            event_user_dropped = true;
                        } else {
                            event_assistant_dropped = true;
                        }
                    }
                    item_order += 1;
                }
            }
            _ => {}
        }
    })?;
    if malformed {
        return Err("source contains malformed or oversized records");
    }

    let session_key = session_key.ok_or("current session has no usable session metadata")?;
    Ok(finish_current_snapshot(
        identity,
        session_key,
        repository,
        cwd,
        model,
        started_at.or(first_record_at),
        response_has_assistant_text,
        response_dropped,
        event_user_dropped,
        event_assistant_dropped,
        response_items,
        event_user_items,
        event_assistant_items,
        partial_reason,
    ))
}

#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
fn finish_current_snapshot(
    identity: SourceIdentity,
    session_key: String,
    repository: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    started_at: Option<i64>,
    response_has_assistant_text: bool,
    response_dropped: bool,
    event_user_dropped: bool,
    event_assistant_dropped: bool,
    response_items: Vec<(usize, Option<i64>, ExtractedItem)>,
    event_user_items: Vec<(usize, Option<i64>, ExtractedItem)>,
    event_assistant_items: Vec<(usize, Option<i64>, ExtractedItem)>,
    mut partial_reason: Option<PartialReason>,
) -> SourceSnapshot {
    // `event_msg.user_message` is the user-visible Codex input. Response-item
    // user messages can contain injected environment or instruction context, so
    // they are never persisted. Response assistant text and tool markers are
    // canonical; `event_msg.agent_message` is only an old-log fallback.
    let mut selected_items = response_items;
    selected_items.extend(event_user_items);
    if !response_has_assistant_text {
        selected_items.extend(event_assistant_items);
    }
    selected_items.sort_by_key(|(order, _, _)| *order);
    if response_dropped
        || event_user_dropped
        || (!response_has_assistant_text && event_assistant_dropped)
    {
        note_partial(&mut partial_reason, PartialReason::CurrentCandidateBound);
    }
    if selected_items.len() > MAX_ITEMS_PER_SESSION {
        selected_items.truncate(MAX_ITEMS_PER_SESSION);
        note_partial(&mut partial_reason, PartialReason::CurrentItemBound);
    }
    let mut text_bytes = 0;
    let mut bounded_items = Vec::with_capacity(selected_items.len());
    for (order, timestamp, mut extracted) in selected_items {
        if let Some(text) = extracted_text_mut(&mut extracted.item) {
            let remaining = MAX_SELECTED_TEXT_BYTES.saturating_sub(text_bytes);
            if text.len() > remaining {
                if remaining == 0 {
                    note_partial(&mut partial_reason, PartialReason::SelectedTextBound);
                    continue;
                }
                *text = truncate_text(text, remaining);
                extracted.truncated = true;
                note_partial(&mut partial_reason, PartialReason::SelectedTextBound);
            }
            text_bytes += text.len();
        }
        if extracted.truncated {
            note_partial(&mut partial_reason, PartialReason::VisibleTextBound);
        }
        bounded_items.push((order, timestamp, extracted));
    }
    let last_visible_event_at = bounded_items
        .iter()
        .fold(None, |latest, (_, timestamp, _)| {
            max_timestamp(latest, *timestamp)
        });
    let items = bounded_items
        .into_iter()
        .map(|(_, _, extracted)| extracted.item)
        .collect::<Vec<_>>();
    let title = items.iter().find_map(|item| match item {
        CatalogItem::UserText(text) => Some(short_title(text)),
        CatalogItem::AssistantText(_) | CatalogItem::ToolMarker { .. } => None,
    });

    let sessions = contains_text(&items).then_some(CatalogSession {
        session_key,
        title,
        repository,
        cwd,
        model,
        execution_kind: None,
        started_at,
        last_visible_event_at,
        items,
    });

    SourceSnapshot {
        identity,
        diagnostic_status: if partial_reason.is_some() {
            "partial"
        } else {
            "ok"
        },
        diagnostic_message: partial_reason.map(PartialReason::message),
        sessions: sessions.into_iter().collect(),
    }
}

fn parse_legacy_source(
    path: &Path,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let mut sessions = BTreeMap::<String, LegacySession>::new();
    let mut item_count = 0;
    let mut text_bytes = 0;
    let mut partial_reason = None;

    let malformed = stream_jsonl(path, &mut |value| {
        let Some(session_key) = bounded_string(value.get("session_id"), MAX_SESSION_KEY_BYTES)
        else {
            note_partial(
                &mut partial_reason,
                PartialReason::LegacyMissingRequiredFields,
            );
            return;
        };
        let Some((mut text, text_partial)) = bounded_text(value.get("text")) else {
            note_partial(
                &mut partial_reason,
                PartialReason::LegacyMissingRequiredFields,
            );
            return;
        };
        if !sessions.contains_key(&session_key) && sessions.len() >= MAX_SESSIONS_PER_LEGACY_SOURCE
        {
            note_partial(&mut partial_reason, PartialReason::LegacySessionBound);
            return;
        }
        if item_count >= MAX_ITEMS_PER_LEGACY_SOURCE {
            note_partial(&mut partial_reason, PartialReason::LegacyItemBound);
            return;
        }
        let remaining = MAX_SELECTED_TEXT_BYTES.saturating_sub(text_bytes);
        if text.len() > remaining {
            if remaining == 0 {
                note_partial(&mut partial_reason, PartialReason::LegacyTextBound);
                return;
            }
            text = truncate_text(&text, remaining);
            note_partial(&mut partial_reason, PartialReason::LegacyTextBound);
        }
        let stored_text_bytes = text.len();
        let session = sessions.entry(session_key).or_default();
        if session.items.len() >= MAX_ITEMS_PER_SESSION {
            note_partial(&mut partial_reason, PartialReason::LegacyItemBound);
            return;
        }
        session.items.push(CatalogItem::UserText(text));
        text_bytes += stored_text_bytes;
        let record_timestamp = timestamp(value.get("ts").or_else(|| value.get("timestamp")));
        session.started_at = min_timestamp(session.started_at, record_timestamp);
        session.last_visible_event_at =
            max_timestamp(session.last_visible_event_at, record_timestamp);
        item_count += 1;
        if text_partial {
            note_partial(&mut partial_reason, PartialReason::VisibleTextBound);
        }
    })?;
    if malformed {
        return Err("source contains malformed or oversized records");
    }

    let sessions = sessions
        .into_iter()
        .map(|(session_key, session)| CatalogSession {
            title: session.items.iter().find_map(|item| match item {
                CatalogItem::UserText(text) => Some(short_title(text)),
                CatalogItem::AssistantText(_) | CatalogItem::ToolMarker { .. } => None,
            }),
            session_key,
            repository: None,
            cwd: None,
            model: None,
            execution_kind: Some("legacy_history".to_owned()),
            started_at: session.started_at,
            last_visible_event_at: session.last_visible_event_at,
            items: session.items,
        })
        .collect();

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

fn stream_jsonl(path: &Path, handle: &mut dyn FnMut(Value)) -> Result<bool, &'static str> {
    let file = File::open(path).map_err(|_| "could not read provider source")?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::with_capacity(8 * 1024);
    let mut chunk = [0_u8; 8 * 1024];
    let mut malformed = false;
    let mut overlong = false;

    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|_| "could not read provider source")?;
        if read == 0 {
            if !line.is_empty() || overlong {
                process_jsonl_line(&line, overlong, handle, &mut malformed);
            }
            return Ok(malformed);
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                process_jsonl_line(&line, overlong, handle, &mut malformed);
                line.clear();
                overlong = false;
            } else if line.len() < MAX_JSONL_RECORD_BYTES {
                line.push(*byte);
            } else {
                overlong = true;
            }
        }
    }
}

fn process_jsonl_line(
    line: &[u8],
    overlong: bool,
    handle: &mut dyn FnMut(Value),
    malformed: &mut bool,
) {
    if overlong || line.is_empty() {
        *malformed = true;
        return;
    }
    match std::str::from_utf8(line)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    {
        Some(value) => handle(value),
        None => *malformed = true,
    }
}

fn response_items_from_record(value: &Value) -> Vec<ExtractedItem> {
    let payload = value.get("payload").unwrap_or(&Value::Null);
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => message_items(payload),
        Some("function_call" | "custom_tool_call") => tool_marker(payload),
        _ => Vec::new(),
    }
}

fn message_items(payload: &Value) -> Vec<ExtractedItem> {
    let role = payload.get("role").and_then(Value::as_str);
    let Some(item_kind) = (match role {
        Some("assistant") => Some("assistant"),
        _ => None,
    }) else {
        return Vec::new();
    };
    let Some(parts) = payload.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for part in parts {
        if !matches!(
            part.get("type").and_then(Value::as_str),
            Some("input_text" | "output_text" | "text")
        ) {
            continue;
        }
        let Some((text, text_partial)) = bounded_text(part.get("text")) else {
            continue;
        };
        items.push(ExtractedItem {
            item: match item_kind {
                "assistant" => CatalogItem::AssistantText(text),
                _ => unreachable!("role is closed above"),
            },
            truncated: text_partial,
        });
    }
    items
}

fn event_items_from_record(value: &Value) -> Vec<ExtractedItem> {
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let role = match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => "user",
        Some("agent_message") => "assistant",
        _ => return Vec::new(),
    };
    let Some((text, partial)) = bounded_text(payload.get("message")) else {
        return Vec::new();
    };
    let item = match role {
        "user" => CatalogItem::UserText(text),
        "assistant" => CatalogItem::AssistantText(text),
        _ => unreachable!("role is closed above"),
    };
    vec![ExtractedItem {
        item,
        truncated: partial,
    }]
}

fn tool_marker(payload: &Value) -> Vec<ExtractedItem> {
    let Some(name) = bounded_string(payload.get("name"), MAX_TOOL_NAME_BYTES) else {
        return Vec::new();
    };
    let status = bounded_string(payload.get("status"), MAX_TOOL_NAME_BYTES);
    vec![ExtractedItem {
        item: CatalogItem::ToolMarker { name, status },
        truncated: false,
    }]
}

fn bounded_string(value: Option<&Value>, max_bytes: usize) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty() && text.len() <= max_bytes).then(|| text.to_owned())
}

fn bounded_text(value: Option<&Value>) -> Option<(String, bool)> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= MAX_TEXT_BYTES {
        return Some((text.to_owned(), false));
    }
    Some((truncate_text(text, MAX_TEXT_BYTES), true))
}

fn truncate_text(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn short_title(text: &str) -> String {
    const MAX_TITLE_BYTES: usize = 160;
    if text.len() <= MAX_TITLE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_TITLE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn repository_name(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?.split(['?', '#']).next()?;
    let name = raw.rsplit('/').next()?.trim_end_matches(".git").trim();
    (!name.is_empty() && !name.contains('@') && name.len() <= MAX_SESSION_KEY_BYTES)
        .then(|| name.to_owned())
}

fn contains_text(items: &[CatalogItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            CatalogItem::UserText(_) | CatalogItem::AssistantText(_)
        )
    })
}

fn push_candidate_item(
    items: &mut Vec<(usize, Option<i64>, ExtractedItem)>,
    text_bytes: &mut usize,
    order: usize,
    timestamp: Option<i64>,
    item: ExtractedItem,
) -> bool {
    let item_text_bytes = extracted_text_bytes(&item.item);
    if items.len() >= MAX_ITEMS_PER_SESSION
        || text_bytes.saturating_add(item_text_bytes) > MAX_CANDIDATE_TEXT_BYTES
    {
        return false;
    }
    items.push((order, timestamp, item));
    *text_bytes += item_text_bytes;
    true
}

fn extracted_text_bytes(item: &CatalogItem) -> usize {
    match item {
        CatalogItem::UserText(text) | CatalogItem::AssistantText(text) => text.len(),
        CatalogItem::ToolMarker { .. } => 0,
    }
}

fn extracted_text_mut(item: &mut CatalogItem) -> Option<&mut String> {
    match item {
        CatalogItem::UserText(text) | CatalogItem::AssistantText(text) => Some(text),
        CatalogItem::ToolMarker { .. } => None,
    }
}

fn min_timestamp(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (current, candidate) => current.or(candidate),
    }
}

fn max_timestamp(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (current, candidate) => current.or(candidate),
    }
}

fn timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().and_then(normalize_epoch),
        Value::String(text) => text
            .parse::<i64>()
            .ok()
            .and_then(normalize_epoch)
            .or_else(|| parse_rfc3339_epoch(text)),
        _ => None,
    }
}

fn normalize_epoch(value: i64) -> Option<i64> {
    match value {
        value if value >= 1_000_000_000_000 => Some(value / 1_000),
        value if value >= 0 => Some(value),
        _ => None,
    }
}

fn parse_rfc3339_epoch(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = i64::from(decimal(&bytes[0..4])?);
    let month = i64::from(decimal(&bytes[5..7])?);
    let day = i64::from(decimal(&bytes[8..10])?);
    let hour = i64::from(decimal(&bytes[11..13])?);
    let minute = i64::from(decimal(&bytes[14..16])?);
    let second = i64::from(decimal(&bytes[17..19])?);
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut timezone_index = 19;
    if bytes.get(timezone_index) == Some(&b'.') {
        timezone_index += 1;
        while bytes.get(timezone_index).is_some_and(u8::is_ascii_digit) {
            timezone_index += 1;
        }
    }
    let offset_seconds = match bytes.get(timezone_index) {
        Some(b'Z' | b'z') if timezone_index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if timezone_index + 6 == bytes.len()
                && bytes.get(timezone_index + 3) == Some(&b':') =>
        {
            let hours = i64::from(decimal(&bytes[timezone_index + 1..timezone_index + 3])?);
            let minutes = i64::from(decimal(&bytes[timezone_index + 4..timezone_index + 6])?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3_600 + minutes * 60;
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };
    days_from_civil(year, month, day)?
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(offset_seconds)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

// Gregorian civil date to days since the Unix epoch. This stays local so the
// collector does not need a broad time dependency just to normalize log fields.
fn days_from_civil(mut year: i64, month: i64, day: i64) -> Option<i64> {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, fs, path::Path};

    use serde_json::Value;
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use crate::{
        providers::{ProviderScanner, SourceOutcome},
        storage::{
            CatalogItem, CatalogSession, SourceIdentity, SourceSnapshot, open_database,
            replace_source_snapshot,
        },
    };

    use super::{
        CodexProvider, CodexScanner, CodexSource, CodexSourceFormat, MAX_ITEMS_PER_SESSION,
        MAX_SELECTED_TEXT_BYTES, MAX_TEXT_BYTES, collect_codex, parse_current_source,
        source_identity, timestamp,
    };

    fn temporary() -> TempDir {
        TempDir::new().expect("temporary directory")
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, content).expect("write fixture");
    }

    async fn database(temporary: &TempDir) -> SqlitePool {
        open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog")
    }

    #[tokio::test]
    async fn discovery_identity_survives_a_delete_before_source_read() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        let source = root.join("sessions/project/session.jsonl");
        write(
            &source,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\"}}\n",
        );
        let expected_locator = source
            .canonicalize()
            .expect("canonical source before race")
            .to_string_lossy()
            .into_owned();
        let scanner = CodexScanner::new(CodexProvider::at_root(root));
        let mut scan = scanner.start().expect("complete discovery");

        fs::remove_file(&source).expect("simulate discovery-to-read delete");
        let outcome = scan
            .next_outcome()
            .await
            .expect("source outcome")
            .expect("discovered source");

        match outcome {
            SourceOutcome::Failed { identity, .. } => {
                assert_eq!(identity.canonical_locator, expected_locator);
            }
            SourceOutcome::Accepted(_) => panic!("deleted source must fail"),
        }
    }

    #[tokio::test]
    async fn imports_current_and_legacy_sources_without_exposing_private_record_fields() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        write(
            &root.join("sessions/2026/current.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\",\"cwd\":\"/work\",\"git\":{\"repository_url\":\"https://token@github.example/org/private-repo.git?secret=value#fragment\"}}}\n",
                "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-test\"}}\n",
                "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>injected\"},{\"type\":\"input_image\",\"image_url\":\"private-image\"}]}}\n",
                "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"visible request\"}}\n",
                "{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"visible answer\"},{\"type\":\"reasoning\",\"text\":\"private reasoning\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"private tool input\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"private tool output\"}}\n"
            ),
        );
        write(
            &root.join("history.jsonl"),
            "{\"session_id\":\"legacy-id\",\"text\":\"legacy visible request\",\"secret\":\"do not store\"}\n",
        );
        let pool = database(&temporary).await;

        let report = collect_codex(&root, &pool).await.expect("collect Codex");
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT content FROM transcript_items WHERE content IS NOT NULL ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("read catalog text");
        let tools = sqlx::query_scalar::<_, String>(
            "SELECT tool_name FROM transcript_items WHERE item_kind = 'tool_marker'",
        )
        .fetch_all(&pool)
        .await
        .expect("read tool markers");
        let session_fields = sqlx::query_as::<_, (String, String, i64, i64)>(
            "SELECT repository, model, started_at, last_visible_event_at FROM sessions WHERE session_key = 'current-id'",
        )
        .fetch_one(&pool)
        .await
        .expect("read session fields");

        assert_eq!(report.refreshed_sources, 2);
        assert_eq!(
            rows,
            [
                "legacy visible request",
                "visible request",
                "visible answer"
            ]
        );
        assert_eq!(tools, ["shell"]);
        assert_eq!(session_fields.0, "private-repo");
        assert_eq!(session_fields.1, "gpt-test");
        assert_eq!(session_fields.2, 1_767_323_045);
        assert_eq!(session_fields.3, 1_767_323_048);
        let joined = rows.join(" ");
        for forbidden in [
            "private-image",
            "private reasoning",
            "private tool input",
            "private tool output",
            "do not store",
            "<environment_context>",
        ] {
            assert!(!joined.contains(forbidden));
        }
        pool.close().await;
    }

    #[test]
    fn response_items_win_over_event_message_mirrors() {
        let temporary = temporary();
        let path = temporary.path().join("current.jsonl");
        write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"same message\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"same message\"}]}}\n"
            ),
        );
        let source = CodexSource {
            path,
            format: CodexSourceFormat::CurrentSessionJsonl,
        };
        let snapshot =
            parse_current_source(&source.path, source_identity(&source)).expect("parse source");

        assert_eq!(
            snapshot.sessions[0].items,
            [CatalogItem::UserText("same message".to_owned())]
        );
    }

    #[test]
    fn response_items_keep_text_and_tool_marker_record_order() {
        let temporary = temporary();
        let path = temporary.path().join("current.jsonl");
        write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"first\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"third\"}]}}\n"
            ),
        );
        let source = CodexSource {
            path,
            format: CodexSourceFormat::CurrentSessionJsonl,
        };
        let snapshot =
            parse_current_source(&source.path, source_identity(&source)).expect("parse source");

        assert_eq!(
            snapshot.sessions[0].items,
            [
                CatalogItem::AssistantText("first".to_owned()),
                CatalogItem::ToolMarker {
                    name: "shell".to_owned(),
                    status: None,
                },
                CatalogItem::AssistantText("third".to_owned()),
            ]
        );
    }

    #[test]
    fn event_fallback_keeps_its_order_with_response_tool_markers() {
        let temporary = temporary();
        let path = temporary.path().join("current.jsonl");
        write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"second\"}}\n"
            ),
        );
        let source = CodexSource {
            path,
            format: CodexSourceFormat::CurrentSessionJsonl,
        };
        let snapshot =
            parse_current_source(&source.path, source_identity(&source)).expect("parse source");

        assert_eq!(
            snapshot.sessions[0].items,
            [
                CatalogItem::ToolMarker {
                    name: "shell".to_owned(),
                    status: None,
                },
                CatalogItem::UserText("second".to_owned()),
            ]
        );
    }

    #[test]
    fn capped_response_candidates_do_not_hide_event_fallback_text() {
        let temporary = temporary();
        let path = temporary.path().join("current.jsonl");
        let mut fixture = String::from(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"fallback\"}}\n",
        );
        for _ in 0..MAX_ITEMS_PER_SESSION {
            fixture.push_str(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\"}}\n",
            );
        }
        fixture.push_str(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"dropped\"}]}}\n",
        );
        write(&path, &fixture);
        let source = CodexSource {
            path,
            format: CodexSourceFormat::CurrentSessionJsonl,
        };
        let snapshot =
            parse_current_source(&source.path, source_identity(&source)).expect("parse source");

        assert_eq!(snapshot.diagnostic_status, "partial");
        assert_eq!(
            snapshot.sessions[0].items[0],
            CatalogItem::UserText("fallback".to_owned())
        );
    }

    #[tokio::test]
    async fn malformed_and_oversized_records_preserve_a_last_good_snapshot() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        let path = root.join("sessions/current.jsonl");
        write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"visible\"}}\n",
        );
        let pool = database(&temporary).await;
        let first = collect_codex(&root, &pool).await.expect("good source");
        assert_eq!(first.refreshed_sources, 1);

        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"current-id\"}}\nnot-json\n",
        )
        .expect("malformed fixture");
        let malformed = collect_codex(&root, &pool)
            .await
            .expect("malformed source is diagnosed");
        assert_eq!(malformed.failed_sources, 1);

        fs::write(&path, vec![b'x'; 64 * 1024 * 1024 + 1]).expect("oversized fixture");
        let second = collect_codex(&root, &pool)
            .await
            .expect("source failure is diagnosed");
        let sessions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .expect("count sessions");
        let status = sqlx::query_scalar::<_, String>("SELECT diagnostic_status FROM sources")
            .fetch_one(&pool)
            .await
            .expect("source status");

        assert_eq!(second.failed_sources, 1);
        assert_eq!(sessions, 1);
        assert_eq!(status, "error");
        pool.close().await;
    }

    #[tokio::test]
    async fn failure_of_one_source_does_not_block_another_or_modify_inputs() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        let good = root.join("sessions/a/good.jsonl");
        let bad = root.join("sessions/b/bad.jsonl");
        write(
            &good,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"visible\"}}\n",
        );
        write(&bad, "{\"type\":\"session_meta\",\"payload\":{}}\n");
        let before_good = fs::read(&good).expect("read good before");
        let before_bad = fs::read(&bad).expect("read bad before");
        let before_good_metadata = fs::metadata(&good).expect("good metadata before");
        let before_bad_metadata = fs::metadata(&bad).expect("bad metadata before");
        let pool = database(&temporary).await;

        let report = collect_codex(&root, &pool).await.expect("collect sources");
        let sessions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .expect("count sessions");

        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.failed_sources, 1);
        assert_eq!(sessions, 1);
        assert_eq!(fs::read(&good).expect("read good after"), before_good);
        assert_eq!(fs::read(&bad).expect("read bad after"), before_bad);
        let after_good_metadata = fs::metadata(&good).expect("good metadata after");
        let after_bad_metadata = fs::metadata(&bad).expect("bad metadata after");
        assert_eq!(after_good_metadata.len(), before_good_metadata.len());
        assert_eq!(after_bad_metadata.len(), before_bad_metadata.len());
        assert_eq!(
            after_good_metadata.modified().expect("good mtime after"),
            before_good_metadata.modified().expect("good mtime before")
        );
        assert_eq!(
            after_bad_metadata.modified().expect("bad mtime after"),
            before_bad_metadata.modified().expect("bad mtime before")
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn bounded_visible_text_replaces_snapshot_with_a_partial_diagnostic() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        let path = root.join("sessions/current.jsonl");
        let long_text = "x".repeat(MAX_TEXT_BYTES + 1);
        write(
            &path,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"current-id\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{long_text}\"}}}}\n"
            ),
        );
        let pool = database(&temporary).await;

        let report = collect_codex(&root, &pool).await.expect("bounded scan");
        let stored_length = sqlx::query_scalar::<_, i64>(
            "SELECT length(content) FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&pool)
        .await
        .expect("stored text length");
        let status = sqlx::query_scalar::<_, String>("SELECT diagnostic_status FROM sources")
            .fetch_one(&pool)
            .await
            .expect("source status");

        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.partial_sources, 1);
        assert_eq!(
            stored_length,
            i64::try_from(MAX_TEXT_BYTES).expect("fits i64")
        );
        assert_eq!(status, "partial");
        pool.close().await;
    }

    #[tokio::test]
    async fn legacy_record_missing_required_fields_is_partial_and_diagnosed() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        write(
            &root.join("history.jsonl"),
            "{\"session_id\":\"legacy-id\",\"text\":\"visible\",\"ts\":1700000000}\n{\"session_id\":\"missing-text\"}\n",
        );
        let pool = database(&temporary).await;

        let report = collect_codex(&root, &pool).await.expect("collect legacy");
        let status = sqlx::query_scalar::<_, String>("SELECT diagnostic_status FROM sources")
            .fetch_one(&pool)
            .await
            .expect("source status");
        let sessions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .expect("session count");

        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.partial_sources, 1);
        assert_eq!(status, "partial");
        assert_eq!(sessions, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn legacy_source_text_is_bounded_to_two_mebibytes() {
        let temporary = temporary();
        let root = temporary.path().join("codex");
        let text = "x".repeat(MAX_TEXT_BYTES);
        let mut fixture = String::new();
        for index in 0..=(MAX_SELECTED_TEXT_BYTES / MAX_TEXT_BYTES) {
            writeln!(
                fixture,
                "{{\"session_id\":\"legacy-{index}\",\"text\":\"{text}\"}}"
            )
            .expect("format legacy fixture");
        }
        write(&root.join("history.jsonl"), &fixture);
        let pool = database(&temporary).await;

        let report = collect_codex(&root, &pool).await.expect("collect legacy");
        let stored_bytes = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(length(CAST(content AS BLOB))), 0) FROM transcript_items",
        )
        .fetch_one(&pool)
        .await
        .expect("stored bytes");
        let status = sqlx::query_scalar::<_, String>("SELECT diagnostic_status FROM sources")
            .fetch_one(&pool)
            .await
            .expect("source status");

        assert_eq!(report.partial_sources, 1);
        assert_eq!(status, "partial");
        assert_eq!(
            stored_bytes,
            i64::try_from(MAX_SELECTED_TEXT_BYTES).expect("fits i64")
        );
        pool.close().await;
    }

    #[test]
    fn timestamps_reject_invalid_calendar_dates() {
        assert_eq!(
            timestamp(Some(&Value::String("2026-02-29T00:00:00Z".to_owned()))),
            None
        );
        assert_eq!(
            timestamp(Some(&Value::String("2024-02-29T00:00:00Z".to_owned()))),
            Some(1_709_164_800)
        );
    }

    #[tokio::test]
    async fn source_identity_is_scoped_to_its_own_file() {
        let temporary = temporary();
        let pool = database(&temporary).await;
        let first = SourceSnapshot {
            identity: SourceIdentity {
                provider: "codex",
                source_format: "session_jsonl",
                canonical_locator: "one".to_owned(),
            },
            diagnostic_status: "ok",
            diagnostic_message: None,
            sessions: vec![CatalogSession {
                session_key: "same-id".to_owned(),
                title: None,
                repository: None,
                cwd: None,
                model: None,
                execution_kind: None,
                started_at: None,
                last_visible_event_at: None,
                items: Vec::new(),
            }],
        };
        let mut second = first.clone();
        second.identity.canonical_locator = "two".to_owned();
        replace_source_snapshot(&pool, &first)
            .await
            .expect("first source");
        replace_source_snapshot(&pool, &second)
            .await
            .expect("second source");
        let sessions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .expect("count sessions");
        assert_eq!(sessions, 2);
        pool.close().await;
    }
}
