//! Provider-native models and their Agentlog scanning adapters.

use std::{future::Future, path::PathBuf, pin::Pin};

use thiserror::Error;

use crate::{
    paths::ProviderRoots,
    storage::{SourceIdentity, SourceSnapshot},
};

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod kimi;
pub mod opencode;

/// Iterates the provider scanners installed in this Agentlog release.
#[must_use]
pub fn installed(roots: &ProviderRoots) -> InstalledProviders<'_> {
    InstalledProviders { roots, next: 0 }
}

/// Provider registry iterator that resolves each provider in scan order.
pub struct InstalledProviders<'a> {
    roots: &'a ProviderRoots,
    next: u8,
}

impl Iterator for InstalledProviders<'_> {
    type Item = Result<Box<dyn ProviderScanner>, ProviderRootError>;

    fn next(&mut self) -> Option<Self::Item> {
        let scanner: Result<Box<dyn ProviderScanner>, ProviderRootError> = match self.next {
            0 => codex::CodexProvider::resolve(self.roots.codex_root()).map(|provider| {
                Box::new(codex::CodexScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            1 => claude::ClaudeProvider::resolve(self.roots.claude_root()).map(|provider| {
                Box::new(claude::ClaudeScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            2 => opencode::OpenCodeProvider::resolve(self.roots.opencode_root()).map(|provider| {
                Box::new(opencode::OpenCodeScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            3 => gemini::GeminiProvider::resolve(self.roots.gemini_root()).map(|provider| {
                Box::new(gemini::GeminiScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            4 => cursor::CursorProvider::resolve(self.roots.cursor_root()).map(|provider| {
                Box::new(cursor::CursorScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            5 => kimi::KimiProvider::resolve(self.roots.kimi_root()).map(|provider| {
                Box::new(kimi::KimiScanner::new(provider)) as Box<dyn ProviderScanner>
            }),
            _ => return None,
        };
        self.next += 1;
        Some(scanner)
    }
}

/// Describes all provider-owned source locations for diagnostics.
#[must_use]
pub fn source_descriptions(roots: &ProviderRoots) -> String {
    let descriptions = [
        codex::CodexProvider::resolve(roots.codex_root()).map_or_else(
            |_| "Codex root (read-only): unavailable".to_owned(),
            |provider| format!("Codex root (read-only): {}", provider.root().display()),
        ),
        claude::ClaudeProvider::resolve(roots.claude_root()).map_or_else(
            |_| "Claude root (read-only): unavailable".to_owned(),
            |provider| format!("Claude root (read-only): {}", provider.root().display()),
        ),
        opencode::OpenCodeProvider::resolve(roots.opencode_root()).map_or_else(
            |_| "OpenCode roots (read-only): unavailable".to_owned(),
            |provider| {
                format!(
                    "OpenCode roots (read-only): {}",
                    provider
                        .roots()
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        ),
        gemini::GeminiProvider::resolve(roots.gemini_root()).map_or_else(
            |_| "Gemini root (read-only): unavailable".to_owned(),
            |provider| format!("Gemini root (read-only): {}", provider.root().display()),
        ),
        cursor::CursorProvider::resolve(roots.cursor_root()).map_or_else(
            |_| "Cursor root (read-only): unavailable".to_owned(),
            |provider| format!("Cursor root (read-only): {}", provider.root().display()),
        ),
        kimi::KimiProvider::resolve(roots.kimi_root()).map_or_else(
            |_| "Kimi root (read-only): unavailable".to_owned(),
            |provider| format!("Kimi root (read-only): {}", provider.root().display()),
        ),
    ];
    descriptions.join("; ")
}

/// Stable identity of a provider supported by this Agentlog release.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProviderId {
    Codex,
    Claude,
    OpenCode,
    Gemini,
    Cursor,
    Kimi,
}

impl ProviderId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Kimi => "kimi",
        }
    }
}

/// One normalized result produced while scanning a provider-owned source.
#[derive(Debug)]
pub enum SourceOutcome {
    Accepted(SourceSnapshot),
    Failed {
        identity: SourceIdentity,
        message: &'static str,
    },
}

/// Aggregate result of one provider scan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderScanReport {
    pub candidate_sources: u64,
    pub refreshed_sources: u64,
    pub partial_sources: u64,
    pub failed_sources: u64,
    pub missing_sources: u64,
}

/// A provider-specific adapter that starts one Agentlog scan.
pub trait ProviderScanner {
    /// Stable provider identity stored in the catalog.
    fn provider_id(&self) -> ProviderId;

    /// Discovers provider-native sources and creates a single-pass scan.
    ///
    /// # Errors
    ///
    /// Returns an error when source discovery cannot inspect the provider.
    fn start(&self) -> Result<Box<dyn ProviderScan + '_>, ProviderScanError>;
}

/// One in-progress, single-pass provider scan.
pub trait ProviderScan {
    /// Number of provider-owned sources discovered for this scan.
    fn candidate_sources(&self) -> u64;

    /// Produces the next normalized source outcome.
    fn next_outcome(&mut self) -> ProviderScanFuture<'_>;
}

/// Future returned by [`ProviderScan::next_outcome`].
pub type ProviderScanFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<SourceOutcome>, ProviderScanError>> + 'a>>;

/// Fatal failures that prevent a provider scan from continuing.
#[derive(Debug, Error)]
pub enum ProviderScanError {
    #[error("cannot inspect a provider-owned source: {0}")]
    SourceIo(#[from] std::io::Error),
}

/// Failures while resolving provider-owned source locations.
#[derive(Debug, Error)]
pub enum ProviderRootError {
    #[error(
        "cannot resolve the default Codex root because neither CODEX_HOME nor HOME is available"
    )]
    CodexHomeUnavailable,
    #[error("Codex root must be a nonempty absolute path, got {path}")]
    InvalidCodexRoot { path: PathBuf },
    #[error(
        "cannot resolve the default Claude root because neither CLAUDE_CONFIG_DIR nor HOME is available"
    )]
    ClaudeHomeUnavailable,
    #[error("Claude root must be a nonempty absolute path, got {path}")]
    InvalidClaudeRoot { path: PathBuf },
    #[error(
        "cannot resolve the default OpenCode root because XDG_DATA_HOME and HOME are unavailable"
    )]
    OpenCodeHomeUnavailable,
    #[error("OpenCode root must be a nonempty absolute path, got {path}")]
    InvalidOpenCodeRoot { path: PathBuf },
    #[error(
        "cannot resolve the default Gemini root because GEMINI_CLI_HOME and HOME are unavailable"
    )]
    GeminiHomeUnavailable,
    #[error("Gemini root must be a nonempty absolute path, got {path}")]
    InvalidGeminiRoot { path: PathBuf },
    #[error("cannot resolve the default Cursor root because HOME is unavailable")]
    CursorHomeUnavailable,
    #[error("Cursor root must be a nonempty absolute path, got {path}")]
    InvalidCursorRoot { path: PathBuf },
    #[error("cannot resolve the default Kimi root because KIMI_CODE_HOME and HOME are unavailable")]
    KimiHomeUnavailable,
    #[error("Kimi root must be a nonempty absolute path, got {path}")]
    InvalidKimiRoot { path: PathBuf },
}

impl ProviderRootError {
    #[must_use]
    pub const fn provider_id(&self) -> ProviderId {
        match self {
            Self::CodexHomeUnavailable | Self::InvalidCodexRoot { .. } => ProviderId::Codex,
            Self::ClaudeHomeUnavailable | Self::InvalidClaudeRoot { .. } => ProviderId::Claude,
            Self::OpenCodeHomeUnavailable | Self::InvalidOpenCodeRoot { .. } => {
                ProviderId::OpenCode
            }
            Self::GeminiHomeUnavailable | Self::InvalidGeminiRoot { .. } => ProviderId::Gemini,
            Self::CursorHomeUnavailable | Self::InvalidCursorRoot { .. } => ProviderId::Cursor,
            Self::KimiHomeUnavailable | Self::InvalidKimiRoot { .. } => ProviderId::Kimi,
        }
    }
}
