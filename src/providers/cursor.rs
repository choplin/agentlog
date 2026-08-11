//! Read-only `Cursor` transcript JSONL collection.

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
use serde_json::Value;
use sqlx::SqlitePool;
use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_TOOL_BYTES: usize = 256;
const MAX_ITEMS: usize = 2_000;
const MAX_TEXT_TOTAL: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CursorScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
}

/// Cursor-owned source locations and native discovery rules.
#[derive(Clone, Debug)]
pub struct CursorProvider {
    root: PathBuf,
}

impl CursorProvider {
    /// Resolves the Cursor root using provider-native precedence.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable absolute root can be resolved.
    pub fn resolve(configured: Option<&Path>) -> Result<Self, ProviderRootError> {
        Self::resolve_from(configured, env::var_os("HOME"))
    }

    pub(crate) fn resolve_from(
        configured: Option<&Path>,
        os_home: Option<OsString>,
    ) -> Result<Self, ProviderRootError> {
        let root = if let Some(root) = configured {
            root.to_path_buf()
        } else {
            os_home
                .map(PathBuf::from)
                .filter(|home| !home.as_os_str().is_empty())
                .ok_or(ProviderRootError::CursorHomeUnavailable)?
                .join(".cursor")
        };
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(ProviderRootError::InvalidCursorRoot { path: root });
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
        ProviderId::Cursor
    }

    fn sources(&self) -> std::io::Result<Vec<(CursorSource, SourceIdentity)>> {
        discover(&self.root)
    }
}

/// Agentlog projection of Cursor-native sources.
#[derive(Clone, Debug)]
pub struct CursorScanner {
    provider: CursorProvider,
}

impl CursorScanner {
    #[must_use]
    pub fn new(provider: CursorProvider) -> Self {
        Self { provider }
    }
}

impl ProviderScanner for CursorScanner {
    fn provider_id(&self) -> ProviderId {
        self.provider.id()
    }

    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError> {
        let sources = self.provider.sources()?;
        let candidate_sources = u64::try_from(sources.len()).unwrap_or(u64::MAX);
        Ok(Box::new(CursorScan {
            candidate_sources,
            sources: sources.into_iter(),
        }))
    }
}

struct CursorScan {
    candidate_sources: u64,
    sources: std::vec::IntoIter<(CursorSource, SourceIdentity)>,
}

impl ProviderScan for CursorScan {
    fn candidate_sources(&self) -> u64 {
        self.candidate_sources
    }

    fn next_outcome(&mut self) -> ProviderScanFuture<'_> {
        let outcome =
            self.sources.next().map(
                |(source, identity)| match parse(&source, identity.clone()) {
                    Ok(snapshot) => SourceOutcome::Accepted(snapshot),
                    Err(message) => SourceOutcome::Failed { identity, message },
                },
            );
        Box::pin(async move { Ok(outcome) })
    }
}

#[derive(Clone, Debug)]
struct CursorSource {
    path: PathBuf,
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct Fingerprint {
    size: u64,
    modified: Option<SystemTime>,
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

/// Collects native `Cursor` transcript JSONL files below one configuration root.
///
/// # Errors
///
/// Returns an error when Agentlog cannot record source outcomes.
pub async fn collect_cursor(
    root: &Path,
    pool: &SqlitePool,
) -> Result<CursorScanReport, StorageError> {
    let scanner = CursorScanner::new(CursorProvider::at_root(root.to_path_buf()));
    let report = scan_provider_with_pool(pool, &scanner).await?;
    Ok(CursorScanReport {
        candidate_sources: report.candidate_sources,
        refreshed_sources: report.refreshed_sources,
        partial_sources: report.partial_sources,
        failed_sources: report.failed_sources,
    })
}
fn discover(root: &Path) -> std::io::Result<Vec<(CursorSource, SourceIdentity)>> {
    let mut sources = Vec::new();
    visit(&root.join("projects"), &mut |path| {
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            let source = CursorSource {
                path: path.to_path_buf(),
            };
            let identity = identity(&source);
            sources.push((source, identity));
        }
        Ok(())
    })?;
    sources.sort_by(|left, right| left.0.path.cmp(&right.0.path));
    Ok(sources)
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
fn identity(source: &CursorSource) -> SourceIdentity {
    SourceIdentity {
        provider: "cursor",
        source_format: "transcript_jsonl",
        canonical_locator: source
            .path
            .canonicalize()
            .unwrap_or_else(|_| source.path.clone())
            .to_string_lossy()
            .into_owned(),
    }
}
fn parse(source: &CursorSource, identity: SourceIdentity) -> Result<SourceSnapshot, &'static str> {
    let before = fingerprint(&source.path)?;
    if before.size > MAX_SOURCE_BYTES {
        return Err("source exceeds the supported size limit");
    }
    let key = source
        .path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .and_then(bounded_str)
        .ok_or("source has no usable Cursor session identity")?;
    let mut items = Vec::new();
    let mut text_bytes = 0;
    let mut partial = None;
    let mut malformed = false;
    let mut invalid_native_record = false;
    let mut native = false;
    stream(
        &source.path,
        &mut |record| {
            let role = match record.get("role").and_then(Value::as_str) {
                Some("user") => Some(true),
                Some("assistant") => Some(false),
                _ => None,
            };
            let Some(user) = role else {
                return;
            };
            native = true;
            if synthetic_or_ignored(&record) {
                return;
            }
            let Some(message) = record.get("message").filter(|value| value.is_object()) else {
                invalid_native_record = true;
                return;
            };
            if synthetic_or_ignored(message) {
                return;
            }
            for extracted in content(message.get("content"), user) {
                if extracted.truncated {
                    note(&mut partial, PartialReason::Text);
                }
                let _ = push(&mut items, &mut text_bytes, &mut partial, extracted.item);
            }
        },
        &mut malformed,
    )?;
    if malformed || invalid_native_record || !native {
        return Err("source contains malformed, oversized, or unsupported records");
    }
    if fingerprint(&source.path)? != before {
        return Err("source changed while it was being collected");
    }
    let sessions = if has_text(&items) {
        let title = items.iter().find_map(|item| match item {
            CatalogItem::UserText(text) => Some(short_title(text)),
            _ => None,
        });
        vec![CatalogSession {
            session_key: key,
            title,
            repository: None,
            cwd: None,
            model: None,
            execution_kind: Some("cursor_transcript".to_owned()),
            started_at: None,
            last_visible_event_at: None,
            items,
        }]
    } else {
        Vec::new()
    };
    Ok(SourceSnapshot {
        identity,
        diagnostic_status: if partial.is_some() { "partial" } else { "ok" },
        diagnostic_message: partial.map(PartialReason::message),
        sessions,
    })
}
struct Extracted {
    item: CatalogItem,
    truncated: bool,
}
fn content(value: Option<&Value>, user: bool) -> Vec<Extracted> {
    let mut extracted = Vec::new();
    let Some(Value::Array(parts)) = value else {
        return extracted;
    };
    for part in parts {
        if synthetic_or_ignored(part) {
            continue;
        }
        if matches!(part.get("type").and_then(Value::as_str), Some("text"))
            && let Some(text) = part.get("text").and_then(Value::as_str)
            && let Some(item) = visible(text, user)
        {
            extracted.push(item);
        }
        if !user
            && part.get("type").and_then(Value::as_str) == Some("tool_use")
            && let Some(name) = bounded(part.get("name"), MAX_TOOL_BYTES)
        {
            extracted.push(Extracted {
                item: CatalogItem::ToolMarker {
                    name,
                    status: Some("requested".to_owned()),
                },
                truncated: false,
            });
        }
    }
    extracted
}
fn visible(text: &str, user: bool) -> Option<Extracted> {
    let text = text.trim();
    if text.is_empty() || (user && control_text(text)) {
        return None;
    }
    let truncated = text.len() > MAX_TEXT_BYTES;
    let text = truncate(text, MAX_TEXT_BYTES);
    Some(Extracted {
        item: if user {
            CatalogItem::UserText(text)
        } else {
            CatalogItem::AssistantText(text)
        },
        truncated,
    })
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
    let file = File::open(path).map_err(|_| "could not read provider source")?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut overlong = false;
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|_| "could not read provider source")?;
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
    let metadata = fs::metadata(path).map_err(|_| "could not read provider source")?;
    Ok(Fingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}
fn bounded(value: Option<&Value>, max: usize) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty() && value.len() <= max).then(|| value.to_owned())
}
fn bounded_str(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= MAX_KEY_BYTES).then(|| value.to_owned())
}
fn synthetic_or_ignored(value: &Value) -> bool {
    ["isMeta", "isSynthetic", "synthetic", "ignored"]
        .iter()
        .any(|field| value.get(*field).and_then(Value::as_bool) == Some(true))
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
    use super::{MAX_TEXT_BYTES, collect_cursor};
    use crate::storage::open_database;
    use sqlx::query_scalar;
    use std::fs;
    use tempfile::TempDir;
    #[tokio::test]
    async fn collects_cursor_visible_parts_without_tool_payloads_and_isolates_bad_source() {
        let temporary = TempDir::new().expect("temporary");
        let root = temporary.path().join("cursor");
        let good = root.join("projects/a/session-a/arbitrary-capture-name.jsonl");
        let bad = root.join("projects/b/session-b/transcript.jsonl");
        fs::create_dir_all(good.parent().expect("parent")).expect("good dir");
        fs::create_dir_all(bad.parent().expect("parent")).expect("bad dir");
        fs::write(&good, "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"visible user\"},{\"type\":\"text\",\"text\":\"<system-reminder>PRIVATE_CONTROL\"},{\"type\":\"text\",\"text\":\"PRIVATE_META\",\"isMeta\":true}]}}\n{\"role\":\"user\",\"isSynthetic\":true,\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"PRIVATE_SYNTHETIC\"}]}}\n{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"visible assistant\"},{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"text\":\"PRIVATE_INPUT\"}}]}}\n").expect("good");
        fs::write(&bad, "not json\n").expect("bad");
        let before = fs::read(&good).expect("before");
        let before_metadata = fs::metadata(&good).expect("metadata");
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("catalog");
        let report = collect_cursor(&root, &catalog).await.expect("scan");
        assert_eq!(report.refreshed_sources, 1);
        assert_eq!(report.failed_sources, 1);
        assert_eq!(fs::read(&good).expect("after"), before);
        let after_metadata = fs::metadata(&good).expect("metadata");
        assert_eq!(after_metadata.len(), before_metadata.len());
        assert_eq!(
            after_metadata.modified().ok(),
            before_metadata.modified().ok()
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
        let key = query_scalar::<_, String>("SELECT session_key FROM sessions")
            .fetch_one(&catalog)
            .await
            .expect("session key");
        assert_eq!(key, "session-a");
        let timestamps = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT started_at, last_visible_event_at FROM sessions",
        )
        .fetch_one(&catalog)
        .await
        .expect("session timestamps");
        assert_eq!(timestamps, (None, None));
        fs::write(&good, "not json\n").expect("break last-good source");
        let report = collect_cursor(&root, &catalog).await.expect("rescan");
        assert_eq!(report.failed_sources, 2);
        let retained = query_scalar::<_, String>(
            "SELECT content FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&catalog)
        .await
        .expect("retained text");
        assert_eq!(retained, "visible user");
        catalog.close().await;
    }

    #[tokio::test]
    async fn text_over_the_cursor_item_bound_is_partial() {
        let temporary = TempDir::new().expect("temporary");
        let root = temporary.path().join("cursor");
        let path = root.join("projects/project/session/capture.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(
            &path,
            format!(
                "{{\"role\":\"user\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
                "x".repeat(MAX_TEXT_BYTES + 1)
            ),
        )
        .expect("source");
        let catalog = open_database(&temporary.path().join("catalog.sqlite3"))
            .await
            .expect("catalog");

        let report = collect_cursor(&root, &catalog).await.expect("scan");

        assert_eq!(report.partial_sources, 1);
        let length = query_scalar::<_, i64>(
            "SELECT length(content) FROM transcript_items WHERE item_kind = 'user_text'",
        )
        .fetch_one(&catalog)
        .await
        .expect("stored text length");
        assert_eq!(length, i64::try_from(MAX_TEXT_BYTES).expect("bound fits"));
        catalog.close().await;
    }
}
