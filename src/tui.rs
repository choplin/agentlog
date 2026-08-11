//! Interactive, catalog-only browse, refine, and diagnostics interface.

use std::{
    collections::BTreeSet,
    future::Future,
    io::{self, Stderr},
    panic::{self, PanicHookInfo},
    sync::{Arc, Mutex, MutexGuard, TryLockError, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::task::JoinHandle;

use crate::{
    app::{
        SyncProgress, SyncSummary, diagnostics_shell, list_shell, show_shell_by_identity,
        sync_shell_with_progress,
    },
    display::visible_text,
    paths::AppPaths,
    storage::{
        CatalogItemView, CatalogSessionPreview, CatalogSessionSummary, CatalogSourceDiagnostic,
    },
};

const BROWSE_SESSION_LIMIT: u32 = 500;
const WIDE_MIN_WIDTH: u16 = 100;
const WIDE_MIN_HEIGHT: u16 = 24;
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;
const PREVIEW_SCROLL_STEP: u16 = 3;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PREVIEW_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const DAY_SECONDS: i64 = 24 * 60 * 60;

static TERMINAL_SESSION_LOCK: Mutex<()> = Mutex::new(());

/// Starts the interactive catalog browser and synchronizes provider sources in the background.
///
/// # Errors
///
/// Returns an error when terminal setup, input, drawing, or normal terminal
/// restoration fails. Catalog read failures remain visible and retryable in UI.
pub async fn run(paths: &AppPaths) -> anyhow::Result<()> {
    let anchor_now = unix_now().context("read browse activity-date anchor")?;
    let mut terminal = TerminalSession::enter()?;
    let mut state = BrowseState::new(anchor_now);
    let mut preview_loader = PreviewLoader::new();
    let mut sync_loader = SyncLoader::new();
    if let Some(id) = load_sessions(paths, &mut state).await {
        preview_loader.request(paths, &state, id);
    }
    sync_loader.request(paths, &mut state);

    loop {
        preview_loader.collect_ready(&mut state);
        sync_loader.collect_progress(&mut state);
        if let Some(result) = sync_loader.collect_ready() {
            state.finish_sync(&result);
            preview_loader.clear();
            if let Some(id) = load_sessions(paths, &mut state).await {
                preview_loader.request(paths, &state, id);
            }
        }
        terminal.draw(|frame| render(frame, &mut state))?;
        if let Some(event) = poll_event(sync_loader.wait_timeout(preview_loader.wait_timeout()))? {
            match update(&mut state, event) {
                BrowseEffect::None => {}
                BrowseEffect::Exit => break,
                BrowseEffect::Sync => {
                    sync_loader.request(paths, &mut state);
                }
                BrowseEffect::LoadSessions => {
                    preview_loader.clear();
                    if let Some(id) = load_sessions(paths, &mut state).await {
                        preview_loader.request(paths, &state, id);
                    }
                }
                BrowseEffect::LoadPreview(id) => preview_loader.request(paths, &state, id),
                BrowseEffect::LoadDiagnostics => load_diagnostics(paths, &mut state).await,
            }
        }
    }
    terminal.finish().context("restore interactive terminal")
}

/// Coordinates one synchronization at a time while the terminal remains responsive.
///
/// Synchronization deliberately has no queue or cancellation behaviour: a repeated
/// request while the current sync is active is ignored. This keeps the catalog
/// write boundary single-flight without introducing a general scheduler.
#[derive(Debug, Default)]
struct SyncLoader {
    result_receiver: Option<mpsc::Receiver<anyhow::Result<SyncSummary>>>,
    progress_receiver: Option<mpsc::Receiver<SyncProgress>>,
}

impl SyncLoader {
    fn new() -> Self {
        Self::default()
    }

    fn request(&mut self, paths: &AppPaths, state: &mut BrowseState) -> bool {
        if self.result_receiver.is_some() {
            return false;
        }
        state.start_sync();
        let paths = paths.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (progress_sender, progress_receiver) = mpsc::sync_channel(64);
        let spawned = thread::Builder::new()
            .name("agentlog-sync".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(sync_shell_with_progress(&paths, |progress| {
                            if matches!(progress, SyncProgress::SourceStaged { .. }) {
                                let _ = progress_sender.try_send(progress);
                            } else {
                                let _ = progress_sender.send(progress);
                            }
                        }))
                    });
                let _ = sender.send(result);
            });
        match spawned {
            Ok(_) => {
                self.result_receiver = Some(receiver);
                self.progress_receiver = Some(progress_receiver);
                true
            }
            Err(error) => {
                state.finish_sync(&Err(anyhow::Error::from(error)));
                false
            }
        }
    }

    fn collect_ready(&mut self) -> Option<anyhow::Result<SyncSummary>> {
        let receiver = self.result_receiver.as_ref()?;
        match receiver.try_recv() {
            Ok(result) => {
                self.result_receiver = None;
                self.progress_receiver = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.result_receiver = None;
                self.progress_receiver = None;
                Some(Err(anyhow::anyhow!("sync worker ended without a result")))
            }
        }
    }

    fn collect_progress(&mut self, state: &mut BrowseState) {
        let Some(receiver) = self.progress_receiver.as_ref() else {
            return;
        };
        while let Ok(progress) = receiver.try_recv() {
            state.apply_sync_progress(progress);
        }
    }

    fn wait_timeout(&self, idle_timeout: Duration) -> Duration {
        if self.result_receiver.is_some() {
            PREVIEW_RESULT_POLL_INTERVAL
        } else {
            idle_timeout
        }
    }
}

fn unix_now() -> anyhow::Result<i64> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(duration.as_secs()).context("convert current Unix timestamp")
}

async fn load_sessions(paths: &AppPaths, state: &mut BrowseState) -> Option<i64> {
    match list_shell(paths, BROWSE_SESSION_LIMIT).await {
        Ok(sessions) => state.replace_sessions(sessions),
        Err(error) => {
            state.catalog_error = Some(format!("Could not read the catalog: {error:#}"));
            None
        }
    }
}

async fn load_diagnostics(paths: &AppPaths, state: &mut BrowseState) {
    match diagnostics_shell(paths).await {
        Ok(diagnostics) => {
            state.diagnostics = Some(diagnostics);
            state.diagnostics_error = None;
        }
        Err(error) => {
            state.diagnostics_error = Some(format!("Could not read diagnostics: {error:#}"));
        }
    }
}

fn poll_event(timeout: Duration) -> io::Result<Option<BrowseEvent>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(browse_event_from_key(key))),
        _ => Ok(Some(BrowseEvent::Ignore)),
    }
}

fn browse_event_from_key(key: KeyEvent) -> BrowseEvent {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return BrowseEvent::Quit;
    }

    match key.code {
        KeyCode::Char('q') => BrowseEvent::Quit,
        KeyCode::Char('r') => BrowseEvent::Reload,
        KeyCode::Char('f') => BrowseEvent::OpenRefine,
        KeyCode::Char('?') => BrowseEvent::OpenHelp,
        KeyCode::Char('!') => BrowseEvent::OpenDiagnostics,
        KeyCode::Char('j') | KeyCode::Down => BrowseEvent::Down,
        KeyCode::Char('k') | KeyCode::Up => BrowseEvent::Up,
        KeyCode::Char('d') => BrowseEvent::HalfPageDown,
        KeyCode::Char('u') => BrowseEvent::HalfPageUp,
        KeyCode::Char('g') | KeyCode::Home => BrowseEvent::Top,
        KeyCode::Char('G') | KeyCode::End => BrowseEvent::Bottom,
        KeyCode::PageDown => BrowseEvent::PageDown,
        KeyCode::PageUp => BrowseEvent::PageUp,
        KeyCode::Char(' ') => BrowseEvent::Toggle,
        KeyCode::Tab | KeyCode::Enter => BrowseEvent::Next,
        KeyCode::BackTab => BrowseEvent::Previous,
        KeyCode::Char('x') => BrowseEvent::ClearCurrent,
        KeyCode::Char('C') => BrowseEvent::ClearAll,
        KeyCode::Esc => BrowseEvent::Back,
        _ => BrowseEvent::Ignore,
    }
}

#[derive(Debug)]
struct PreviewLoader {
    current: Option<PreviewRequest>,
    generation: u64,
    result: Arc<Mutex<Option<PreviewLoadResult>>>,
    in_flight: Option<InFlightPreview>,
}

#[derive(Debug)]
struct InFlightPreview {
    request: PreviewRequest,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct PreviewLoadResult {
    request: PreviewRequest,
    result: anyhow::Result<CatalogSessionPreview>,
}

impl PreviewLoader {
    fn new() -> Self {
        Self {
            current: None,
            generation: 0,
            result: Arc::new(Mutex::new(None)),
            in_flight: None,
        }
    }

    fn request(&mut self, paths: &AppPaths, state: &BrowseState, id: i64) {
        let Some(session) = state
            .selected_session()
            .filter(|session| session.id == id)
            .cloned()
        else {
            return;
        };
        let paths = paths.clone();
        self.start(
            id,
            async move { show_shell_by_identity(&paths, &session).await },
        );
    }

    fn clear(&mut self) {
        self.cancel_in_flight();
        self.current = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn wait_timeout(&self) -> Duration {
        if self.in_flight.is_some() {
            PREVIEW_RESULT_POLL_INTERVAL
        } else {
            INPUT_POLL_INTERVAL
        }
    }

    fn start<F>(&mut self, id: i64, load: F)
    where
        F: Future<Output = anyhow::Result<CatalogSessionPreview>> + Send + 'static,
    {
        self.cancel_in_flight();
        self.generation = self.generation.wrapping_add(1);
        let request = PreviewRequest {
            id,
            generation: self.generation,
        };
        self.current = Some(request);
        let result_slot = Arc::clone(&self.result);
        let task = tokio::spawn(async move {
            let result = load.await;
            let completed = PreviewLoadResult { request, result };
            if let Ok(mut result_slot) = result_slot.lock()
                && result_slot
                    .as_ref()
                    .is_none_or(|current| current.request.generation < completed.request.generation)
            {
                *result_slot = Some(completed);
            }
        });
        self.in_flight = Some(InFlightPreview { request, task });
    }

    fn apply_ready(&mut self, state: &mut BrowseState) {
        if let Some(result) = self.take_ready_result() {
            if self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.request == result.request)
            {
                self.in_flight = None;
            }
            apply_preview_result(state, self.current, result);
        }
    }

    fn collect_ready(&mut self, state: &mut BrowseState) {
        self.reap_finished();
        self.apply_ready(state);
    }

    fn take_ready_result(&self) -> Option<PreviewLoadResult> {
        match self.result.try_lock() {
            Ok(mut result) => result.take(),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
        }
    }

    fn reap_finished(&mut self) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.task.is_finished())
        {
            self.in_flight = None;
        }
    }

    fn cancel_in_flight(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            in_flight.task.abort();
        }
    }
}

impl Drop for PreviewLoader {
    fn drop(&mut self) {
        self.cancel_in_flight();
    }
}

fn apply_preview_result(
    state: &mut BrowseState,
    current: Option<PreviewRequest>,
    result: PreviewLoadResult,
) {
    if current != Some(result.request) || state.selected_id() != Some(result.request.id) {
        return;
    }
    match result.result {
        Ok(preview) => state.set_preview(preview),
        Err(error) => {
            state.preview_error = Some(format!("Could not load this preview: {error:#}"));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviewRequest {
    id: i64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowseEvent {
    Down,
    Up,
    HalfPageDown,
    HalfPageUp,
    Top,
    Bottom,
    PageDown,
    PageUp,
    Toggle,
    Next,
    Previous,
    ClearCurrent,
    ClearAll,
    OpenRefine,
    OpenHelp,
    OpenDiagnostics,
    Reload,
    Quit,
    Back,
    Ignore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Browse,
    Preview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefineStep {
    Provider,
    Repository,
    Cwd,
    Model,
    Execution,
    Date,
    Group,
}

impl RefineStep {
    const ALL: [Self; 7] = [
        Self::Provider,
        Self::Repository,
        Self::Cwd,
        Self::Model,
        Self::Execution,
        Self::Date,
        Self::Group,
    ];
    fn next(self) -> Self {
        Self::ALL[usize::from(self as u8)
            .saturating_add(1)
            .min(Self::ALL.len() - 1)]
    }
    fn previous(self) -> Self {
        Self::ALL[usize::from(self as u8).saturating_sub(1)]
    }
    fn title(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::Repository => "Repository",
            Self::Cwd => "CWD",
            Self::Model => "Model",
            Self::Execution => "Execution",
            Self::Date => "Activity date",
            Self::Group => "Group",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Facet {
    Provider,
    Repository,
    Cwd,
    Model,
    Execution,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DateBucket {
    Within24Hours,
    TwoToSevenDays,
    EightToThirtyDays,
    OlderThanThirtyDays,
    Future,
}

impl DateBucket {
    const ALL: [Self; 5] = [
        Self::Within24Hours,
        Self::TwoToSevenDays,
        Self::EightToThirtyDays,
        Self::OlderThanThirtyDays,
        Self::Future,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Within24Hours => "<=24h",
            Self::TwoToSevenDays => "2-7d",
            Self::EightToThirtyDays => "8-30d",
            Self::OlderThanThirtyDays => ">30d",
            Self::Future => "future",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Grouping {
    Recent,
    Provider,
    Repository,
}

impl Grouping {
    const ALL: [Self; 3] = [Self::Recent, Self::Provider, Self::Repository];
    fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Provider => "Provider",
            Self::Repository => "Repository",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RefineFilters {
    providers: BTreeSet<String>,
    repositories: BTreeSet<String>,
    cwds: BTreeSet<String>,
    models: BTreeSet<String>,
    executions: BTreeSet<String>,
    dates: BTreeSet<DateBucket>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StableIdentity {
    provider: String,
    source_format: String,
    source_locator: String,
    session_key: String,
}

impl StableIdentity {
    fn from_session(session: &CatalogSessionSummary) -> Self {
        Self {
            provider: session.provider.clone(),
            source_format: session.source_format.clone(),
            source_locator: session.source_locator.clone(),
            session_key: session.session_key.clone(),
        }
    }
}

#[derive(Clone, Debug)]
enum VisibleRow {
    Header(GroupHeader),
    Session(Box<CatalogSessionSummary>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GroupHeader {
    Provider(String),
    Repository(RepositoryGroupKey),
}

impl GroupHeader {
    fn label(&self) -> &str {
        match self {
            Self::Provider(value) | Self::Repository(RepositoryGroupKey::Present(value)) => value,
            Self::Repository(RepositoryGroupKey::Missing) => "(no repository)",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryGroupKey {
    Missing,
    Present(String),
}

impl VisibleRow {
    fn identity(&self) -> Option<StableIdentity> {
        match self {
            Self::Header(_) => None,
            Self::Session(session) => Some(StableIdentity::from_session(session)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Overlay {
    None,
    Refine(RefineStep),
    Help,
    Diagnostics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowseEffect {
    None,
    Exit,
    Sync,
    LoadSessions,
    LoadPreview(i64),
    LoadDiagnostics,
}

#[derive(Clone, Debug)]
struct BrowseState {
    anchor_now: i64,
    loaded: Vec<CatalogSessionSummary>,
    filtered: Vec<CatalogSessionSummary>,
    rows: Vec<VisibleRow>,
    selected: Option<StableIdentity>,
    list_offset: usize,
    list_viewport_rows: usize,
    view: View,
    overlay: Overlay,
    refine_cursor: usize,
    refine_offset: usize,
    filters: RefineFilters,
    grouping: Grouping,
    preview: Option<CatalogSessionPreview>,
    preview_error: Option<String>,
    preview_scroll: u16,
    preview_content_width: u16,
    preview_viewport_rows: usize,
    catalog_error: Option<String>,
    sync_status: SyncStatus,
    diagnostics: Option<Vec<CatalogSourceDiagnostic>>,
    diagnostics_error: Option<String>,
    diagnostics_scroll: u16,
    diagnostics_content_width: u16,
    diagnostics_viewport_lines: usize,
}

#[derive(Clone, Debug, Default)]
enum SyncStatus {
    #[default]
    Idle,
    Running(SyncProgressLine),
    Complete(SyncSummaryLine),
    Failed(String),
}

#[derive(Clone, Debug)]
struct SyncSummaryLine {
    providers_failed: u8,
    refreshed_sources: u64,
    partial_sources: u64,
    failed_sources: u64,
    missing_sources: u64,
    sessions_available: u64,
}

#[derive(Clone, Debug, Default)]
struct SyncProgressLine {
    provider: Option<String>,
    processed_sources: u64,
    candidate_sources: Option<u64>,
}

impl BrowseState {
    fn new(anchor_now: i64) -> Self {
        Self {
            anchor_now,
            loaded: Vec::new(),
            filtered: Vec::new(),
            rows: Vec::new(),
            selected: None,
            list_offset: 0,
            list_viewport_rows: 1,
            view: View::Browse,
            overlay: Overlay::None,
            refine_cursor: 0,
            refine_offset: 0,
            filters: RefineFilters::default(),
            grouping: Grouping::Recent,
            preview: None,
            preview_error: None,
            preview_scroll: 0,
            preview_content_width: 1,
            preview_viewport_rows: 1,
            catalog_error: None,
            sync_status: SyncStatus::Idle,
            diagnostics: None,
            diagnostics_error: None,
            diagnostics_scroll: 0,
            diagnostics_content_width: 1,
            diagnostics_viewport_lines: 1,
        }
    }

    fn replace_sessions(&mut self, sessions: Vec<CatalogSessionSummary>) -> Option<i64> {
        self.loaded = sessions;
        self.catalog_error = None;
        self.rebuild();
        self.preview = None;
        self.preview_error = None;
        self.preview_scroll = 0;
        self.selected_id()
    }
    fn start_sync(&mut self) {
        self.sync_status = SyncStatus::Running(SyncProgressLine::default());
    }
    fn apply_sync_progress(&mut self, progress: SyncProgress) {
        match progress {
            SyncProgress::ProviderDiscovering { provider } => {
                self.sync_status = SyncStatus::Running(SyncProgressLine {
                    provider: Some(provider),
                    processed_sources: 0,
                    candidate_sources: None,
                });
            }
            SyncProgress::ProviderCandidates {
                provider,
                candidate_sources,
            } => {
                self.sync_status = SyncStatus::Running(SyncProgressLine {
                    provider: Some(provider),
                    processed_sources: 0,
                    candidate_sources: Some(candidate_sources),
                });
            }
            SyncProgress::SourceStaged {
                provider,
                processed_sources,
                candidate_sources,
            } => {
                self.sync_status = SyncStatus::Running(SyncProgressLine {
                    provider: Some(provider),
                    processed_sources,
                    candidate_sources: Some(candidate_sources),
                });
            }
            SyncProgress::ProviderCompleted {
                provider, report, ..
            } => {
                self.sync_status = SyncStatus::Running(SyncProgressLine {
                    provider: Some(provider),
                    processed_sources: report.refreshed_sources + report.failed_sources,
                    candidate_sources: Some(report.candidate_sources),
                });
            }
        }
    }
    fn finish_sync(&mut self, result: &anyhow::Result<SyncSummary>) {
        self.sync_status = match result {
            Ok(summary) => SyncStatus::Complete(SyncSummaryLine {
                providers_failed: summary.providers_failed,
                refreshed_sources: summary.sources_refreshed,
                partial_sources: summary.sources_partial,
                failed_sources: summary.sources_failed,
                missing_sources: summary.sources_missing,
                sessions_available: summary.sessions_available,
            }),
            Err(error) => SyncStatus::Failed(format!("Sync failed: {error:#}")),
        };
    }
    fn selected_session(&self) -> Option<&CatalogSessionSummary> {
        let selected = self.selected.as_ref()?;
        self.filtered
            .iter()
            .find(|session| StableIdentity::from_session(session) == *selected)
    }
    fn selected_id(&self) -> Option<i64> {
        self.selected_session().map(|session| session.id)
    }
    fn selected_row(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .position(|row| row.identity().as_ref() == Some(selected))
    }
    fn rebuild(&mut self) -> Option<i64> {
        let previous = self.selected.clone();
        self.filtered = filter_sessions(&self.loaded, &self.filters, self.anchor_now);
        self.rows = group_sessions(&self.filtered, self.grouping);
        self.selected = previous
            .clone()
            .filter(|identity| {
                self.rows
                    .iter()
                    .any(|row| row.identity().as_ref() == Some(identity))
            })
            .or_else(|| self.rows.iter().find_map(VisibleRow::identity));
        if self.selected == previous {
            None
        } else {
            self.preview = None;
            self.preview_error = None;
            self.preview_scroll = 0;
            self.selected_id()
        }
    }
    fn set_preview(&mut self, preview: CatalogSessionPreview) {
        self.preview = Some(preview);
        self.preview_error = None;
        self.preview_scroll = 0;
    }
    fn move_selection(&mut self, delta: isize) -> Option<i64> {
        let selectable = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.identity().map(|identity| (index, identity)))
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.selected = None;
            return None;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|identity| {
                selectable
                    .iter()
                    .position(|(_, candidate)| candidate == identity)
            })
            .unwrap_or(0);
        let target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(usize::try_from(delta).expect("nonnegative delta is representable"))
                .min(selectable.len() - 1)
        };
        let next = selectable[target].1.clone();
        if self.selected.as_ref() == Some(&next) {
            return None;
        }
        self.selected = Some(next);
        self.preview = None;
        self.preview_error = None;
        self.preview_scroll = 0;
        self.selected_id()
    }

    fn move_selection_by_rows(&mut self, down: bool, steps: usize) -> Option<i64> {
        let current = self.selected_row()?;
        let target = if down {
            current
                .saturating_add(steps)
                .min(self.rows.len().saturating_sub(1))
        } else {
            current.saturating_sub(steps)
        };
        let next = if down {
            (target..self.rows.len()).find_map(|index| self.rows[index].identity())
        } else {
            (0..=target)
                .rev()
                .find_map(|index| self.rows[index].identity())
        };
        let next = next?;
        if self.selected.as_ref() == Some(&next) {
            return None;
        }
        self.selected = Some(next);
        self.preview = None;
        self.preview_error = None;
        self.preview_scroll = 0;
        self.selected_id()
    }
    fn facet_options(&self, facet: Facet) -> Vec<String> {
        facet_options(&self.loaded, facet)
    }
    fn refine_option_count(&self, step: RefineStep) -> usize {
        match step {
            RefineStep::Provider => self.facet_options(Facet::Provider).len(),
            RefineStep::Repository => self.facet_options(Facet::Repository).len(),
            RefineStep::Cwd => self.facet_options(Facet::Cwd).len(),
            RefineStep::Model => self.facet_options(Facet::Model).len(),
            RefineStep::Execution => self.facet_options(Facet::Execution).len(),
            RefineStep::Date => DateBucket::ALL.len(),
            RefineStep::Group => Grouping::ALL.len(),
        }
    }
    fn toggle_refine(&mut self, step: RefineStep) -> Option<i64> {
        match step {
            RefineStep::Provider
            | RefineStep::Repository
            | RefineStep::Cwd
            | RefineStep::Model
            | RefineStep::Execution => {
                let facet = match step {
                    RefineStep::Provider => Facet::Provider,
                    RefineStep::Repository => Facet::Repository,
                    RefineStep::Cwd => Facet::Cwd,
                    RefineStep::Model => Facet::Model,
                    _ => Facet::Execution,
                };
                if let Some(value) = self.facet_options(facet).get(self.refine_cursor).cloned() {
                    toggle_set(self.facet_set_mut(facet), &value);
                }
            }
            RefineStep::Date => {
                if let Some(bucket) = DateBucket::ALL.get(self.refine_cursor).copied() {
                    toggle_set(&mut self.filters.dates, &bucket);
                }
            }
            RefineStep::Group => {
                self.grouping = Grouping::ALL
                    .get(self.refine_cursor)
                    .copied()
                    .unwrap_or(Grouping::Recent);
            }
        }
        self.rebuild()
    }
    fn facet_set_mut(&mut self, facet: Facet) -> &mut BTreeSet<String> {
        match facet {
            Facet::Provider => &mut self.filters.providers,
            Facet::Repository => &mut self.filters.repositories,
            Facet::Cwd => &mut self.filters.cwds,
            Facet::Model => &mut self.filters.models,
            Facet::Execution => &mut self.filters.executions,
        }
    }
    fn clear_current(&mut self, step: RefineStep) -> Option<i64> {
        match step {
            RefineStep::Provider => self.filters.providers.clear(),
            RefineStep::Repository => self.filters.repositories.clear(),
            RefineStep::Cwd => self.filters.cwds.clear(),
            RefineStep::Model => self.filters.models.clear(),
            RefineStep::Execution => self.filters.executions.clear(),
            RefineStep::Date => self.filters.dates.clear(),
            RefineStep::Group => self.grouping = Grouping::Recent,
        }
        self.rebuild()
    }
    fn clear_all(&mut self) -> Option<i64> {
        self.filters = RefineFilters::default();
        self.grouping = Grouping::Recent;
        self.rebuild()
    }
}

fn toggle_set<T: Clone + Ord>(values: &mut BTreeSet<T>, value: &T) {
    if !values.insert(value.clone()) {
        values.remove(value);
    }
}
fn facet_value(session: &CatalogSessionSummary, facet: Facet) -> Option<&str> {
    let value = match facet {
        Facet::Provider => Some(session.provider.as_str()),
        Facet::Repository => session.repository.as_deref(),
        Facet::Cwd => session.cwd.as_deref(),
        Facet::Model => session.model.as_deref(),
        Facet::Execution => session.execution_kind.as_deref(),
    };
    value.filter(|value| !value.trim().is_empty())
}
fn date_bucket(session: &CatalogSessionSummary, anchor: i64) -> Option<DateBucket> {
    Some(match session.started_at {
        None => return None,
        Some(timestamp) if timestamp > anchor => DateBucket::Future,
        Some(timestamp) => {
            let age = anchor.saturating_sub(timestamp);
            if age <= DAY_SECONDS {
                DateBucket::Within24Hours
            } else if age <= 7 * DAY_SECONDS {
                DateBucket::TwoToSevenDays
            } else if age <= 30 * DAY_SECONDS {
                DateBucket::EightToThirtyDays
            } else {
                DateBucket::OlderThanThirtyDays
            }
        }
    })
}
fn matches_facet(values: &BTreeSet<String>, candidate: Option<&str>) -> bool {
    values.is_empty() || candidate.is_some_and(|value| values.contains(value))
}
fn filter_sessions(
    sessions: &[CatalogSessionSummary],
    filters: &RefineFilters,
    anchor: i64,
) -> Vec<CatalogSessionSummary> {
    sessions
        .iter()
        .filter(|session| {
            matches_facet(&filters.providers, facet_value(session, Facet::Provider))
                && matches_facet(
                    &filters.repositories,
                    facet_value(session, Facet::Repository),
                )
                && matches_facet(&filters.cwds, facet_value(session, Facet::Cwd))
                && matches_facet(&filters.models, facet_value(session, Facet::Model))
                && matches_facet(&filters.executions, facet_value(session, Facet::Execution))
                && (filters.dates.is_empty()
                    || date_bucket(session, anchor)
                        .is_some_and(|bucket| filters.dates.contains(&bucket)))
        })
        .cloned()
        .collect()
}
fn facet_options(sessions: &[CatalogSessionSummary], facet: Facet) -> Vec<String> {
    let mut values = Vec::new();
    for session in sessions {
        if let Some(value) = facet_value(session, facet)
            && !values.iter().any(|existing| existing == value)
        {
            values.push(value.to_owned());
        }
    }
    values
}
fn group_sessions(sessions: &[CatalogSessionSummary], grouping: Grouping) -> Vec<VisibleRow> {
    if grouping == Grouping::Recent {
        return sessions
            .iter()
            .cloned()
            .map(|session| VisibleRow::Session(Box::new(session)))
            .collect();
    }
    let mut groups: Vec<(GroupHeader, Vec<CatalogSessionSummary>)> = Vec::new();
    for session in sessions {
        let key = match grouping {
            Grouping::Provider => GroupHeader::Provider(
                facet_value(session, Facet::Provider)
                    .unwrap_or("(no provider)")
                    .to_owned(),
            ),
            Grouping::Repository => GroupHeader::Repository(
                facet_value(session, Facet::Repository)
                    .map_or(RepositoryGroupKey::Missing, |value| {
                        RepositoryGroupKey::Present(value.to_owned())
                    }),
            ),
            Grouping::Recent => unreachable!("recent grouping returned above"),
        };
        if let Some((_, members)) = groups.iter_mut().find(|(candidate, _)| candidate == &key) {
            members.push(session.clone());
        } else {
            groups.push((key, vec![session.clone()]));
        }
    }
    groups
        .into_iter()
        .flat_map(|(key, members)| {
            std::iter::once(VisibleRow::Header(key)).chain(
                members
                    .into_iter()
                    .map(|session| VisibleRow::Session(Box::new(session))),
            )
        })
        .collect()
}

fn update(state: &mut BrowseState, event: BrowseEvent) -> BrowseEffect {
    if event == BrowseEvent::Reload {
        return BrowseEffect::Sync;
    }
    if state.catalog_error.is_some() {
        return match event {
            BrowseEvent::Quit | BrowseEvent::Back => BrowseEffect::Exit,
            _ => BrowseEffect::None,
        };
    }
    match state.overlay.clone() {
        Overlay::Refine(step) => update_refine(state, step, event),
        Overlay::Diagnostics => update_diagnostics(state, event),
        Overlay::Help => match event {
            BrowseEvent::Quit => BrowseEffect::Exit,
            BrowseEvent::Back => {
                state.overlay = Overlay::None;
                BrowseEffect::None
            }
            _ => BrowseEffect::None,
        },
        Overlay::None => update_base(state, event),
    }
}

fn update_diagnostics(state: &mut BrowseState, event: BrowseEvent) -> BrowseEffect {
    match event {
        BrowseEvent::Quit => BrowseEffect::Exit,
        BrowseEvent::Back => {
            state.overlay = Overlay::None;
            BrowseEffect::None
        }
        BrowseEvent::Down => {
            state.diagnostics_scroll = state.diagnostics_scroll.saturating_add(1);
            BrowseEffect::None
        }
        BrowseEvent::Up => {
            state.diagnostics_scroll = state.diagnostics_scroll.saturating_sub(1);
            BrowseEffect::None
        }
        BrowseEvent::PageDown => {
            state.diagnostics_scroll = state.diagnostics_scroll.saturating_add(PREVIEW_SCROLL_STEP);
            BrowseEffect::None
        }
        BrowseEvent::PageUp => {
            state.diagnostics_scroll = state.diagnostics_scroll.saturating_sub(PREVIEW_SCROLL_STEP);
            BrowseEffect::None
        }
        BrowseEvent::Top => {
            state.diagnostics_scroll = 0;
            BrowseEffect::None
        }
        BrowseEvent::Bottom => {
            let lines = diagnostic_content(state).0;
            let bottom = diagnostics_visual_line_count(&lines, state.diagnostics_content_width)
                .saturating_sub(state.diagnostics_viewport_lines);
            state.diagnostics_scroll = u16::try_from(bottom).unwrap_or(u16::MAX);
            BrowseEffect::None
        }
        _ => BrowseEffect::None,
    }
}

fn update_refine(state: &mut BrowseState, step: RefineStep, event: BrowseEvent) -> BrowseEffect {
    match event {
        BrowseEvent::Quit => BrowseEffect::Exit,
        BrowseEvent::Back => {
            state.overlay = Overlay::None;
            BrowseEffect::None
        }
        BrowseEvent::Down => {
            let count = state.refine_option_count(step);
            if count > 0 {
                state.refine_cursor = state.refine_cursor.saturating_add(1).min(count - 1);
            }
            BrowseEffect::None
        }
        BrowseEvent::Up => {
            state.refine_cursor = state.refine_cursor.saturating_sub(1);
            BrowseEffect::None
        }
        BrowseEvent::Toggle => state
            .toggle_refine(step)
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::Next => {
            let next = step.next();
            state.overlay = Overlay::Refine(next);
            state.refine_cursor = 0;
            state.refine_offset = 0;
            BrowseEffect::None
        }
        BrowseEvent::Previous => {
            let previous = step.previous();
            state.overlay = Overlay::Refine(previous);
            state.refine_cursor = 0;
            state.refine_offset = 0;
            BrowseEffect::None
        }
        BrowseEvent::ClearCurrent => state
            .clear_current(step)
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::ClearAll => state
            .clear_all()
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        _ => BrowseEffect::None,
    }
}

fn update_base(state: &mut BrowseState, event: BrowseEvent) -> BrowseEffect {
    match state.view {
        View::Browse => update_browse(state, event),
        View::Preview => update_preview(state, event),
    }
}

fn update_browse(state: &mut BrowseState, event: BrowseEvent) -> BrowseEffect {
    match event {
        BrowseEvent::Quit | BrowseEvent::Back => BrowseEffect::Exit,
        BrowseEvent::Reload => BrowseEffect::LoadSessions,
        BrowseEvent::OpenRefine => {
            state.overlay = Overlay::Refine(RefineStep::Provider);
            state.refine_cursor = 0;
            state.refine_offset = 0;
            BrowseEffect::None
        }
        BrowseEvent::OpenHelp => {
            state.overlay = Overlay::Help;
            BrowseEffect::None
        }
        BrowseEvent::OpenDiagnostics => {
            state.overlay = Overlay::Diagnostics;
            state.diagnostics_scroll = 0;
            BrowseEffect::LoadDiagnostics
        }
        BrowseEvent::Down => state
            .move_selection(1)
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::Up => state
            .move_selection(-1)
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::HalfPageDown => state
            .move_selection_by_rows(true, half_page_step(state.list_viewport_rows))
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::HalfPageUp => state
            .move_selection_by_rows(false, half_page_step(state.list_viewport_rows))
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        BrowseEvent::Top => {
            state.selected = state.rows.iter().find_map(VisibleRow::identity);
            state.list_offset = 0;
            state
                .selected_id()
                .map_or(BrowseEffect::None, BrowseEffect::LoadPreview)
        }
        BrowseEvent::Bottom => {
            state.selected = state.rows.iter().rev().find_map(VisibleRow::identity);
            state
                .selected_id()
                .map_or(BrowseEffect::None, BrowseEffect::LoadPreview)
        }
        BrowseEvent::Next => {
            if state.selected.is_some() {
                state.view = View::Preview;
            }
            BrowseEffect::None
        }
        _ => BrowseEffect::None,
    }
}

fn update_preview(state: &mut BrowseState, event: BrowseEvent) -> BrowseEffect {
    match event {
        BrowseEvent::Quit => BrowseEffect::Exit,
        BrowseEvent::Back => {
            state.view = View::Browse;
            BrowseEffect::None
        }
        BrowseEvent::OpenRefine => {
            state.overlay = Overlay::Refine(RefineStep::Provider);
            state.refine_cursor = 0;
            state.refine_offset = 0;
            BrowseEffect::None
        }
        BrowseEvent::OpenHelp => {
            state.overlay = Overlay::Help;
            BrowseEffect::None
        }
        BrowseEvent::OpenDiagnostics => {
            state.overlay = Overlay::Diagnostics;
            state.diagnostics_scroll = 0;
            BrowseEffect::LoadDiagnostics
        }
        BrowseEvent::Down => {
            state.preview_scroll = state.preview_scroll.saturating_add(1);
            BrowseEffect::None
        }
        BrowseEvent::Up => {
            state.preview_scroll = state.preview_scroll.saturating_sub(1);
            BrowseEffect::None
        }
        BrowseEvent::HalfPageDown => {
            state.preview_scroll = state
                .preview_scroll
                .saturating_add(
                    u16::try_from(half_page_step(state.preview_viewport_rows)).unwrap_or(u16::MAX),
                )
                .min(preview_bottom_scroll(state));
            BrowseEffect::None
        }
        BrowseEvent::HalfPageUp => {
            state.preview_scroll = state.preview_scroll.saturating_sub(
                u16::try_from(half_page_step(state.preview_viewport_rows)).unwrap_or(u16::MAX),
            );
            BrowseEffect::None
        }
        BrowseEvent::PageDown => {
            state.preview_scroll = state.preview_scroll.saturating_add(PREVIEW_SCROLL_STEP);
            BrowseEffect::None
        }
        BrowseEvent::PageUp => {
            state.preview_scroll = state.preview_scroll.saturating_sub(PREVIEW_SCROLL_STEP);
            BrowseEffect::None
        }
        BrowseEvent::Top => {
            state.preview_scroll = 0;
            BrowseEffect::None
        }
        BrowseEvent::Bottom => {
            state.preview_scroll = preview_bottom_scroll(state);
            BrowseEffect::None
        }
        BrowseEvent::Reload => state
            .selected_id()
            .map_or(BrowseEffect::None, BrowseEffect::LoadPreview),
        _ => BrowseEffect::None,
    }
}

fn half_page_step(viewport_rows: usize) -> usize {
    (viewport_rows / 2).max(1)
}

fn preview_bottom_scroll(state: &BrowseState) -> u16 {
    let line_count = state.preview.as_ref().map_or(0, |preview| {
        preview_visual_line_count(preview, state.preview_content_width)
    });
    u16::try_from(line_count.saturating_sub(state.preview_viewport_rows)).unwrap_or(u16::MAX)
}

fn render(frame: &mut Frame, state: &mut BrowseState) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_message(
            frame,
            area,
            "Terminal too small",
            "Resize to at least 40 columns by 10 rows.",
            "q quit",
        );
        return;
    }
    if let Some(error) = &state.catalog_error {
        render_message(frame, area, "Catalog error", error, "r retry | q quit");
        return;
    }
    match state.view {
        View::Browse => render_browse(frame, area, state),
        View::Preview => render_full_preview(frame, area, state),
    }
    match state.overlay.clone() {
        Overlay::None => {}
        Overlay::Refine(step) => {
            let area = overlay_area(area);
            frame.render_widget(Clear, area);
            render_refine(frame, area, state, step);
        }
        Overlay::Help => {
            let area = overlay_area(area);
            frame.render_widget(Clear, area);
            render_help(frame, area);
        }
        Overlay::Diagnostics => {
            let area = overlay_area(area);
            frame.render_widget(Clear, area);
            render_diagnostics(frame, area, state);
        }
    }
}

fn render_browse(frame: &mut Frame, area: Rect, state: &mut BrowseState) {
    if state.rows.is_empty() {
        render_message(
            frame,
            area,
            "No matching sessions",
            "Adjust Refine filters or press C in Refine to clear them.",
            &format!(
                "{} | f refine | ? help | ! diagnostics | r sync | q quit",
                sync_status_line(state)
            ),
        );
        return;
    }
    if area.width >= WIDE_MIN_WIDTH && area.height >= WIDE_MIN_HEIGHT {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        render_session_list(frame, columns[0], state, true);
        render_preview(frame, columns[1], state, "Preview");
    } else {
        render_session_list(frame, area, state, false);
    }
}

fn filter_summary(state: &BrowseState) -> String {
    let mut parts = Vec::new();
    for (name, values) in [
        ("P", &state.filters.providers),
        ("R", &state.filters.repositories),
        ("C", &state.filters.cwds),
        ("M", &state.filters.models),
        ("E", &state.filters.executions),
    ] {
        if !values.is_empty() {
            parts.push(format!("{name}:{}", values.len()));
        }
    }
    if !state.filters.dates.is_empty() {
        parts.push(format!("D:{}", state.filters.dates.len()));
    }
    if parts.is_empty() {
        "All".to_owned()
    } else {
        parts.join(" ")
    }
}

fn sync_status_line(state: &BrowseState) -> String {
    match &state.sync_status {
        SyncStatus::Idle => "Sync: not started".to_owned(),
        SyncStatus::Running(progress) => match &progress.provider {
            Some(provider) if progress.candidate_sources.is_some() => format!(
                "Sync: {} {}/{} sources processed; existing sessions remain available",
                visible_text(provider),
                progress.processed_sources,
                progress.candidate_sources.unwrap_or(0)
            ),
            Some(provider) => format!(
                "Sync: {} discovering sources; existing sessions remain available",
                visible_text(provider)
            ),
            None => "Sync: starting; existing sessions remain available".to_owned(),
        },
        SyncStatus::Complete(summary) => format!(
            "Sync: providers_failed={} sources={} partial={} failed={} missing={} sessions={}",
            summary.providers_failed,
            summary.refreshed_sources,
            summary.partial_sources,
            summary.failed_sources,
            summary.missing_sources,
            summary.sessions_available
        ),
        SyncStatus::Failed(error) => visible_text(error),
    }
}

fn session_row(session: &CatalogSessionSummary, wide: bool, available_width: usize) -> String {
    let started = format_started(session.started_at);
    let title = session_title(session);
    let provider = visible_text(&session.provider);
    if wide {
        let repository = session
            .repository
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map_or_else(|| "(no repository)".to_owned(), visible_text);
        let (title_width, metadata_widths) = row_width_budgets(
            available_width,
            &started,
            &title,
            &[&repository, &provider],
            9,
        );
        format!(
            "{started} | {} | {} | {}",
            ellipsize(&title, title_width),
            ellipsize(&repository, metadata_widths[0]),
            ellipsize(&provider, metadata_widths[1]),
        )
    } else {
        let (title_width, metadata_widths) =
            row_width_budgets(available_width, &started, &title, &[&provider], 6);
        format!(
            "{started} | {} | {}",
            ellipsize(&title, title_width),
            ellipsize(&provider, metadata_widths[0]),
        )
    }
}

fn row_width_budgets(
    available_width: usize,
    started: &str,
    title: &str,
    metadata: &[&str],
    separators_width: usize,
) -> (usize, Vec<usize>) {
    let content_width = available_width
        .saturating_sub(cell_width(started))
        .saturating_sub(separators_width);
    let title_minimum = usize::from(content_width > 0);
    let mut remaining = content_width.saturating_sub(title_minimum);
    let mut metadata_widths = metadata
        .iter()
        .map(|value| {
            let budget = cell_width(value).min(12).min(remaining);
            remaining = remaining.saturating_sub(budget);
            budget
        })
        .collect::<Vec<_>>();
    for (index, value) in metadata.iter().enumerate() {
        let additional = cell_width(value)
            .saturating_sub(metadata_widths[index])
            .min(remaining);
        metadata_widths[index] = metadata_widths[index].saturating_add(additional);
        remaining = remaining.saturating_sub(additional);
    }
    let title_width = title_minimum
        .saturating_add(remaining)
        .min(cell_width(title));
    (title_width, metadata_widths)
}

fn cell_width(text: &str) -> usize {
    Line::from(text).width()
}

fn ellipsize(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if cell_width(text) <= width {
        return text.to_owned();
    }
    let ellipsis_width = cell_width("…");
    if width <= ellipsis_width {
        return "…".to_owned();
    }
    let mut prefix = String::new();
    for character in text.chars() {
        prefix.push(character);
        if cell_width(&prefix).saturating_add(ellipsis_width) > width {
            prefix.pop();
            break;
        }
    }
    if prefix.is_empty() {
        "…".to_owned()
    } else {
        format!("{prefix}…")
    }
}

fn format_started(timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return "Unknown UTC".to_owned();
    };
    let days = timestamp.div_euclid(DAY_SECONDS);
    let (year, month, day) = civil_date_from_days(days);
    let seconds = timestamp.rem_euclid(DAY_SECONDS);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}Z")
}

fn civil_date_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(mp + if mp < 10 { 3 } else { -9 }).unwrap_or(1);
    (year + i64::from(month <= 2), month, day)
}

fn render_session_list(frame: &mut Frame, area: Rect, state: &mut BrowseState, wide: bool) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    let items = state
        .rows
        .iter()
        .map(|row| match row {
            VisibleRow::Header(header) => ListItem::new(Line::styled(
                format!("-- {} --", visible_text(header.label())),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            VisibleRow::Session(session) => ListItem::new(Line::from(session_row(
                session,
                wide,
                usize::from(area.width.saturating_sub(4)),
            ))),
        })
        .collect::<Vec<_>>();
    let title = format!(
        "{} | {} visible | {} | Group: {}",
        sync_status_line(state),
        state.filtered.len(),
        filter_summary(state),
        state.grouping.label()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = ListState::default().with_offset(state.list_offset);
    list_state.select(state.selected_row());
    state.list_viewport_rows = usize::from(regions[0].height.saturating_sub(2));
    frame.render_stateful_widget(list, regions[0], &mut list_state);
    state.list_offset = list_state.offset();
    frame.render_widget(
        Paragraph::new(
            "f refine | ? help | ! diagnostics | j/k select | d/u half page | Enter preview | r sync | q quit",
        ),
        regions[1],
    );
}

fn render_full_preview(frame: &mut Frame, area: Rect, state: &mut BrowseState) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    render_preview(frame, regions[0], state, "Session preview");
    frame.render_widget(
        Paragraph::new(
            "f refine | ? help | ! diagnostics | j/k scroll | d/u half page | PgUp/PgDn | g/G | r sync | Esc back | q quit",
        ),
        regions[1],
    );
}
fn render_preview(frame: &mut Frame, area: Rect, state: &mut BrowseState, title: &str) {
    state.preview_content_width = area.width.saturating_sub(2).max(1);
    state.preview_viewport_rows = usize::from(area.height.saturating_sub(2));
    state.preview_scroll = state.preview_scroll.min(preview_bottom_scroll(state));
    let content = if let Some(error) = &state.preview_error {
        Paragraph::new(visible_text(error))
    } else if let Some(preview) = &state.preview {
        Paragraph::new(preview_lines(preview)).scroll((state.preview_scroll, 0))
    } else {
        Paragraph::new("Loading selected session preview...")
    };
    frame.render_widget(
        content
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn preview_visual_line_count(preview: &CatalogSessionPreview, width: u16) -> usize {
    Paragraph::new(preview_lines(preview))
        .wrap(Wrap { trim: false })
        .line_count(width)
}
fn render_message(frame: &mut Frame, area: Rect, title: &str, message: &str, controls: &str) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    frame.render_widget(
        Paragraph::new(visible_text(message))
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        regions[0],
    );
    frame.render_widget(Paragraph::new(controls), regions[1]);
}
fn overlay_area(area: Rect) -> Rect {
    let mx = if area.width >= 80 { area.width / 8 } else { 1 };
    let my = if area.height >= 24 {
        area.height / 8
    } else {
        1
    };
    Rect {
        x: area.x.saturating_add(mx),
        y: area.y.saturating_add(my),
        width: area.width.saturating_sub(mx.saturating_mul(2)).max(1),
        height: area.height.saturating_sub(my.saturating_mul(2)).max(1),
    }
}
fn render_refine(frame: &mut Frame, area: Rect, state: &mut BrowseState, step: RefineStep) {
    let options = refine_options(state, step);
    let items = options
        .iter()
        .map(|(label, selected)| {
            ListItem::new(Line::from(format!(
                "{} {}",
                if *selected { "[x]" } else { "[ ]" },
                visible_text(label)
            )))
        })
        .collect::<Vec<_>>();
    let title = format!(
        "Refine: {} ({}/7)",
        step.title(),
        usize::from(step as u8) + 1
    );
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = ListState::default().with_offset(state.refine_offset);
    list_state.select((!options.is_empty()).then_some(state.refine_cursor));
    frame.render_stateful_widget(list, regions[0], &mut list_state);
    state.refine_offset = list_state.offset();
    frame.render_widget(
        Paragraph::new(
            "Space toggle | Tab/Enter next | BackTab prev | x clear | C reset | Esc close",
        ),
        regions[1],
    );
}
fn refine_options(state: &BrowseState, step: RefineStep) -> Vec<(String, bool)> {
    match step {
        RefineStep::Provider => state
            .facet_options(Facet::Provider)
            .into_iter()
            .map(|value| {
                let selected = state.filters.providers.contains(&value);
                (value, selected)
            })
            .collect(),
        RefineStep::Repository => state
            .facet_options(Facet::Repository)
            .into_iter()
            .map(|value| {
                let selected = state.filters.repositories.contains(&value);
                (value, selected)
            })
            .collect(),
        RefineStep::Cwd => state
            .facet_options(Facet::Cwd)
            .into_iter()
            .map(|value| {
                let selected = state.filters.cwds.contains(&value);
                (value, selected)
            })
            .collect(),
        RefineStep::Model => state
            .facet_options(Facet::Model)
            .into_iter()
            .map(|value| {
                let selected = state.filters.models.contains(&value);
                (value, selected)
            })
            .collect(),
        RefineStep::Execution => state
            .facet_options(Facet::Execution)
            .into_iter()
            .map(|value| {
                let selected = state.filters.executions.contains(&value);
                (value, selected)
            })
            .collect(),
        RefineStep::Date => DateBucket::ALL
            .into_iter()
            .map(|date| (date.label().to_owned(), state.filters.dates.contains(&date)))
            .collect(),
        RefineStep::Group => Grouping::ALL
            .into_iter()
            .map(|group| (group.label().to_owned(), state.grouping == group))
            .collect(),
    }
}
fn render_help(frame: &mut Frame, area: Rect) {
    render_message(
        frame,
        area,
        "Help",
        "f Refine; j/k move; Space toggles facets/date; Tab/Enter next refine step; BackTab previous; x clear step; C clear all and Recent; ! diagnostics; ? help; Esc closes overlays; q quits.",
        "Esc close | q quit",
    );
}
fn render_diagnostics(frame: &mut Frame, area: Rect, state: &mut BrowseState) {
    let (lines, selected_summary) = diagnostic_content(state);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    let content_width = regions[0].width.saturating_sub(2).max(1);
    let viewport_lines = usize::from(regions[0].height.saturating_sub(2));
    state.diagnostics_content_width = content_width;
    state.diagnostics_viewport_lines = viewport_lines;
    let bottom =
        diagnostics_visual_line_count(&lines, content_width).saturating_sub(viewport_lines);
    state.diagnostics_scroll = state
        .diagnostics_scroll
        .min(u16::try_from(bottom).unwrap_or(u16::MAX));
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .scroll((state.diagnostics_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(selected_summary.unwrap_or_else(|| "Diagnostics".to_owned())),
            )
            .wrap(Wrap { trim: false }),
        regions[0],
    );
    frame.render_widget(
        Paragraph::new("j/k scroll | PgUp/PgDn | g/G | Esc close | q quit"),
        regions[1],
    );
}

fn diagnostic_content(state: &BrowseState) -> (Vec<String>, Option<String>) {
    let mut lines = Vec::new();
    let mut selected_summary = None;
    match (&state.diagnostics_error, &state.diagnostics) {
        (Some(error), _) => lines.push(visible_text(error)),
        (_, None) => lines.push("Loading diagnostics...".to_owned()),
        (None, Some(diagnostics)) => {
            let selected = state.selected.as_ref();
            let mut ok = 0;
            let mut partial = 0;
            let mut error = 0;
            let mut missing = 0;
            for diagnostic in diagnostics {
                match diagnostic.diagnostic_status.as_str() {
                    "ok" => ok += 1,
                    "partial" => partial += 1,
                    "error" => error += 1,
                    "missing" => missing += 1,
                    _ => {}
                }
                let attributed = selected.is_some_and(|identity| {
                    identity.provider == diagnostic.provider
                        && identity.source_format == diagnostic.source_format
                        && identity.source_locator == diagnostic.source_locator
                });
                if attributed {
                    selected_summary = Some(format!(
                        "selected: {} [{}] {}",
                        visible_text(&diagnostic.provider),
                        visible_text(&diagnostic.source_format),
                        visible_text(&diagnostic.source_locator)
                    ));
                }
                lines.push(format!(
                    "{} [{}:{}] {} {}",
                    if attributed { ">" } else { " " },
                    visible_text(&diagnostic.provider),
                    visible_text(&diagnostic.source_format),
                    visible_text(&diagnostic.diagnostic_status),
                    visible_text(&diagnostic.source_locator)
                ));
                if let Some(message) = &diagnostic.diagnostic_message {
                    lines.push(format!("    {}", visible_text(message)));
                }
            }
            lines.insert(
                0,
                format!(
                    "Sources: {} | ok={ok} partial={partial} error={error} missing={missing}",
                    diagnostics.len()
                ),
            );
        }
    }
    (lines, selected_summary)
}

fn diagnostics_visual_line_count(lines: &[String], width: u16) -> usize {
    Paragraph::new(lines.iter().cloned().map(Line::from).collect::<Vec<_>>())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn preview_lines(preview: &CatalogSessionPreview) -> Vec<Line<'static>> {
    let session = &preview.session;
    let mut lines = vec![
        Line::from(Span::styled(
            session_title(session),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{} | {} | {} | source: {}",
            visible_text(&session.provider),
            visible_text(&session.source_format),
            visible_text(&session.session_key),
            visible_text(&session.source_diagnostic_status)
        )),
    ];
    append_metadata(&mut lines, "Repository", session.repository.as_deref());
    append_metadata(&mut lines, "Directory", session.cwd.as_deref());
    append_metadata(&mut lines, "Model", session.model.as_deref());
    append_metadata(&mut lines, "Execution", session.execution_kind.as_deref());
    lines.push(Line::from(""));
    for item in &preview.items {
        match item {
            CatalogItemView::UserText { content } => append_text(
                &mut lines,
                "user",
                content,
                Style::default().fg(Color::Cyan),
            ),
            CatalogItemView::AssistantText { content } => append_text(
                &mut lines,
                "assistant",
                content,
                Style::default().fg(Color::Green),
            ),
            CatalogItemView::ToolMarker { name, status } => lines.push(Line::styled(
                format!(
                    "tool: {} ({})",
                    visible_text(name),
                    visible_text(status.as_deref().unwrap_or("unknown"))
                ),
                Style::default().fg(Color::Yellow),
            )),
        }
    }
    if preview.items_truncated {
        lines.push(Line::from(
            "Preview is limited to the first 80 catalog items.",
        ));
    }
    lines
}
fn append_metadata(lines: &mut Vec<Line<'static>>, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(Line::from(format!("{label}: {}", visible_text(value))));
    }
}
fn append_text(lines: &mut Vec<Line<'static>>, label: &str, text: &str, style: Style) {
    if text.is_empty() {
        lines.push(Line::styled(format!("{label}:"), style));
    } else {
        for (index, line) in text.lines().enumerate() {
            let prefix = if index == 0 {
                format!("{label}: ")
            } else {
                "  ".to_owned()
            };
            lines.push(Line::styled(
                format!("{prefix}{}", visible_text(line)),
                style,
            ));
        }
    }
}
fn session_title(session: &CatalogSessionSummary) -> String {
    visible_text(
        session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled session"),
    )
}

type TerminalBackend = CrosstermBackend<Stderr>;

fn acquire_terminal_session_lock() -> anyhow::Result<MutexGuard<'static, ()>> {
    match TERMINAL_SESSION_LOCK.try_lock() {
        Ok(lock) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(anyhow::anyhow!(
            "an Agentlog interactive terminal session is already active"
        )),
        Err(TryLockError::Poisoned(error)) => Ok(error.into_inner()),
    }
}

struct TerminalSession {
    terminal: Terminal<TerminalBackend>,
    active: bool,
    _panic_hook: PanicHookRestorer,
    _lock: MutexGuard<'static, ()>,
}
impl TerminalSession {
    fn enter() -> anyhow::Result<Self> {
        let lock = acquire_terminal_session_lock()?;
        enable_raw_mode().context("enable terminal raw mode")?;
        if let Err(error) = execute!(io::stderr(), EnterAlternateScreen, Hide) {
            let _ = restore_terminal();
            return Err(error).context("enter terminal alternate screen");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(io::stderr())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore_terminal();
                return Err(error).context("create terminal renderer");
            }
        };
        Ok(Self {
            terminal,
            active: true,
            _panic_hook: PanicHookRestorer::install(),
            _lock: lock,
        })
    }
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.terminal.draw(render).map(|_| ())
    }
    fn finish(mut self) -> io::Result<()> {
        let result = restore_terminal();
        self.active = result.is_err();
        result
    }
}
impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_terminal();
        }
    }
}
fn restore_terminal() -> io::Result<()> {
    let raw = disable_raw_mode();
    let screen = execute!(io::stderr(), LeaveAlternateScreen, Show);
    raw.and(screen)
}
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;
struct PanicHookRestorer {
    previous: Arc<Mutex<Option<PanicHook>>>,
}
impl PanicHookRestorer {
    fn install() -> Self {
        let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
        let for_hook = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            if let Ok(previous) = for_hook.lock()
                && let Some(previous) = previous.as_ref()
            {
                previous(info);
            }
        }));
        Self { previous }
    }
}
impl Drop for PanicHookRestorer {
    fn drop(&mut self) {
        if thread::panicking() {
            let previous = Arc::clone(&self.previous);
            if let Ok(handle) = thread::Builder::new()
                .name("agentlog-panic-hook-restore".to_owned())
                .spawn(move || restore_panic_hook(&previous))
            {
                match handle.join() {
                    Ok(()) | Err(_) => {}
                }
            }
        } else {
            restore_panic_hook(&self.previous);
        }
    }
}
fn restore_panic_hook(previous: &Arc<Mutex<Option<PanicHook>>>) {
    if let Ok(mut previous) = previous.lock()
        && let Some(previous) = previous.take()
    {
        panic::set_hook(previous);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{self, AssertUnwindSafe},
        sync::{
            Arc, Mutex, MutexGuard,
            atomic::{AtomicBool, Ordering},
        },
    };

    use ratatui::{Terminal, backend::TestBackend};

    use crate::storage::CatalogItemView;

    use super::*;

    static LIFECYCLE_TEST_SERIALIZER: Mutex<()> = Mutex::new(());

    fn serialize_lifecycle_test() -> MutexGuard<'static, ()> {
        LIFECYCLE_TEST_SERIALIZER
            .lock()
            .expect("lifecycle test serializer is not poisoned")
    }

    fn session(
        id: i64,
        provider: &str,
        repository: Option<&str>,
        model: Option<&str>,
        execution: Option<&str>,
        activity: Option<i64>,
    ) -> CatalogSessionSummary {
        CatalogSessionSummary {
            id,
            provider: provider.to_owned(),
            source_format: "jsonl".to_owned(),
            source_locator: format!("/source/{id}"),
            session_key: format!("key-{id}"),
            title: Some(format!("session-{id}")),
            repository: repository.map(str::to_owned),
            cwd: None,
            model: model.map(str::to_owned),
            execution_kind: execution.map(str::to_owned),
            started_at: activity,
            last_visible_event_at: activity,
            source_diagnostic_status: "ok".to_owned(),
            source_last_success_at: None,
        }
    }

    fn preview(session: CatalogSessionSummary) -> CatalogSessionPreview {
        CatalogSessionPreview {
            session,
            items: vec![CatalogItemView::UserText {
                content: "A visible request".to_owned(),
            }],
            items_truncated: false,
        }
    }

    fn rendered_text(state: &mut BrowseState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| render(frame, state))
            .expect("render interface");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_area_text(state: &mut BrowseState, width: u16, height: u16, area: Rect) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| render(frame, state))
            .expect("render interface");
        let buffer = terminal.backend().buffer();
        (area.y..area.y.saturating_add(area.height))
            .map(|y| {
                (area.x..area.x.saturating_add(area.width))
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn control_c_key_exits_every_browse_surface() {
        let control_c =
            browse_event_from_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(control_c, BrowseEvent::Quit);
        assert_eq!(
            browse_event_from_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            BrowseEvent::Quit
        );

        let mut browse = BrowseState::new(10);
        assert_eq!(update(&mut browse, control_c), BrowseEffect::Exit);

        let mut preview = BrowseState::new(10);
        preview.view = View::Preview;
        assert_eq!(update(&mut preview, control_c), BrowseEffect::Exit);

        let mut refine = BrowseState::new(10);
        refine.overlay = Overlay::Refine(RefineStep::Provider);
        assert_eq!(update(&mut refine, control_c), BrowseEffect::Exit);

        let mut help = BrowseState::new(10);
        help.overlay = Overlay::Help;
        assert_eq!(update(&mut help, control_c), BrowseEffect::Exit);

        let mut diagnostics = BrowseState::new(10);
        diagnostics.overlay = Overlay::Diagnostics;
        assert_eq!(update(&mut diagnostics, control_c), BrowseEffect::Exit);
    }

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn wait_until_cancelled(cancelled: &AtomicBool) {
        for _ in 0..100 {
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            cancelled.load(Ordering::SeqCst),
            "preview task was not cancelled"
        );
    }

    #[tokio::test]
    async fn preview_request_starts_a_task_immediately() {
        let mut loader = PreviewLoader::new();
        let (started, started_result) = tokio::sync::oneshot::channel();

        loader.start(1, async move {
            let _ = started.send(());
            std::future::pending::<anyhow::Result<CatalogSessionPreview>>().await
        });

        assert_eq!(
            loader.current,
            Some(PreviewRequest {
                id: 1,
                generation: 1
            })
        );
        assert!(loader.in_flight.is_some());
        assert_eq!(loader.wait_timeout(), PREVIEW_RESULT_POLL_INTERVAL);
        started_result
            .await
            .expect("preview task started without a delay");
    }

    #[tokio::test]
    async fn sync_is_single_flight_and_keeps_the_current_selection_visible() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        let selected = state.selected.clone();
        let (ready, receiver) = mpsc::sync_channel(1);
        let mut loader = SyncLoader::new();
        loader.result_receiver = Some(receiver);
        state.start_sync();
        assert!(matches!(state.sync_status, SyncStatus::Running(_)));
        assert!(rendered_text(&mut state, 100, 24).contains("session-1"));
        assert!(rendered_text(&mut state, 100, 24).contains("Sync: starting"));

        assert!(matches!(
            update(&mut state, BrowseEvent::Reload),
            BrowseEffect::Sync
        ));
        let temporary = tempfile::TempDir::new().expect("temporary directory");
        let paths = AppPaths::resolve(Some(temporary.path().join("agentlog"))).expect("paths");
        assert!(
            !loader.request(&paths, &mut state),
            "a repeated request must not start a second sync"
        );

        ready
            .send(Ok(SyncSummary {
                schema_version: 1,
                collectors_installed: 6,
                providers_failed: 1,
                sources_refreshed: 2,
                sources_partial: 1,
                sources_failed: 1,
                sources_missing: 0,
                sessions_available: 1,
                provider_summaries: Vec::new(),
                message: "done".to_owned(),
            }))
            .expect("sync task remains active");
        let result = loader.collect_ready().expect("completed sync result");
        state.finish_sync(&result);

        assert_eq!(state.selected, selected);
        let rendered = rendered_text(&mut state, 180, 24);
        assert!(
            rendered
                .contains("providers_failed=1 sources=2 partial=1 failed=1 missing=0 sessions=1")
        );
        assert!(rendered.contains("session-1"));
    }

    #[test]
    fn sync_status_exposes_current_provider_and_processed_sources() {
        let mut state = BrowseState::new(10);
        state.start_sync();
        state.apply_sync_progress(SyncProgress::SourceStaged {
            provider: "claude".to_owned(),
            processed_sources: 4,
            candidate_sources: 10,
        });

        let rendered = rendered_text(&mut state, 180, 24);
        assert!(rendered.contains("Sync: claude 4/10 sources processed"));
    }

    #[test]
    fn sync_discovery_replaces_the_previous_provider_completion() {
        let mut state = BrowseState::new(10);
        state.start_sync();
        state.apply_sync_progress(SyncProgress::ProviderCompleted {
            provider: "codex".to_owned(),
            report: crate::providers::ProviderScanReport::default(),
            failure: None,
        });
        state.apply_sync_progress(SyncProgress::ProviderDiscovering {
            provider: "claude".to_owned(),
        });

        let status = sync_status_line(&state);
        assert!(status.contains("claude discovering sources"));
        assert!(!status.contains("codex"));
    }

    #[tokio::test]
    async fn consecutive_preview_requests_cancel_stale_work_and_apply_only_latest() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![
            session(1, "a", None, None, None, Some(3)),
            session(2, "a", None, None, None, Some(2)),
        ]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_signal = Arc::clone(&cancelled);
        let (started, started_result) = tokio::sync::oneshot::channel();
        let mut loader = PreviewLoader::new();
        loader.start(1, async move {
            let _signal = DropSignal(cancellation_signal);
            let _ = started.send(());
            std::future::pending::<anyhow::Result<CatalogSessionPreview>>().await
        });
        started_result.await.expect("first preview task started");
        let stale_request = loader.current.expect("first request is current");

        assert_eq!(
            update(&mut state, BrowseEvent::Down),
            BrowseEffect::LoadPreview(2)
        );
        assert_eq!(state.selected_id(), Some(2));
        {
            let mut result = loader.result.lock().expect("lock preview result slot");
            *result = Some(PreviewLoadResult {
                request: stale_request,
                result: Err(anyhow::anyhow!("stale catalog read failed")),
            });
        }
        let latest = session(2, "a", None, None, None, Some(2));
        let (latest_ready, latest_result) = tokio::sync::oneshot::channel();
        loader.start(2, async move {
            latest_result
                .await
                .map_err(|_| anyhow::anyhow!("latest preview task was dropped"))
        });
        wait_until_cancelled(&cancelled).await;
        assert!(
            rendered_text(&mut state, 100, 24).contains("session-2"),
            "the selected row redraws while the first preview is still in flight"
        );

        loader.collect_ready(&mut state);
        assert!(
            state.preview.is_none(),
            "a stale result must not overwrite the new selection"
        );
        latest_ready
            .send(preview(latest))
            .expect("latest preview task remains available");
        tokio::task::yield_now().await;
        loader.collect_ready(&mut state);

        assert_eq!(
            state.preview.as_ref().map(|preview| preview.session.id),
            Some(2)
        );
        assert!(loader.in_flight.is_none());
        assert!(state.preview_error.is_none());
        assert_eq!(loader.wait_timeout(), INPUT_POLL_INTERVAL);
    }

    #[test]
    fn list_cursor_stays_in_viewport_before_scrolling_at_the_edge() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(
            (0_i64..24)
                .map(|id| session(id, "a", None, None, None, Some(24 - id)))
                .collect(),
        );
        for _ in 0..4 {
            update(&mut state, BrowseEvent::Down);
            let _ = rendered_text(&mut state, 80, 12);
        }
        assert_eq!(state.list_offset, 0);

        for _ in 0..12 {
            update(&mut state, BrowseEvent::Down);
            let _ = rendered_text(&mut state, 80, 12);
        }
        let selected_row = state.selected_row().expect("selected row");
        assert!(state.list_offset > 0);
        assert!(state.list_offset < selected_row);
    }

    #[test]
    fn half_page_list_navigation_uses_the_rendered_viewport_and_skips_group_headers() {
        let mut state = BrowseState::new(10);
        state.grouping = Grouping::Provider;
        state.replace_sessions(vec![
            session(1, "a", None, None, None, Some(6)),
            session(2, "a", None, None, None, Some(5)),
            session(3, "b", None, None, None, Some(4)),
            session(4, "b", None, None, None, Some(3)),
            session(5, "c", None, None, None, Some(2)),
        ]);

        let _ = rendered_text(&mut state, 40, 10);
        assert_eq!(state.list_viewport_rows, 6);
        assert_eq!(
            update(&mut state, BrowseEvent::HalfPageDown),
            BrowseEffect::LoadPreview(3)
        );
        assert_eq!(state.selected_id(), Some(3));
        assert_eq!(
            update(&mut state, BrowseEvent::HalfPageUp),
            BrowseEffect::LoadPreview(1)
        );
        assert_eq!(state.selected_id(), Some(1));

        update(&mut state, BrowseEvent::HalfPageDown);
        update(&mut state, BrowseEvent::HalfPageDown);
        assert_eq!(state.selected_id(), Some(5));
        assert_eq!(
            update(&mut state, BrowseEvent::HalfPageDown),
            BrowseEffect::None
        );
    }

    #[test]
    fn half_page_preview_navigation_uses_wrapped_viewport_rows_and_clamps() {
        let mut state = BrowseState::new(10);
        let selected = session(1, "a", None, None, None, Some(1));
        state.replace_sessions(vec![selected.clone()]);
        state.set_preview(CatalogSessionPreview {
            session: selected,
            items: vec![CatalogItemView::UserText {
                content: (0..16)
                    .map(|line| format!("line-{line:02} has enough text to wrap"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }],
            items_truncated: false,
        });
        state.view = View::Preview;

        let _ = rendered_text(&mut state, 40, 10);
        assert_eq!(state.preview_viewport_rows, 6);
        let bottom = preview_bottom_scroll(&state);
        assert!(bottom > 3);

        update(&mut state, BrowseEvent::HalfPageDown);
        assert_eq!(state.preview_scroll, 3);
        update(&mut state, BrowseEvent::HalfPageUp);
        assert_eq!(state.preview_scroll, 0);

        state.preview_scroll = bottom.saturating_sub(1);
        update(&mut state, BrowseEvent::HalfPageDown);
        assert_eq!(state.preview_scroll, bottom);
        update(&mut state, BrowseEvent::HalfPageDown);
        assert_eq!(state.preview_scroll, bottom);
    }
    #[test]
    fn filters_are_or_within_a_field_and_and_across_fields() {
        let sessions = vec![
            session(1, "a", Some("r"), Some("m"), None, Some(1)),
            session(2, "b", Some("r"), Some("x"), None, Some(1)),
            session(3, "c", Some("z"), Some("m"), None, Some(1)),
        ];
        let filters = RefineFilters {
            providers: ["a".to_owned(), "b".to_owned()].into_iter().collect(),
            repositories: ["r".to_owned()].into_iter().collect(),
            ..RefineFilters::default()
        };
        assert_eq!(
            filter_sessions(&sessions, &filters, 10)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
    #[test]
    fn cwd_is_an_exact_and_field_and_missing_values_do_not_match() {
        let mut sessions = vec![
            session(1, "a", Some("repo"), None, None, Some(1)),
            session(2, "a", Some("repo"), None, None, Some(1)),
            session(3, "a", Some("repo"), None, None, Some(1)),
        ];
        sessions[0].cwd = Some("/work/a".to_owned());
        sessions[1].cwd = Some("/work/b".to_owned());
        let filters = RefineFilters {
            repositories: ["repo".to_owned()].into_iter().collect(),
            cwds: ["/work/a".to_owned(), "/work/b".to_owned()]
                .into_iter()
                .collect(),
            ..RefineFilters::default()
        };
        assert_eq!(
            facet_options(&sessions, Facet::Cwd),
            vec!["/work/a", "/work/b"]
        );
        assert_eq!(
            filter_sessions(&sessions, &filters, 10)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
    #[test]
    fn missing_or_blank_metadata_is_never_a_facet_candidate_or_match() {
        let sessions = vec![
            session(1, "a", None, None, None, Some(1)),
            session(2, "a", Some(""), None, None, Some(1)),
            session(3, "a", Some(" "), None, None, Some(1)),
            session(4, "a", Some("repo"), None, None, Some(1)),
        ];
        let filters = RefineFilters {
            repositories: ["repo".to_owned()].into_iter().collect(),
            ..RefineFilters::default()
        };
        assert_eq!(facet_options(&sessions, Facet::Repository), vec!["repo"]);
        assert_eq!(
            filter_sessions(&sessions, &filters, 10)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![4]
        );
    }
    #[test]
    fn date_buckets_use_started_at_boundaries_and_multi_select_or() {
        let anchor = 1_000_000;
        let cases = [
            (anchor - DAY_SECONDS, DateBucket::Within24Hours),
            (anchor - DAY_SECONDS - 1, DateBucket::TwoToSevenDays),
            (anchor - 7 * DAY_SECONDS - 1, DateBucket::EightToThirtyDays),
            (
                anchor - 30 * DAY_SECONDS - 1,
                DateBucket::OlderThanThirtyDays,
            ),
            (anchor + 1, DateBucket::Future),
        ];
        for (timestamp, expected) in cases {
            assert_eq!(
                date_bucket(&session(1, "a", None, None, None, Some(timestamp)), anchor),
                Some(expected)
            );
        }
        assert_eq!(
            date_bucket(&session(1, "a", None, None, None, None), anchor),
            None
        );
        let mut started_wins = session(2, "a", None, None, None, Some(anchor - 1));
        started_wins.last_visible_event_at = Some(anchor - 8 * DAY_SECONDS);
        assert_eq!(
            date_bucket(&started_wins, anchor),
            Some(DateBucket::Within24Hours)
        );
        let filters = RefineFilters {
            dates: [DateBucket::Within24Hours, DateBucket::Future]
                .into_iter()
                .collect(),
            ..RefineFilters::default()
        };
        let sessions = vec![
            session(1, "a", None, None, None, Some(anchor - 1)),
            session(2, "a", None, None, None, Some(anchor + 1)),
            session(3, "a", None, None, None, Some(anchor - 8 * DAY_SECONDS)),
            session(4, "a", None, None, None, None),
        ];
        assert_eq!(
            filter_sessions(&sessions, &filters, anchor)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
    #[test]
    fn grouping_is_first_seen_and_members_keep_recent_order() {
        let sessions = vec![
            session(1, "b", Some("r2"), None, None, Some(3)),
            session(2, "a", Some("r1"), None, None, Some(2)),
            session(3, "b", Some("r2"), None, None, Some(1)),
        ];
        let rows = group_sessions(&sessions, Grouping::Provider);
        assert!(
            matches!(&rows[0], VisibleRow::Header(GroupHeader::Provider(value)) if value == "b")
        );
        assert_eq!(
            rows.iter()
                .filter_map(|row| match row {
                    VisibleRow::Session(session) => Some(session.id),
                    VisibleRow::Header(_) => None,
                })
                .collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
    }
    #[test]
    fn repository_grouping_keeps_missing_distinct_from_the_literal_label() {
        let sessions = vec![
            session(1, "a", None, None, None, Some(2)),
            session(2, "a", Some("(no repository)"), None, None, Some(1)),
        ];
        let rows = group_sessions(&sessions, Grouping::Repository);
        assert!(matches!(
            &rows[0],
            VisibleRow::Header(GroupHeader::Repository(RepositoryGroupKey::Missing))
        ));
        assert!(matches!(
            &rows[2],
            VisibleRow::Header(GroupHeader::Repository(RepositoryGroupKey::Present(value))) if value == "(no repository)"
        ));
    }
    #[test]
    fn stable_identity_survives_reloaded_row_id() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        let selected = state.selected.clone();
        let mut reloaded = session(99, "a", None, None, None, Some(1));
        reloaded.source_locator = "/source/1".to_owned();
        reloaded.session_key = "key-1".to_owned();
        state.replace_sessions(vec![reloaded]);
        assert_eq!(state.selected, selected);
        assert_eq!(state.selected_id(), Some(99));
    }
    #[test]
    fn refine_toggle_persists_when_escaped_and_no_match_clears_selection() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        state.overlay = Overlay::Refine(RefineStep::Provider);
        update(&mut state, BrowseEvent::Toggle);
        update(&mut state, BrowseEvent::Back);
        assert!(state.filters.providers.contains("a"));
        state.filters.providers = ["not-present".to_owned()].into_iter().collect();
        state.rebuild();
        assert!(state.selected.is_none());
        assert!(state.rows.is_empty());
    }

    #[test]
    fn reload_preserves_the_stable_selection_when_the_row_id_changes() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        assert_eq!(update(&mut state, BrowseEvent::Reload), BrowseEffect::Sync);
        let mut replacement = session(22, "a", None, None, None, Some(1));
        replacement.source_locator = "/source/1".to_owned();
        replacement.session_key = "key-1".to_owned();
        state.set_preview(preview(session(1, "a", None, None, None, Some(1))));
        assert_eq!(state.replace_sessions(vec![replacement]), Some(22));
        assert_eq!(state.selected_id(), Some(22));
        assert!(state.preview.is_none());
    }
    #[test]
    fn headers_are_not_selectable() {
        let mut state = BrowseState::new(10);
        state.grouping = Grouping::Provider;
        state.replace_sessions(vec![
            session(1, "a", None, None, None, Some(1)),
            session(2, "b", None, None, None, Some(1)),
        ]);
        assert_eq!(state.selected_id(), Some(1));
        state.move_selection(1);
        assert_eq!(state.selected_id(), Some(2));
    }
    #[test]
    fn overlay_precedence_keeps_refine_above_preview() {
        let mut state = BrowseState::new(10);
        state.view = View::Preview;
        update(&mut state, BrowseEvent::OpenRefine);
        assert!(matches!(state.overlay, Overlay::Refine(_)));
        update(&mut state, BrowseEvent::Back);
        assert_eq!(state.view, View::Preview);
    }

    #[test]
    fn overlays_clear_their_full_rectangles_before_rendering() {
        let sessions = (0_i64..30)
            .map(|id| {
                let mut session = session(id, "a", None, None, None, Some(30 - id));
                session.title = Some("UNDERLYING CATALOG CONTENT".to_owned());
                session
            })
            .collect::<Vec<_>>();
        for overlay in [
            Overlay::Refine(RefineStep::Provider),
            Overlay::Help,
            Overlay::Diagnostics,
        ] {
            let mut state = BrowseState::new(10);
            state.replace_sessions(sessions.clone());
            state.overlay = overlay;
            let text =
                rendered_area_text(&mut state, 80, 24, overlay_area(Rect::new(0, 0, 80, 24)));
            assert!(!text.contains("UNDERLYING CATALOG CONTENT"));
        }
    }
    #[test]
    fn refine_list_scrolls_to_keep_a_long_facet_cursor_visible() {
        let sessions = (0_i64..30)
            .map(|id| session(id, &format!("provider-{id:02}"), None, None, None, Some(id)))
            .collect();
        let mut state = BrowseState::new(10);
        state.replace_sessions(sessions);
        state.overlay = Overlay::Refine(RefineStep::Provider);
        state.refine_cursor = 29;
        let rendered = rendered_text(&mut state, 80, 12);
        assert!(state.refine_offset > 0);
        assert!(rendered.contains("provider-29"));
        assert!(rendered.contains("Space toggle"));
    }
    #[test]
    fn renderer_handles_tiny_and_wide_layout_breakpoints() {
        let mut state = BrowseState::new(10);
        let selected = session(1, "a", None, None, None, Some(1));
        state.replace_sessions(vec![selected.clone()]);
        state.set_preview(preview(selected));
        let _ = rendered_text(&mut state, 1, 1);
        assert!(rendered_text(&mut state, 20, 5).contains("Terminal too small"));
        assert!(!rendered_text(&mut state, 99, 24).contains("A visible request"));
        assert!(rendered_text(&mut state, 100, 24).contains("A visible request"));
    }

    #[test]
    fn renderer_uses_packet_rows_and_untitled_fallback_at_wide_and_narrow_widths() {
        let mut selected = session(1, "codex", Some("repo"), None, None, Some(0));
        selected.title = None;
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![selected]);
        let wide = rendered_text(&mut state, 100, 24);
        assert!(wide.contains("1970-01-01 00:00Z"));
        assert!(wide.contains("Untitled session"));
        assert!(wide.contains("repo"));
        assert!(wide.contains("codex"));
        let narrow = rendered_text(&mut state, 80, 24);
        assert!(narrow.contains("1970-01-01 00:00Z"));
        assert!(narrow.contains("Untitled session"));
        assert!(narrow.contains("codex"));
        assert!(!narrow.contains(" | repo | "));
    }

    #[test]
    fn started_timestamp_is_concise_utc_with_time() {
        assert_eq!(format_started(Some(0)), "1970-01-01 00:00Z");
        assert_eq!(format_started(Some(90 * 60)), "1970-01-01 01:30Z");
        assert_eq!(format_started(Some(-60)), "1969-12-31 23:59Z");
        assert_eq!(format_started(None), "Unknown UTC");
    }

    #[test]
    fn long_titles_do_not_clip_required_wide_or_narrow_row_fields() {
        let mut selected = session(
            1,
            "provider-非常に長い識別子",
            Some("repository-非常に長い識別子"),
            None,
            None,
            Some(0),
        );
        selected.title = Some("日本語のとても長いタイトルと🐈emoji ".repeat(12));
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![selected]);

        let wide = rendered_text(&mut state, 100, 24);
        let wide_row = session_row(&state.filtered[0], true, 58);
        assert!(cell_width(&wide_row) <= 58);
        assert!(wide.contains("repository"));
        assert!(wide.contains("provider"));
        assert!(wide.contains('…'));

        let narrow = rendered_text(&mut state, 80, 24);
        let narrow_row = session_row(&state.filtered[0], false, 76);
        assert!(cell_width(&narrow_row) <= 76);
        assert!(narrow.contains("provider"));
        assert!(narrow.contains('…'));
        assert!(!narrow.contains("repository"));
    }

    #[test]
    fn preview_back_preserves_list_context_and_preview_scroll() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(
            (0_i64..40)
                .map(|id| session(id, "a", None, None, None, Some(id)))
                .collect(),
        );
        state.selected = Some(StableIdentity::from_session(&state.filtered[20]));
        state.list_offset = 15;
        state.set_preview(preview(state.filtered[20].clone()));
        update(&mut state, BrowseEvent::Next);
        update(&mut state, BrowseEvent::PageDown);
        let preview_scroll = state.preview_scroll;

        update(&mut state, BrowseEvent::Back);

        assert_eq!(state.view, View::Browse);
        assert_eq!(state.selected_id(), Some(20));
        assert_eq!(state.list_offset, 15);
        assert_eq!(state.preview_scroll, preview_scroll);
    }

    #[test]
    fn renderer_shows_no_match_and_diagnostics_without_replacing_sessions() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        state.filters.providers = ["no-match".to_owned()].into_iter().collect();
        state.rebuild();
        assert!(rendered_text(&mut state, 80, 24).contains("No matching sessions"));
        state.filters.providers.clear();
        state.rebuild();
        state.selected = Some(StableIdentity {
            provider: "a\u{1b}]8;;bad\u{7}".to_owned(),
            source_format: "jsonl".to_owned(),
            source_locator: "/source/1\u{1b}[2J".to_owned(),
            session_key: "key-1".to_owned(),
        });

        state.diagnostics = Some(vec![
            CatalogSourceDiagnostic {
                provider: "a\u{1b}]8;;bad\u{7}".to_owned(),
                source_format: "jsonl".to_owned(),
                source_locator: "/source/1\u{1b}[2J".to_owned(),
                diagnostic_status: "error".to_owned(),
                diagnostic_message: Some("broken\u{9b} input".to_owned()),
                diagnostic_recorded_at: Some(1),
                last_success_at: None,
            },
            CatalogSourceDiagnostic {
                provider: "gemini".to_owned(),
                source_format: "jsonl".to_owned(),
                source_locator: "/source/missing".to_owned(),
                diagnostic_status: "missing".to_owned(),
                diagnostic_message: Some("not discovered".to_owned()),
                diagnostic_recorded_at: Some(2),
                last_success_at: Some(1),
            },
        ]);
        state.overlay = Overlay::Diagnostics;
        let diagnostics = rendered_text(&mut state, 80, 24);
        assert!(diagnostics.contains("Sources: 2 | ok=0 partial=0 error=1 missing=1"));
        assert!(diagnostics.contains("broken\\u{009B} input"));
        assert!(diagnostics.contains("> [a\\u{001B}]8;;bad\\u{0007}:jsonl]"));
        assert!(!diagnostics.contains('\u{1b}'));
        assert!(!diagnostics.contains('\u{9b}'));
        assert_eq!(state.loaded.len(), 1);
    }

    #[test]
    fn diagnostics_overlay_scrolls_without_changing_the_session_selection() {
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![session(1, "a", None, None, None, Some(1))]);
        state.overlay = Overlay::Diagnostics;
        let selected = state.selected.clone();
        update(&mut state, BrowseEvent::Down);
        update(&mut state, BrowseEvent::PageDown);
        assert!(state.diagnostics_scroll > 0);
        assert_eq!(state.selected, selected);
        update(&mut state, BrowseEvent::Top);
        assert_eq!(state.diagnostics_scroll, 0);
        update(&mut state, BrowseEvent::Back);
        assert_eq!(state.overlay, Overlay::None);
    }

    #[test]
    fn diagnostics_bottom_clamps_to_the_last_visible_content_line() {
        let mut state = BrowseState::new(10);
        state.diagnostics = Some(vec![CatalogSourceDiagnostic {
            provider: "provider".to_owned(),
            source_format: "jsonl".to_owned(),
            source_locator: format!("{}locator-tail", "very-long-locator-".repeat(12)),
            diagnostic_status: "error".to_owned(),
            diagnostic_message: Some(format!("{}message-tail", "very long message ".repeat(18))),
            diagnostic_recorded_at: None,
            last_success_at: None,
        }]);
        state.overlay = Overlay::Diagnostics;
        let _ = rendered_text(&mut state, 80, 12);
        update(&mut state, BrowseEvent::Bottom);
        assert!(state.diagnostics_scroll < u16::MAX);
        let rendered = rendered_text(&mut state, 80, 12);
        assert!(rendered.contains("message-tail"));
        assert!(state.diagnostics_scroll < u16::MAX);
    }

    #[test]
    fn renderer_makes_all_visible_catalog_text_inert() {
        let mut selected = session(
            1,
            "provider\u{1b}]8;;https://bad\u{7}",
            Some("repo\u{1b}]52;c;bad\u{7}"),
            Some("model\u{9b}"),
            Some("kind\u{7}"),
            Some(1),
        );
        selected.title = Some("title\u{1b}]0;owned\u{7}".to_owned());
        selected.source_format = "format\u{9b}".to_owned();
        selected.session_key = "key\u{1b}]2;bad\u{7}".to_owned();
        selected.cwd = Some("cwd\u{9b}".to_owned());
        selected.source_diagnostic_status = "status\u{1b}[2J".to_owned();
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![selected.clone()]);
        state.set_preview(CatalogSessionPreview {
            session: selected,
            items: vec![
                CatalogItemView::UserText {
                    content: "first line\nsecond\u{1b}]0;bad\u{7}".to_owned(),
                },
                CatalogItemView::ToolMarker {
                    name: "tool\u{1b}]8;;bad\u{7}".to_owned(),
                    status: Some("status\u{9b}".to_owned()),
                },
            ],
            items_truncated: false,
        });
        state.view = View::Preview;

        let rendered = rendered_text(&mut state, 120, 30);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{9b}'));
        assert!(rendered.contains("\\u{001B}"));
        assert!(rendered.contains("\\u{009B}"));
        assert!(rendered.contains("user: first line"));
        assert!(rendered.contains("  second\\u{001B}]0;bad\\u{0007}"));
    }

    #[test]
    fn renderer_keeps_empty_transcript_items_visible() {
        let selected = session(1, "a", None, None, None, Some(1));
        let mut state = BrowseState::new(10);
        state.replace_sessions(vec![selected.clone()]);
        state.set_preview(CatalogSessionPreview {
            session: selected,
            items: vec![CatalogItemView::UserText {
                content: String::new(),
            }],
            items_truncated: false,
        });
        state.view = View::Preview;
        assert!(rendered_text(&mut state, 80, 12).contains("user:"));
    }

    #[test]
    fn process_wide_terminal_lock_rejects_a_nested_session() {
        let _serialized = serialize_lifecycle_test();
        let first = acquire_terminal_session_lock().expect("acquire first terminal lock");
        let second = acquire_terminal_session_lock().expect_err("reject nested terminal lock");
        assert!(second.to_string().contains("already active"));
        drop(first);
    }

    #[test]
    fn caught_panic_restores_the_hook_and_releases_the_next_session_lock() {
        let _serialized = serialize_lifecycle_test();
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let mut hook_state = None;
        let first_panic = panic::catch_unwind(AssertUnwindSafe(|| {
            let _session_lock = acquire_terminal_session_lock().expect("acquire session lock");
            let hook = PanicHookRestorer::install();
            hook_state = Some(Arc::clone(&hook.previous));
            let _hook = hook;
            panic!("test panic while the interactive hook is active");
        }));
        let hook_was_restored = hook_state
            .as_ref()
            .and_then(|state| state.lock().ok().map(|previous| previous.is_none()))
            .unwrap_or(false);
        let next_session = acquire_terminal_session_lock();
        let second_panic = panic::catch_unwind(AssertUnwindSafe(|| panic!("test next session")));
        panic::set_hook(original_hook);

        assert!(first_panic.is_err());
        assert!(hook_was_restored);
        assert!(next_session.is_ok());
        assert!(second_panic.is_err());
    }
}
