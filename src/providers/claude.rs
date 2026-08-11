//! Read-only Claude Code project transcript collection.
//!
//! Claude Code stores parent project transcripts and subagent transcripts below
//! its configuration root. This collector accepts an intentionally narrow
//! allowlist: visible user text, visible assistant text, and tool names only.
//! It never stores thinking, tool input or output, system reminders, images,
//! attachments, or raw JSON.

use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
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
// Claude's observed local JSONL contains valid records above 1 MiB (mostly
// provider-native tool payloads we do not retain). Four MiB is the smallest
// whole-MiB ceiling above the observed 3.14 MiB maximum.
const MAX_JSONL_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_KEY_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ITEMS_PER_SOURCE: usize = 2_000;
const MAX_SELECTED_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Aggregate result of one Claude collection pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClaudeScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
}

/// Claude-owned source locations and native discovery rules.
#[derive(Clone, Debug)]
pub struct ClaudeProvider {
    root: PathBuf,
}

impl ClaudeProvider {
    /// Resolves the Claude root using provider-native precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable absolute root can be resolved.
    pub fn resolve(configured: Option<&Path>) -> Result<Self, ProviderRootError> {
        Self::resolve_from(
            configured,
            env::var_os("CLAUDE_CONFIG_DIR"),
            env::var_os("HOME"),
        )
    }

    pub(crate) fn resolve_from(
        configured: Option<&Path>,
        claude_config_dir: Option<OsString>,
        os_home: Option<OsString>,
    ) -> Result<Self, ProviderRootError> {
        let root = if let Some(root) = configured {
            root.to_path_buf()
        } else if let Some(root) = claude_config_dir {
            PathBuf::from(root)
        } else {
            os_home
                .map(PathBuf::from)
                .filter(|home| !home.as_os_str().is_empty())
                .ok_or(ProviderRootError::ClaudeHomeUnavailable)?
                .join(".claude")
        };
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(ProviderRootError::InvalidClaudeRoot { path: root });
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
        ProviderId::Claude
    }

    fn sources(&self) -> std::io::Result<Vec<(ClaudeSource, SourceIdentity)>> {
        discover_sources(&self.root)
    }
}

/// Agentlog projection of Claude-native sources.
#[derive(Clone, Debug)]
pub struct ClaudeScanner {
    provider: ClaudeProvider,
}

impl ClaudeScanner {
    #[must_use]
    pub fn new(provider: ClaudeProvider) -> Self {
        Self { provider }
    }
}

impl ProviderScanner for ClaudeScanner {
    fn provider_id(&self) -> ProviderId {
        self.provider.id()
    }

    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
        let sources = self.provider.sources()?;
        let candidate_sources = u64::try_from(sources.len()).unwrap_or(u64::MAX);
        Ok(Box::new(ClaudeScan {
            candidate_sources,
            sources: sources.into_iter(),
        }))
    }
}

struct ClaudeScan {
    candidate_sources: u64,
    sources: std::vec::IntoIter<(ClaudeSource, SourceIdentity)>,
}

impl ProviderScan for ClaudeScan {
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
enum ClaudeSourceFormat {
    ProjectJsonl,
    SubagentJsonl,
}

impl ClaudeSourceFormat {
    const fn name(self) -> &'static str {
        match self {
            Self::ProjectJsonl => "project_jsonl",
            Self::SubagentJsonl => "subagent_jsonl",
        }
    }

    const fn execution_kind(self) -> &'static str {
        match self {
            Self::ProjectJsonl => "claude_project",
            Self::SubagentJsonl => "claude_subagent",
        }
    }
}

#[derive(Clone, Debug)]
struct ClaudeSource {
    path: PathBuf,
    format: ClaudeSourceFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFingerprint {
    size: u64,
    modified: Option<SystemTime>,
}

struct SnapshotFields {
    session_key: Option<String>,
    repository: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    started_at: Option<i64>,
    last_visible_event_at: Option<i64>,
    items: Vec<CatalogItem>,
    partial_reason: Option<PartialReason>,
}

#[derive(Clone, Copy)]
enum PartialReason {
    VisibleTextTruncated,
    ItemCapacity,
    SourceTextCapacity,
}

impl PartialReason {
    const fn message(self) -> &'static str {
        match self {
            Self::VisibleTextTruncated => "visible text exceeded the catalog item bound",
            Self::ItemCapacity => "source exceeded the catalog item bound",
            Self::SourceTextCapacity => "source exceeded the catalog text bound",
        }
    }
}

/// Discovers and imports Claude Code project and subagent JSONL files.
///
/// Each source file is parsed and replaced independently. If one file is
/// malformed, oversized, or changes during its read, its prior snapshot stays
/// available and other files continue to refresh.
///
/// # Errors
///
/// Returns an error when source discovery cannot inspect the configured root
/// or Agentlog cannot record a source result.
pub async fn collect_claude(
    root: &Path,
    pool: &SqlitePool,
) -> Result<ClaudeScanReport, StorageError> {
    let scanner = ClaudeScanner::new(ClaudeProvider::at_root(root.to_path_buf()));
    let report = scan_provider_with_pool(pool, &scanner).await?;
    Ok(ClaudeScanReport {
        candidate_sources: report.candidate_sources,
        refreshed_sources: report.refreshed_sources,
        partial_sources: report.partial_sources,
        failed_sources: report.failed_sources,
    })
}

fn discover_sources(root: &Path) -> std::io::Result<Vec<(ClaudeSource, SourceIdentity)>> {
    let mut sources = Vec::new();
    visit_project_sources(&root.join("projects"), &mut |path, format| {
        let source = ClaudeSource {
            path: path.to_path_buf(),
            format,
        };
        let identity = source_identity(&source);
        sources.push((source, identity));
        Ok(())
    })?;
    sources.sort_by(|left, right| left.0.path.cmp(&right.0.path));
    Ok(sources)
}

fn visit_project_sources(
    root: &Path,
    visit: &mut dyn FnMut(&Path, ClaudeSourceFormat) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
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
            visit_project_sources(&path, visit)?;
        } else if metadata.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        {
            let format = if path
                .ancestors()
                .skip(1)
                .any(|ancestor| ancestor.file_name().is_some_and(|name| name == "subagents"))
            {
                ClaudeSourceFormat::SubagentJsonl
            } else {
                ClaudeSourceFormat::ProjectJsonl
            };
            visit(&path, format)?;
        }
    }
    Ok(())
}

fn source_identity(source: &ClaudeSource) -> SourceIdentity {
    let canonical_locator = source
        .path
        .canonicalize()
        .unwrap_or_else(|_| source.path.clone())
        .to_string_lossy()
        .into_owned();
    SourceIdentity {
        provider: "claude",
        source_format: source.format.name(),
        canonical_locator,
    }
}

fn parse_source(
    source: &ClaudeSource,
    identity: SourceIdentity,
) -> Result<SourceSnapshot, &'static str> {
    let before = source_fingerprint(&source.path)?;
    if before.size > MAX_SOURCE_BYTES {
        return Err("source exceeds the supported size limit");
    }

    let mut session_key = None;
    let mut cwd = None;
    let mut repository = None;
    let mut model = None;
    let mut started_at = None;
    let mut last_visible_event_at = None;
    let mut items = Vec::new();
    let mut selected_text_bytes = 0;
    let mut partial_reason = None;
    let mut missing_identity = false;
    let mut multiple_session_ids = false;

    let malformed = stream_jsonl(&source.path, &mut |record| {
        let outcome = extract_record(&record);
        match outcome {
            RecordOutcome::Ignore => {}
            RecordOutcome::Visible {
                key,
                metadata,
                items: record_items,
            } => {
                let Some(key) = key else {
                    missing_identity = true;
                    return;
                };
                if let Some(existing) = &session_key {
                    if existing != &key {
                        multiple_session_ids = true;
                        return;
                    }
                } else {
                    session_key = Some(key);
                }
                let mut record_retained = false;
                for extracted in record_items {
                    if extracted.truncated {
                        note_partial(&mut partial_reason, PartialReason::VisibleTextTruncated);
                    }
                    record_retained |= push_bounded_item(
                        &mut items,
                        &mut selected_text_bytes,
                        &mut partial_reason,
                        extracted.item,
                    );
                }
                if record_retained {
                    if cwd.is_none() {
                        cwd = metadata.cwd;
                        repository = cwd.as_deref().and_then(repository_from_cwd);
                    }
                    if model.is_none() {
                        model = metadata.model;
                    }
                    started_at = min_timestamp(started_at, metadata.timestamp);
                    last_visible_event_at =
                        max_timestamp(last_visible_event_at, metadata.timestamp);
                }
            }
        }
    })?;

    if malformed {
        return Err("source contains malformed or oversized records");
    }
    if missing_identity {
        return Err("source contains visible transcript records without a session identity");
    }
    if multiple_session_ids {
        return Err("source contains multiple visible transcript session identities");
    }
    if source_fingerprint(&source.path)? != before {
        return Err("source changed while it was being collected");
    }

    Ok(finish_snapshot(
        identity,
        source.format,
        SnapshotFields {
            session_key,
            repository,
            cwd,
            model,
            started_at,
            last_visible_event_at,
            items,
            partial_reason,
        },
    ))
}

fn finish_snapshot(
    identity: SourceIdentity,
    source_format: ClaudeSourceFormat,
    fields: SnapshotFields,
) -> SourceSnapshot {
    let Some(session_key) = fields.session_key else {
        return SourceSnapshot {
            identity,
            diagnostic_status: "ok",
            diagnostic_message: None,
            sessions: Vec::new(),
        };
    };
    let title = fields.items.iter().find_map(|item| match item {
        CatalogItem::UserText(text) => Some(short_title(text)),
        CatalogItem::AssistantText(_) | CatalogItem::ToolMarker { .. } => None,
    });
    let sessions = contains_visible_text(&fields.items).then_some(CatalogSession {
        session_key,
        title,
        repository: fields.repository,
        cwd: fields.cwd,
        model: fields.model,
        execution_kind: Some(source_format.execution_kind().to_owned()),
        started_at: fields.started_at,
        last_visible_event_at: fields.last_visible_event_at,
        items: fields.items,
    });
    SourceSnapshot {
        identity,
        diagnostic_status: if fields.partial_reason.is_some() {
            "partial"
        } else {
            "ok"
        },
        diagnostic_message: fields.partial_reason.map(PartialReason::message),
        sessions: sessions.into_iter().collect(),
    }
}

fn source_fingerprint(path: &Path) -> Result<SourceFingerprint, &'static str> {
    let metadata = fs::metadata(path).map_err(|_| "could not read provider source")?;
    Ok(SourceFingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

struct RecordMetadata {
    cwd: Option<String>,
    model: Option<String>,
    timestamp: Option<i64>,
}

enum RecordOutcome {
    Ignore,
    Visible {
        key: Option<String>,
        metadata: RecordMetadata,
        items: Vec<ExtractedItem>,
    },
}

struct ExtractedItem {
    item: CatalogItem,
    truncated: bool,
}

fn extract_record(record: &Value) -> RecordOutcome {
    let Some(record_type) = record.get("type").and_then(Value::as_str) else {
        return RecordOutcome::Ignore;
    };
    if !matches!(record_type, "user" | "assistant") {
        return RecordOutcome::Ignore;
    }
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return RecordOutcome::Ignore;
    }
    let Some(message) = record.get("message").and_then(Value::as_object) else {
        return RecordOutcome::Ignore;
    };
    let expected_role = record_type;
    if message.get("role").and_then(Value::as_str) != Some(expected_role) {
        return RecordOutcome::Ignore;
    }
    let Some(content) = message.get("content") else {
        return RecordOutcome::Ignore;
    };
    let metadata = RecordMetadata {
        cwd: bounded_absolute_string(record.get("cwd"), MAX_SESSION_KEY_BYTES),
        model: (record_type == "assistant")
            .then(|| bounded_string(message.get("model"), MAX_SESSION_KEY_BYTES))
            .flatten(),
        timestamp: timestamp(record.get("timestamp")),
    };
    let key = bounded_string(record.get("sessionId"), MAX_SESSION_KEY_BYTES);

    match (record_type, content) {
        ("user", Value::String(text)) => visible_text(text, true, key, metadata),
        (_, Value::Array(parts)) => extract_content_part(record_type, parts, key, metadata),
        _ => RecordOutcome::Ignore,
    }
}

fn extract_content_part(
    record_type: &str,
    parts: &[Value],
    key: Option<String>,
    metadata: RecordMetadata,
) -> RecordOutcome {
    let mut items = Vec::new();
    for part in parts {
        let Some(part_type) = part.get("type").and_then(Value::as_str) else {
            continue;
        };
        match (record_type, part_type) {
            ("user" | "assistant", "text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && let Some(item) = visible_item(text, record_type == "user")
                {
                    items.push(item);
                }
            }
            ("assistant", "tool_use") => {
                if let Some(name) = bounded_string(part.get("name"), MAX_TOOL_NAME_BYTES) {
                    items.push(ExtractedItem {
                        item: CatalogItem::ToolMarker {
                            name,
                            status: Some("requested".to_owned()),
                        },
                        truncated: false,
                    });
                }
            }
            // Thinking, tool results, images, documents, and other provider-native
            // content are valid but deliberately outside the display allowlist.
            _ => {}
        }
    }
    if items.is_empty() {
        RecordOutcome::Ignore
    } else {
        RecordOutcome::Visible {
            key,
            metadata,
            items,
        }
    }
}

fn visible_text(
    text: &str,
    is_user: bool,
    key: Option<String>,
    metadata: RecordMetadata,
) -> RecordOutcome {
    let Some(item) = visible_item(text, is_user) else {
        return RecordOutcome::Ignore;
    };
    RecordOutcome::Visible {
        key,
        metadata,
        items: vec![item],
    }
}

fn visible_item(text: &str, is_user: bool) -> Option<ExtractedItem> {
    let text = text.trim();
    if text.is_empty() || (is_user && is_claude_control_payload(text)) {
        return None;
    }
    let truncated = text.len() > MAX_TEXT_BYTES;
    let text = if truncated {
        truncate_text(text, MAX_TEXT_BYTES)
    } else {
        text.to_owned()
    };
    Some(ExtractedItem {
        item: if is_user {
            CatalogItem::UserText(text)
        } else {
            CatalogItem::AssistantText(text)
        },
        truncated,
    })
}

fn is_claude_control_payload(text: &str) -> bool {
    // These are observed Claude Code injected records. The check deliberately
    // uses a finite provider-specific denylist instead of discarding arbitrary
    // XML/HTML that a user may have written in a normal prompt.
    [
        "system-reminder",
        "task-notification",
        "command-message",
        "command-name",
        "teammate-message",
        "local-command-stdout",
    ]
    .iter()
    .any(|tag| text.starts_with(&format!("<{tag}>")))
}

fn push_bounded_item(
    items: &mut Vec<CatalogItem>,
    selected_text_bytes: &mut usize,
    partial_reason: &mut Option<PartialReason>,
    mut item: CatalogItem,
) -> bool {
    if items.len() >= MAX_ITEMS_PER_SOURCE {
        note_partial(partial_reason, PartialReason::ItemCapacity);
        return false;
    }
    if let Some(text) = item_text_mut(&mut item) {
        if text.len() > MAX_TEXT_BYTES {
            *text = truncate_text(text, MAX_TEXT_BYTES);
            note_partial(partial_reason, PartialReason::VisibleTextTruncated);
        }
        let remaining = MAX_SELECTED_TEXT_BYTES.saturating_sub(*selected_text_bytes);
        if text.len() > remaining {
            if remaining == 0 {
                note_partial(partial_reason, PartialReason::SourceTextCapacity);
                return false;
            }
            *text = truncate_text(text, remaining);
            note_partial(partial_reason, PartialReason::SourceTextCapacity);
        }
        *selected_text_bytes += text.len();
    }
    items.push(item);
    true
}

fn note_partial(reason: &mut Option<PartialReason>, new_reason: PartialReason) {
    if reason.is_none() {
        *reason = Some(new_reason);
    }
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
        Some(value) if value.is_object() => handle(value),
        _ => *malformed = true,
    }
}

fn bounded_string(value: Option<&Value>, max_bytes: usize) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty() && text.len() <= max_bytes).then(|| text.to_owned())
}

fn bounded_absolute_string(value: Option<&Value>, max_bytes: usize) -> Option<String> {
    let value = bounded_string(value, max_bytes)?;
    Path::new(&value).is_absolute().then_some(value)
}

fn item_text_mut(item: &mut CatalogItem) -> Option<&mut String> {
    match item {
        CatalogItem::UserText(text) | CatalogItem::AssistantText(text) => Some(text),
        CatalogItem::ToolMarker { .. } => None,
    }
}

fn contains_visible_text(items: &[CatalogItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            CatalogItem::UserText(_) | CatalogItem::AssistantText(_)
        )
    })
}

fn repository_from_cwd(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn short_title(text: &str) -> String {
    const MAX_TITLE_BYTES: usize = 160;
    if text.len() <= MAX_TITLE_BYTES {
        return text.to_owned();
    }
    format!("{}…", truncate_text(text, MAX_TITLE_BYTES))
}

fn truncate_text(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
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
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .and_then(normalize_epoch)
            .or_else(|| parse_rfc3339_seconds(value)),
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

fn parse_rfc3339_seconds(value: &str) -> Option<i64> {
    // Claude Code writes RFC3339 timestamps. The MVP only needs the stable UTC
    // calendar prefix, avoiding an extra broad date/time dependency.
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
    let mut timezone = 19;
    if bytes.get(timezone) == Some(&b'.') {
        timezone += 1;
        while bytes.get(timezone).is_some_and(u8::is_ascii_digit) {
            timezone += 1;
        }
    }
    if bytes.get(timezone) != Some(&b'Z') || timezone + 1 != bytes.len() {
        return None;
    }
    days_from_civil(year, month, day)?
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
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

fn days_from_civil(mut year: i64, month: i64, day: i64) -> Option<i64> {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use sqlx::query_scalar;
    use tempfile::TempDir;

    use crate::storage::{open_database, read_catalog_preview};

    use super::{collect_claude, parse_rfc3339_seconds};

    fn write_source(root: &Path, relative: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = root.join("projects").join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create source directory");
        fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write source");
        path
    }

    fn user(session: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": session,
            "cwd": "/work/repo",
            "timestamp": "2026-07-26T12:00:00.000Z",
            "message": { "role": "user", "content": text },
        })
        .to_string()
    }

    fn assistant(session: &str, text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": session,
            "cwd": "/work/repo",
            "timestamp": "2026-07-26T12:01:00.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-test",
                "content": [{ "type": "text", "text": text }],
            },
        })
        .to_string()
    }

    #[tokio::test]
    async fn collects_parent_and_subagent_without_storing_private_content() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        write_source(
            &root,
            "project/parent.jsonl",
            &[
                &user("parent", "visible user request"),
                &assistant("parent", "visible assistant answer"),
                &assistant(
                    "parent",
                    "<system-reminder>assistant literal</system-reminder>",
                ),
                r#"{"type":"assistant","sessionId":"parent","message":{"role":"assistant","content":[{"type":"thinking","thinking":"PRIVATE_THINKING"},{"type":"tool_use","name":"Bash","input":{"command":"PRIVATE_TOOL_INPUT"}},{"type":"text","text":"later visible assistant text"}]}}"#,
                r#"{"type":"user","sessionId":"parent","isMeta":true,"message":{"role":"user","content":"PRIVATE_REMINDER"}}"#,
                r#"{"type":"user","sessionId":"parent","message":{"role":"user","content":"<system-reminder>PRIVATE_SYSTEM_REMINDER</system-reminder>"}}"#,
                r#"{"type":"user","sessionId":"parent","message":{"role":"user","content":[{"type":"tool_result","content":"PRIVATE_TOOL_OUTPUT"}]}}"#,
            ],
        );
        write_source(
            &root,
            "project/subagents/child.jsonl",
            &[
                &user("child", "subagent request"),
                &assistant("child", "subagent answer"),
            ],
        );
        let control_payloads = [
            "<system-reminder>PRIVATE_SYSTEM_REMINDER</system-reminder>",
            "<task-notification>PRIVATE_TASK_NOTIFICATION</task-notification>",
            "<command-message>PRIVATE_COMMAND_MESSAGE</command-message>",
            "<command-name>PRIVATE_COMMAND_NAME</command-name>",
            "<teammate-message>PRIVATE_TEAMMATE_MESSAGE</teammate-message>",
            "<local-command-stdout>PRIVATE_COMMAND_OUTPUT</local-command-stdout>",
        ];
        let control_records = control_payloads
            .iter()
            .map(|payload| user("controls", payload))
            .collect::<Vec<_>>();
        let control_refs = control_records
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        write_source(&root, "project/controls.jsonl", &control_refs);
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_claude(&root, &database)
            .await
            .expect("collect Claude");
        assert_eq!(report.candidate_sources, 3);
        assert_eq!(report.refreshed_sources, 3);
        let stored = query_scalar::<_, String>(
            "SELECT group_concat(COALESCE(content, tool_name), '|') FROM transcript_items",
        )
        .fetch_one(&database)
        .await
        .expect("read stored text");
        assert!(stored.contains("visible user request"));
        assert!(stored.contains("visible assistant answer"));
        assert!(stored.contains("assistant literal"));
        assert!(stored.contains("later visible assistant text"));
        assert!(stored.contains("Bash"));
        assert!(!stored.contains("PRIVATE_"));
        let subagent_count = query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sources WHERE source_format = 'subagent_jsonl'",
        )
        .fetch_one(&database)
        .await
        .expect("count subagents");
        assert_eq!(subagent_count, 1);
        database.close().await;
    }

    #[tokio::test]
    async fn source_failure_preserves_last_good_snapshot_and_never_changes_source_bytes() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        let source = write_source(
            &root,
            "project/parent.jsonl",
            &[
                &user("parent", "first request"),
                &assistant("parent", "first answer"),
            ],
        );
        let source_before = fs::read(&source).expect("read source before scan");
        let metadata_before = fs::metadata(&source).expect("source metadata before scan");
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        collect_claude(&root, &database).await.expect("first scan");
        assert_eq!(
            fs::read(&source).expect("read source after scan"),
            source_before
        );
        let metadata_after = fs::metadata(&source).expect("source metadata after scan");
        assert_eq!(metadata_after.len(), metadata_before.len());
        assert_eq!(
            metadata_after.modified().expect("source mtime after scan"),
            metadata_before
                .modified()
                .expect("source mtime before scan")
        );

        fs::write(&source, "{not json}\n").expect("break source");
        let report = collect_claude(&root, &database).await.expect("second scan");
        assert_eq!(report.failed_sources, 1);
        let session_id = query_scalar::<_, i64>("SELECT id FROM sessions")
            .fetch_one(&database)
            .await
            .expect("retained session");
        let preview = read_catalog_preview(&database, session_id, 80, 4 * 1024)
            .await
            .expect("retained preview");
        assert!(
            matches!(preview.items.first(), Some(crate::storage::CatalogItemView::UserText { content }) if content == "first request")
        );
        database.close().await;
    }

    #[tokio::test]
    async fn missing_session_identity_preserves_last_good_snapshot() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        let source = write_source(
            &root,
            "project/identity.jsonl",
            &[&user("known", "last good request")],
        );
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");
        collect_claude(&root, &database).await.expect("first scan");

        fs::write(
            &source,
            r#"{"type":"user","message":{"role":"user","content":"missing identity"}}"#,
        )
        .expect("remove session identity");
        let report = collect_claude(&root, &database).await.expect("second scan");
        assert_eq!(report.failed_sources, 1);
        let retained = query_scalar::<_, String>(
            "SELECT content FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&database)
        .await
        .expect("retained text");
        assert_eq!(retained, "last good request");
        database.close().await;
    }

    #[tokio::test]
    async fn malformed_source_does_not_block_a_different_good_source() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        write_source(
            &root,
            "project/good.jsonl",
            &[&user("good", "good source request")],
        );
        write_source(&root, "project/bad.jsonl", &["not json"]);
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_claude(&root, &database)
            .await
            .expect("collect mixed sources");
        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.failed_sources, 1);
        let retained = query_scalar::<_, String>(
            "SELECT content FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&database)
        .await
        .expect("good source snapshot");
        assert_eq!(retained, "good source request");
        database.close().await;
    }

    #[tokio::test]
    async fn bounds_are_partial_while_malformed_records_fail() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        let long = "x".repeat(128 * 1024 + 1);
        write_source(&root, "project/bounded.jsonl", &[&user("bounded", &long)]);
        write_source(&root, "project/broken.jsonl", &["not json"]);
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_claude(&root, &database)
            .await
            .expect("collect Claude");
        assert_eq!(report.partial_sources, 1);
        assert_eq!(report.failed_sources, 1);
        let length = query_scalar::<_, i64>(
            "SELECT length(content) FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&database)
        .await
        .expect("stored bounded text");
        assert_eq!(length, 128 * 1024);
        database.close().await;
    }

    #[tokio::test]
    async fn multiple_session_ids_in_one_physical_source_fail_without_combining_them() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        write_source(
            &root,
            "project/mixed.jsonl",
            &[
                &user("first", "first session request"),
                &assistant("second", "second session answer"),
            ],
        );
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        let report = collect_claude(&root, &database)
            .await
            .expect("collect Claude");
        assert_eq!(report.failed_sources, 1);
        let sessions = query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&database)
            .await
            .expect("count sessions");
        assert_eq!(sessions, 0);
        database.close().await;
    }

    #[tokio::test]
    async fn timestamp_tracks_only_items_retained_within_the_source_text_limit() {
        let temporary = TempDir::new().expect("temporary directory");
        let root = temporary.path().join("claude");
        let text = "x".repeat(128 * 1024);
        let mut records = Vec::new();
        for second in 0..17 {
            records.push(
                serde_json::json!({
                    "type": "user",
                    "sessionId": "bounded",
                    "timestamp": format!("2026-07-26T12:00:{second:02}Z"),
                    "message": { "role": "user", "content": text },
                })
                .to_string(),
            );
        }
        let record_refs = records.iter().map(String::as_str).collect::<Vec<_>>();
        write_source(&root, "project/bounded-timestamps.jsonl", &record_refs);
        let database = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("open catalog");

        collect_claude(&root, &database)
            .await
            .expect("collect Claude");
        let timestamp = query_scalar::<_, i64>("SELECT last_visible_event_at FROM sessions")
            .fetch_one(&database)
            .await
            .expect("read retained timestamp");
        assert_eq!(
            timestamp,
            parse_rfc3339_seconds("2026-07-26T12:00:15Z").expect("timestamp")
        );
        database.close().await;
    }

    #[test]
    fn parses_claude_rfc3339_timestamp() {
        assert_eq!(parse_rfc3339_seconds("1970-01-01T00:00:01.123Z"), Some(1));
        assert!(parse_rfc3339_seconds("not a timestamp").is_none());
    }
}
