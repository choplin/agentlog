//! Read-only Kimi Code session-index and wire JSONL collection.

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
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_TOOL_BYTES: usize = 256;
const MAX_ITEMS: usize = 2_000;
const MAX_TEXT_TOTAL: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KimiScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
}

/// Kimi-owned source locations and native discovery rules.
#[derive(Clone, Debug)]
pub struct KimiProvider {
    root: PathBuf,
}

impl KimiProvider {
    /// Resolves the Kimi root using provider-native precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable absolute root can be resolved.
    pub fn resolve(configured: Option<&Path>) -> Result<Self, ProviderRootError> {
        Self::resolve_from(
            configured,
            env::var_os("KIMI_CODE_HOME"),
            env::var_os("HOME"),
        )
    }

    pub(crate) fn resolve_from(
        configured: Option<&Path>,
        kimi_code_home: Option<OsString>,
        os_home: Option<OsString>,
    ) -> Result<Self, ProviderRootError> {
        let root = if let Some(root) = configured {
            root.to_path_buf()
        } else if let Some(root) = kimi_code_home {
            PathBuf::from(root)
        } else {
            os_home
                .map(PathBuf::from)
                .filter(|home| !home.as_os_str().is_empty())
                .ok_or(ProviderRootError::KimiHomeUnavailable)?
                .join(".kimi-code")
        };
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(ProviderRootError::InvalidKimiRoot { path: root });
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
        ProviderId::Kimi
    }

    fn sources(&self) -> std::io::Result<Vec<(KimiSource, SourceIdentity)>> {
        discover(&self.root)
    }
}

/// Agentlog projection of Kimi-native sources.
#[derive(Clone, Debug)]
pub struct KimiScanner {
    provider: KimiProvider,
}

impl KimiScanner {
    #[must_use]
    pub fn new(provider: KimiProvider) -> Self {
        Self { provider }
    }
}

impl ProviderScanner for KimiScanner {
    fn provider_id(&self) -> ProviderId {
        self.provider.id()
    }

    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
        let sources = self.provider.sources()?;
        let candidate_sources = u64::try_from(sources.len()).unwrap_or(u64::MAX);
        Ok(Box::new(KimiScan {
            root: self.provider.root().to_path_buf(),
            candidate_sources,
            sources: sources.into_iter(),
            index: parse_index(self.provider.root()),
        }))
    }
}

struct KimiScan {
    root: PathBuf,
    candidate_sources: u64,
    sources: std::vec::IntoIter<(KimiSource, SourceIdentity)>,
    index: Result<IndexSnapshot, &'static str>,
}

impl ProviderScan for KimiScan {
    fn candidate_sources(&self) -> u64 {
        self.candidate_sources
    }

    fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
        let outcome = self.sources.next().map(|(source, identity)| {
            let parsed = match &self.index {
                Ok(index) => parse(&self.root, &source, identity.clone(), index),
                Err(message) => Err(*message),
            };
            match parsed {
                Ok(snapshot) => SourceOutcome::Accepted(snapshot),
                Err(message) => SourceOutcome::Failed { identity, message },
            }
        });
        Box::pin(async move { Ok(outcome) })
    }
}

#[derive(Clone, Debug)]
struct KimiSource {
    wire: PathBuf,
    session_dir: PathBuf,
    session_key: String,
    agent_key: String,
}

#[derive(Clone, Debug)]
struct IndexEntry {
    session_key: String,
    session_dir: PathBuf,
}

struct IndexSnapshot {
    entries: Vec<IndexEntry>,
    fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Fingerprint {
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct InputFingerprint {
    index: Fingerprint,
    state: Fingerprint,
    wire: Fingerprint,
}

#[derive(Clone, Copy)]
enum PartialReason {
    Text,
    Items,
    TotalText,
}

impl PartialReason {
    const fn message(self) -> &'static str {
        match self {
            Self::Text => "visible text exceeded the catalog item bound",
            Self::Items => "source exceeded the catalog item bound",
            Self::TotalText => "source exceeded the catalog text bound",
        }
    }
}

/// Collects supported Kimi Code agent wire sources below one configuration root.
///
/// # Errors
///
/// Returns an error when Agentlog cannot record source outcomes.
pub async fn collect_kimi(root: &Path, pool: &SqlitePool) -> Result<KimiScanReport, StorageError> {
    let scanner = KimiScanner::new(KimiProvider::at_root(root.to_path_buf()));
    let report = scan_provider_with_pool(pool, &scanner).await?;
    Ok(KimiScanReport {
        candidate_sources: report.candidate_sources,
        refreshed_sources: report.refreshed_sources,
        partial_sources: report.partial_sources,
        failed_sources: report.failed_sources,
    })
}

fn discover(root: &Path) -> std::io::Result<Vec<(KimiSource, SourceIdentity)>> {
    let mut sources = Vec::new();
    visit(&root.join("sessions"), &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) == Some("wire.jsonl")
            && let Some(source) = wire_parts(root, path)
        {
            let identity = identity(&source);
            sources.push((source, identity));
        }
        Ok(())
    })?;
    sources.sort_by(|left, right| left.0.wire.cmp(&right.0.wire));
    Ok(sources)
}

fn wire_parts(root: &Path, wire: &Path) -> Option<KimiSource> {
    let components = wire
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    if components.len() != 6
        || components[0] != "sessions"
        || components[3] != "agents"
        || components[5] != "wire.jsonl"
    {
        return None;
    }
    let session_key = bounded_str(components[2], MAX_KEY_BYTES)?;
    let agent_key = bounded_str(components[4], MAX_KEY_BYTES)?;
    let session_dir = root
        .join("sessions")
        .join(components[1])
        .join(components[2]);
    let session_dir = session_dir.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    session_dir.starts_with(root).then_some(KimiSource {
        wire: wire.to_path_buf(),
        session_dir,
        session_key,
        agent_key,
    })
}

fn visit(
    root: &Path,
    callback: &mut dyn FnMut(&Path) -> std::io::Result<()>,
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
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            visit(&path, callback)?;
        } else if metadata.is_file() {
            callback(&path)?;
        }
    }
    Ok(())
}

fn identity(source: &KimiSource) -> SourceIdentity {
    SourceIdentity {
        provider: "kimi",
        source_format: "agent_wire_jsonl",
        canonical_locator: source
            .wire
            .canonicalize()
            .unwrap_or_else(|_| source.wire.clone())
            .to_string_lossy()
            .into_owned(),
    }
}

fn parse_index(root: &Path) -> Result<IndexSnapshot, &'static str> {
    let path = root.join("session_index.jsonl");
    let before = fingerprint(&path)?;
    if before.size > MAX_SOURCE_BYTES {
        return Err("Kimi session index exceeds the supported size limit");
    }
    let root = root
        .canonicalize()
        .map_err(|_| "could not read Kimi source root")?;
    let mut entries = Vec::new();
    let mut malformed = false;
    stream(
        &path,
        &mut |record| {
            let Some(session_key) = bounded(record.get("sessionId"), MAX_KEY_BYTES) else {
                return;
            };
            let Some(raw_directory) = record.get("sessionDir").and_then(Value::as_str) else {
                return;
            };
            let candidate = Path::new(raw_directory);
            let directory = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                root.join(candidate)
            };
            let Ok(directory) = directory.canonicalize() else {
                return;
            };
            if directory.starts_with(&root) {
                entries.push(IndexEntry {
                    session_key,
                    session_dir: directory,
                });
            }
        },
        &mut malformed,
    )?;
    if malformed || fingerprint(&path)? != before {
        return Err("Kimi session index contains malformed, oversized, or changed records");
    }
    Ok(IndexSnapshot {
        entries,
        fingerprint: before,
    })
}

fn parse(
    root: &Path,
    source: &KimiSource,
    identity: SourceIdentity,
    index: &IndexSnapshot,
) -> Result<SourceSnapshot, &'static str> {
    if index
        .entries
        .iter()
        .filter(|entry| {
            entry.session_key == source.session_key && entry.session_dir == source.session_dir
        })
        .count()
        != 1
    {
        return Err("Kimi wire source does not match the session index");
    }
    let state_path = source.session_dir.join("state.json");
    let before = InputFingerprint {
        index: fingerprint(&root.join("session_index.jsonl"))?,
        state: fingerprint(&state_path)?,
        wire: fingerprint(&source.wire)?,
    };
    if before.index != index.fingerprint {
        return Err("Kimi session index changed while it was being collected");
    }
    if before.wire.size > MAX_SOURCE_BYTES
        || before.index.size > MAX_SOURCE_BYTES
        || before.state.size > MAX_RECORD_BYTES as u64
    {
        return Err("Kimi source exceeds the supported size limit");
    }
    let execution_kind = execution_kind(&state_path, &source.agent_key)?;
    let mut items = Vec::new();
    let mut text_bytes = 0;
    let mut partial = None;
    let mut malformed = false;
    stream(
        &source.wire,
        &mut |record| {
            if synthetic_or_ignored(&record) {
                return;
            }
            for extracted in record_items(&record) {
                if extracted.truncated {
                    note(&mut partial, PartialReason::Text);
                }
                let _ = push(&mut items, &mut text_bytes, &mut partial, extracted.item);
            }
        },
        &mut malformed,
    )?;
    if malformed {
        return Err("Kimi wire source contains malformed or oversized records");
    }
    let after = InputFingerprint {
        index: fingerprint(&root.join("session_index.jsonl"))?,
        state: fingerprint(&state_path)?,
        wire: fingerprint(&source.wire)?,
    };
    if after != before || after.index != index.fingerprint {
        return Err("Kimi source changed while it was being collected");
    }
    let sessions = has_text(&items)
        .then(|| CatalogSession {
            session_key: source.session_key.clone(),
            title: items.iter().find_map(|item| match item {
                CatalogItem::UserText(text) => Some(short_title(text)),
                _ => None,
            }),
            repository: None,
            cwd: None,
            model: None,
            execution_kind: Some(execution_kind.to_owned()),
            started_at: None,
            last_visible_event_at: None,
            items,
        })
        .into_iter()
        .collect();
    Ok(SourceSnapshot {
        identity,
        diagnostic_status: if partial.is_some() { "partial" } else { "ok" },
        diagnostic_message: partial.map(PartialReason::message),
        sessions,
    })
}

fn execution_kind(state_path: &Path, agent_key: &str) -> Result<&'static str, &'static str> {
    let bytes = fs::read(state_path).map_err(|_| "could not read Kimi session state")?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("Kimi session state exceeds the supported size limit");
    }
    let state: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Kimi session state is malformed")?;
    let agents = state
        .get("agents")
        .and_then(Value::as_object)
        .ok_or("Kimi session state has no agents")?;
    if !agents.contains_key("main") || !agents.get(agent_key).is_some_and(Value::is_object) {
        return Err("Kimi session state does not describe the wire agent");
    }
    if agent_key == "main" {
        Ok("kimi_primary")
    } else {
        let parent = agents
            .get(agent_key)
            .and_then(|agent| agent.get("parentAgentId"))
            .and_then(Value::as_str);
        bounded_str(parent.unwrap_or(""), MAX_KEY_BYTES)
            .is_some()
            .then_some("kimi_subagent")
            .ok_or("Kimi subagent has no usable parent identity")
    }
}

struct Extracted {
    item: CatalogItem,
    truncated: bool,
}

fn record_items(record: &Value) -> Vec<Extracted> {
    match record.get("type").and_then(Value::as_str) {
        Some("turn.prompt") => content_items(record.get("input"), true),
        Some("context.append_message") => {
            let Some(message) = record.get("message").filter(|value| value.is_object()) else {
                return Vec::new();
            };
            if synthetic_or_ignored(message) {
                return Vec::new();
            }
            match message.get("role").and_then(Value::as_str) {
                Some("user") => message_items(message, true),
                Some("assistant") => message_items(message, false),
                _ => Vec::new(),
            }
        }
        Some("context.append_loop_event") => loop_event_items(record),
        _ => Vec::new(),
    }
}

fn loop_event_items(record: &Value) -> Vec<Extracted> {
    let Some(event) = record.get("event").filter(|value| value.is_object()) else {
        return Vec::new();
    };
    if synthetic_or_ignored(event) {
        return Vec::new();
    }
    if event.get("type").and_then(Value::as_str) == Some("tool.call") {
        let Some(name) = bounded(event.get("name"), MAX_TOOL_BYTES) else {
            return Vec::new();
        };
        return vec![Extracted {
            item: CatalogItem::ToolMarker {
                name,
                status: event
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(allowed_tool_status)
                    .map(ToOwned::to_owned),
            },
            truncated: false,
        }];
    }
    if event.get("type").and_then(Value::as_str) != Some("content.part") {
        return Vec::new();
    }
    let Some(part) = event.get("part").filter(|value| value.is_object()) else {
        return Vec::new();
    };
    if synthetic_or_ignored(part) || part.get("type").and_then(Value::as_str) != Some("text") {
        return Vec::new();
    }
    part.get("text")
        .and_then(Value::as_str)
        .and_then(|text| visible(text, false))
        .into_iter()
        .collect()
}

fn message_items(message: &Value, user: bool) -> Vec<Extracted> {
    let mut items = Vec::new();
    if let Some(text) = message.get("text").and_then(Value::as_str)
        && let Some(item) = visible(text, user)
    {
        items.push(item);
    }
    items.extend(content_items(message.get("content"), user));
    items
}

fn content_items(content: Option<&Value>, user: bool) -> Vec<Extracted> {
    let mut items = Vec::new();
    match content {
        Some(Value::String(text)) => {
            if let Some(item) = visible(text, user) {
                items.push(item);
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if synthetic_or_ignored(part) {
                    continue;
                }
                let visible_text_part = part.get("type").and_then(Value::as_str) == Some("text")
                    || (part.get("type").is_none()
                        && part.as_object().is_some_and(|object| {
                            object.len() == 1 && object.contains_key("text")
                        }));
                if visible_text_part
                    && let Some(text) = part.get("text").and_then(Value::as_str)
                    && let Some(item) = visible(text, user)
                {
                    items.push(item);
                }
            }
        }
        _ => {}
    }
    items
}

fn visible(text: &str, user: bool) -> Option<Extracted> {
    let text = text.trim();
    if text.is_empty() || (user && control_text(text)) {
        return None;
    }
    let truncated = text.len() > MAX_TEXT_BYTES;
    Some(Extracted {
        item: if user {
            CatalogItem::UserText(truncate(text, MAX_TEXT_BYTES))
        } else {
            CatalogItem::AssistantText(truncate(text, MAX_TEXT_BYTES))
        },
        truncated,
    })
}

fn synthetic_or_ignored(value: &Value) -> bool {
    ["isMeta", "isSynthetic", "synthetic", "ignored"]
        .iter()
        .any(|field| value.get(*field).and_then(Value::as_bool) == Some(true))
}
fn allowed_tool_status(status: &str) -> Option<&'static str> {
    match status {
        "requested" => Some("requested"),
        "pending" => Some("pending"),
        "running" => Some("running"),
        "completed" => Some("completed"),
        "error" => Some("error"),
        _ => None,
    }
}
fn control_text(text: &str) -> bool {
    [
        "<system-reminder>",
        "<task-notification>",
        "<command-message>",
        "<command-name>",
        "<teammate-message>",
        "<local-command-stdout>",
    ]
    .iter()
    .any(|prefix| text.trim_start().starts_with(prefix))
}

fn push(
    items: &mut Vec<CatalogItem>,
    bytes: &mut usize,
    partial: &mut Option<PartialReason>,
    mut item: CatalogItem,
) -> bool {
    if items.len() >= MAX_ITEMS {
        note(partial, PartialReason::Items);
        return false;
    }
    if let Some(text) = text_mut(&mut item) {
        let remaining = MAX_TEXT_TOTAL.saturating_sub(*bytes);
        if remaining == 0 {
            note(partial, PartialReason::TotalText);
            return false;
        }
        if text.len() > remaining {
            *text = truncate(text, remaining);
            note(partial, PartialReason::TotalText);
        }
        *bytes += text.len();
    }
    items.push(item);
    true
}

fn stream(
    path: &Path,
    callback: &mut dyn FnMut(Value),
    malformed: &mut bool,
) -> Result<(), &'static str> {
    let file = File::open(path).map_err(|_| "could not read Kimi source")?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut overlong = false;
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|_| "could not read Kimi source")?;
        if read == 0 {
            if !line.is_empty() || overlong {
                process(&line, overlong, callback, malformed);
            }
            return Ok(());
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                process(&line, overlong, callback, malformed);
                line.clear();
                overlong = false;
            } else if line.len() < MAX_RECORD_BYTES {
                line.push(*byte);
            } else {
                overlong = true;
            }
        }
    }
}
fn process(line: &[u8], overlong: bool, callback: &mut dyn FnMut(Value), malformed: &mut bool) {
    if overlong || line.is_empty() {
        *malformed = true;
        return;
    }
    match std::str::from_utf8(line)
        .ok()
        .and_then(|text| serde_json::from_str(text).ok())
    {
        Some(value @ Value::Object(_)) => callback(value),
        _ => *malformed = true,
    }
}
fn fingerprint(path: &Path) -> Result<Fingerprint, &'static str> {
    let metadata = fs::metadata(path).map_err(|_| "could not read Kimi source")?;
    Ok(Fingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}
fn bounded(value: Option<&Value>, max: usize) -> Option<String> {
    bounded_str(value?.as_str()?, max)
}
fn bounded_str(value: &str, max: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= max).then(|| value.to_owned())
}
fn text_mut(item: &mut CatalogItem) -> Option<&mut String> {
    match item {
        CatalogItem::UserText(text) | CatalogItem::AssistantText(text) => Some(text),
        CatalogItem::ToolMarker { .. } => None,
    }
}
fn has_text(items: &[CatalogItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            CatalogItem::UserText(_) | CatalogItem::AssistantText(_)
        )
    })
}
fn note(current: &mut Option<PartialReason>, reason: PartialReason) {
    if current.is_none() {
        *current = Some(reason);
    }
}
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
fn short_title(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = truncate(&text, 160);
    if title.len() < text.len() {
        title.push('…');
    }
    title
}

#[cfg(test)]
mod tests {
    use super::{MAX_TEXT_BYTES, collect_kimi};
    use crate::storage::open_database;
    use sqlx::query_scalar;
    use std::fs;
    use tempfile::TempDir;

    fn write_source(
        root: &std::path::Path,
        workspace: &str,
        session: &str,
        agent: &str,
        wire: &str,
        state: &str,
    ) -> std::path::PathBuf {
        let directory = root.join("sessions").join(workspace).join(session);
        let path = directory.join("agents").join(agent).join("wire.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        fs::write(&path, wire).expect("wire");
        fs::write(directory.join("state.json"), state).expect("state");
        path
    }

    #[tokio::test]
    async fn collects_visible_kimi_wire_text_and_preserves_last_good_source() {
        let temporary = TempDir::new().expect("temporary");
        let root = temporary.path().join("kimi");
        let wire = write_source(
            &root,
            "work",
            "session",
            "main",
            "{\"type\":\"turn.prompt\",\"input\":[{\"text\":\"visible user\"},{\"text\":\"<system-reminder>PRIVATE_CONTROL\"},{\"text\":\"PRIVATE_UNTYPED\",\"args\":\"PRIVATE_ARGS\"}]}\n{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"part\":{\"type\":\"text\",\"text\":\"visible assistant\"}}}\n{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"part\":{\"type\":\"think\",\"text\":\"PRIVATE_THINKING\"}}}\n{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"tool.call\",\"name\":\"Read\",\"status\":\"requested\",\"args\":\"PRIVATE_ARGS\",\"result\":\"PRIVATE_RESULT\",\"display\":\"PRIVATE_DISPLAY\",\"description\":\"PRIVATE_DESCRIPTION\"}}\n",
            "{\"agents\":{\"main\":{}}}",
        );
        fs::write(
            root.join("session_index.jsonl"),
            "{\"sessionId\":\"session\",\"sessionDir\":\"sessions/work/session\"}\n",
        )
        .expect("index");
        let before = fs::read(&wire).expect("before");
        let before_metadata = fs::metadata(&wire).expect("metadata");
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("catalog");
        let report = collect_kimi(&root, &catalog).await.expect("scan");
        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(fs::read(&wire).expect("after"), before);
        assert_eq!(
            fs::metadata(&wire).expect("metadata").len(),
            before_metadata.len()
        );
        let stored = query_scalar::<_, String>(
            "SELECT group_concat(COALESCE(content, tool_name), '|') FROM transcript_items",
        )
        .fetch_one(&catalog)
        .await
        .expect("stored");
        assert!(stored.contains("visible user"));
        assert!(stored.contains("visible assistant"));
        assert!(stored.contains("Read"));
        assert!(!stored.contains("PRIVATE_"));
        let kind = query_scalar::<_, String>("SELECT execution_kind FROM sessions")
            .fetch_one(&catalog)
            .await
            .expect("kind");
        assert_eq!(kind, "kimi_primary");
        let session_key = query_scalar::<_, String>("SELECT session_key FROM sessions")
            .fetch_one(&catalog)
            .await
            .expect("session key");
        assert_eq!(session_key, "session");
        fs::write(
            root.join("session_index.jsonl"),
            "{\"sessionId\":\"session\",\"sessionDir\":\"sessions/work/session\"}\n{\"sessionId\":\"session\",\"sessionDir\":\"sessions/work/session\"}\n",
        )
        .expect("duplicate index");
        let report = collect_kimi(&root, &catalog).await.expect("rescan");
        assert_eq!(report.failed_sources, 1);
        let retained = query_scalar::<_, String>(
            "SELECT content FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&catalog)
        .await
        .expect("retained");
        assert_eq!(retained, "visible user");
        catalog.close().await;
    }

    #[tokio::test]
    async fn subagent_and_text_bound_are_cataloged_without_parent_graph() {
        let temporary = TempDir::new().expect("temporary");
        let root = temporary.path().join("kimi");
        write_source(
            &root,
            "work",
            "session",
            "agent",
            &format!(
                "{{\"type\":\"turn.prompt\",\"input\":[{{\"text\":\"{}\"}}]}}\n",
                "x".repeat(MAX_TEXT_BYTES + 1)
            ),
            "{\"agents\":{\"main\":{},\"agent\":{\"parentAgentId\":\"main\"}}}",
        );
        fs::write(
            root.join("session_index.jsonl"),
            "{\"sessionId\":\"session\",\"sessionDir\":\"sessions/work/session\"}\n",
        )
        .expect("index");
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("catalog");
        let report = collect_kimi(&root, &catalog).await.expect("scan");
        assert_eq!(report.partial_sources, 1);
        let kind = query_scalar::<_, String>("SELECT execution_kind FROM sessions")
            .fetch_one(&catalog)
            .await
            .expect("kind");
        assert_eq!(kind, "kimi_subagent");
        let length = query_scalar::<_, i64>("SELECT length(content) FROM transcript_items")
            .fetch_one(&catalog)
            .await
            .expect("length");
        assert_eq!(length, i64::try_from(MAX_TEXT_BYTES).expect("bound"));
        catalog.close().await;
    }
}
