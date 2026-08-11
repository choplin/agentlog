//! Command-line transport for Agentlog application workflows.

use std::{
    env,
    fs::OpenOptions,
    io::{self, IsTerminal},
    path::PathBuf,
};

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};

use crate::{
    app::{
        SyncProgress, list_shell, paths_report, preview_purge_catalog, purge_catalog_shell,
        purge_previewed_catalog, show_shell, sync_shell_with_progress,
    },
    display::visible_text,
    paths::AppPaths,
    storage::CatalogItemView,
    tui,
};

/// Browse local AI coding-agent history without changing provider-owned files.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Agentlog-owned home directory. Intended for tests and isolated installs.
    #[arg(long, global = true, env = "AGENTLOG_HOME", value_name = "DIR")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show Agentlog-owned paths and the separate provider-source boundary.
    Paths {
        /// Emit a machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Synchronize supported provider history without changing provider-owned files.
    Sync {
        /// Emit a machine-readable result.
        #[arg(long)]
        json: bool,
    },
    /// List already-cataloged sessions without synchronizing provider-owned files.
    List {
        /// Maximum number of sessions to display.
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=500))]
        limit: u32,
        /// Emit a machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Preview one already-cataloged session without synchronizing provider-owned files.
    Show {
        /// Catalog session ID from `agentlog list`.
        session_id: i64,
        /// Emit a machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Preview and interactively confirm, or with --yes purge, the Agentlog-owned catalog.
    Purge {
        /// Purge the reported Agentlog-owned catalog contents.
        #[arg(long)]
        yes: bool,
    },
    /// Browse already-cataloged sessions in an interactive terminal UI.
    Browse,
}

/// Parses process arguments and dispatches one command-line workflow.
///
/// # Errors
///
/// Returns an error when path resolution, an application workflow, or output
/// serialization fails.
pub async fn run() -> anyhow::Result<()> {
    let Cli { home, command } = Cli::parse();
    let Some(command) = command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    let paths = AppPaths::resolve(home).context("resolve Agentlog-owned paths")?;

    match command {
        Command::Paths { json } => print_paths(&paths, json),
        Command::Sync { json } => print_sync(&paths, json).await,
        Command::List { limit, json } => print_list(&paths, limit, json).await,
        Command::Show { session_id, json } => print_show(&paths, session_id, json).await,
        Command::Purge { yes } => print_purge(&paths, yes).await,
        Command::Browse => tui::run(&paths).await,
    }
}

async fn print_purge(paths: &AppPaths, yes: bool) -> anyhow::Result<()> {
    let prompt_eligible = !yes && interactive_terminal();
    let rich_report = prompt_eligible || cliclack_chrome_allowed();

    if rich_report {
        cliclack::intro("Purge Agentlog catalog").context("start purge report")?;
    }

    if yes {
        let summary = purge_catalog_shell(paths, true).await?;
        print_purge_report(&summary, "Purged catalog", rich_report)?;
        print_purge_completion(&summary, rich_report)?;
        return Ok(());
    }

    let preview = preview_purge_catalog(paths).await?;
    print_purge_report(&preview.summary, "Catalog preview", rich_report)?;
    match confirm_purge_from_terminal(&preview.summary, prompt_eligible)? {
        Some(true) => {
            let summary = purge_previewed_catalog(paths, preview).await?;
            print_purge_completion(&summary, rich_report)?;
        }
        Some(false) if rich_report => {
            cliclack::outro_cancel("Purge canceled; no changes were made.")
                .context("finish canceled purge report")?;
        }
        Some(false) => eprintln!("Purge canceled; no changes were made."),
        None if rich_report => {
            cliclack::outro_cancel(
                "No changes were made. Re-run with `agentlog purge --yes` to confirm.",
            )
            .context("finish unconfirmed purge report")?;
        }
        None => {
            eprintln!("No changes were made. Re-run with `agentlog purge --yes` to confirm.");
        }
    }
    Ok(())
}

fn print_purge_report(
    summary: &crate::app::PurgeSummary,
    heading: &str,
    rich_report: bool,
) -> anyhow::Result<()> {
    let body = format!(
        "Agentlog-owned database target: {}\nCatalog sources: {}\nCatalog sessions: {}\nTranscript items: {}\nApproximate database size: {} ({} bytes)",
        visible_text(&summary.database.display().to_string()),
        summary.sources,
        summary.sessions,
        summary.transcript_items,
        human_readable_bytes(summary.approximate_bytes),
        summary.approximate_bytes
    );
    let warning = "Catalog data may not be reconstructible: only provider logs that still exist and remain readable can be synchronized.";

    if rich_report {
        cliclack::note(heading, body).context("write purge catalog report")?;
        cliclack::log::warning(cliclack::termwrap(warning, 3)).context("write purge warning")?;
    } else {
        eprintln!("{body}");
        eprintln!("Warning: {warning}");
    }
    Ok(())
}

fn print_purge_completion(
    summary: &crate::app::PurgeSummary,
    rich_report: bool,
) -> anyhow::Result<()> {
    let message = if summary.cleared {
        format!(
            "Purge complete: cleared {} catalog sources, {} sessions, and {} transcript items from the locked catalog state ({}; {} bytes).",
            summary.sources,
            summary.sessions,
            summary.transcript_items,
            human_readable_bytes(summary.approximate_bytes),
            summary.approximate_bytes
        )
    } else {
        "Purge complete: no Agentlog-owned database was present.".to_owned()
    };
    if rich_report {
        cliclack::outro(message).context("finish purge report")?;
    } else {
        eprintln!("{message}");
    }
    Ok(())
}

/// Prompts only through the controlling terminal, never through piped input.
///
/// An unavailable terminal intentionally leaves purge in preview-only mode.
fn confirm_purge_from_terminal(
    summary: &crate::app::PurgeSummary,
    prompt_eligible: bool,
) -> anyhow::Result<Option<bool>> {
    if !prompt_eligible {
        return Ok(None);
    }

    cliclack::confirm(purge_confirmation_message(summary))
        .initial_value(false)
        .interact()
        .map(Some)
        .context("confirm catalog purge")
}

fn cliclack_chrome_allowed() -> bool {
    let term = env::var("TERM").ok();
    terminal_chrome_allowed(io::stderr().is_terminal(), term.as_deref())
}

fn purge_confirmation_message(summary: &crate::app::PurgeSummary) -> String {
    format!(
        "Purge {} catalog sources, {} sessions, and {} transcript items now?",
        summary.sources, summary.sessions, summary.transcript_items
    )
}

fn interactive_terminal() -> bool {
    let stdin = io::stdin().is_terminal();
    let stdout = io::stdout().is_terminal();
    let stderr = io::stderr().is_terminal();
    if !(stdin && stdout && stderr) {
        return false;
    }
    let Ok(terminal) = OpenOptions::new().read(true).open("/dev/tty") else {
        return false;
    };
    let foreground = rustix::termios::tcgetpgrp(&terminal)
        .is_ok_and(|group| group == rustix::process::getpgrp());
    PromptEligibility {
        stdio: [stdin, stdout, stderr],
        foreground,
    }
    .allows_prompt()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PromptEligibility {
    stdio: [bool; 3],
    foreground: bool,
}

impl PromptEligibility {
    const fn allows_prompt(self) -> bool {
        self.stdio[0] && self.stdio[1] && self.stdio[2] && self.foreground
    }
}

fn human_readable_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut unit = 0;
    let mut unit_bytes = 1_u64;
    while unit < UNITS.len() - 1 && bytes >= unit_bytes * 1024 {
        unit += 1;
        unit_bytes *= 1024;
    }
    let rounded_tenths =
        (u128::from(bytes) * 10 + u128::from(unit_bytes) / 2) / u128::from(unit_bytes);
    format!(
        "{}.{} {}",
        rounded_tenths / 10,
        rounded_tenths % 10,
        UNITS[unit]
    )
}

fn print_paths(paths: &AppPaths, json: bool) -> anyhow::Result<()> {
    let report = paths_report(paths);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Agentlog-owned home: {}",
            visible_text(&report.agentlog_home.display().to_string())
        );
        println!(
            "Agentlog-owned config: {}",
            visible_text(&report.config_file.display().to_string())
        );
        println!(
            "Agentlog-owned data: {}",
            visible_text(&report.data_directory.display().to_string())
        );
        println!(
            "Agentlog-owned database: {}",
            visible_text(&report.database.display().to_string())
        );
        println!(
            "Provider-owned sources: {}",
            visible_text(&report.provider_sources)
        );
    }
    Ok(())
}

async fn print_sync(paths: &AppPaths, json: bool) -> anyhow::Result<()> {
    let mut reporter = SyncProgressReporter::new();
    if reporter.interactive {
        cliclack::intro("Synchronize provider history").context("start sync report")?;
    }
    let summary = sync_shell_with_progress(paths, |progress| reporter.report(progress)).await?;
    if json {
        if reporter.interactive {
            cliclack::outro(format!("Sync result: {}", visible_text(&summary.message)))
                .context("finish sync report")?;
        }
        println!("{}", serde_json::to_string(&summary)?);
    } else if reporter.interactive {
        print_rich_sync_summary(&summary)?;
    } else {
        eprintln!("Schema version: {}", summary.schema_version);
        eprintln!("Collectors installed: {}", summary.collectors_installed);
        eprintln!("Providers failed: {}", summary.providers_failed);
        eprintln!("Sources refreshed: {}", summary.sources_refreshed);
        eprintln!("Sources partial: {}", summary.sources_partial);
        eprintln!("Sources failed: {}", summary.sources_failed);
        eprintln!("Sources missing: {}", summary.sources_missing);
        eprintln!("Sessions available: {}", summary.sessions_available);
        for provider in &summary.provider_summaries {
            eprintln!(
                "Provider {}: candidates={}, refreshed={}, partial={}, failed={}, missing={}, sessions={}",
                visible_text(&provider.provider),
                provider.candidate_sources,
                provider.refreshed_sources,
                provider.partial_sources,
                provider.failed_sources,
                provider.missing_sources,
                provider.sessions_available
            );
            if let Some(failure) = &provider.failure {
                eprintln!("  warning: {}", visible_text(failure));
            }
        }
        eprintln!("Sync result: {}", visible_text(&summary.message));
    }
    Ok(())
}

fn print_rich_sync_summary(summary: &crate::app::SyncSummary) -> anyhow::Result<()> {
    let mut lines = vec![
        format!("Schema version: {}", summary.schema_version),
        format!("Collectors installed: {}", summary.collectors_installed),
        format!("Providers failed: {}", summary.providers_failed),
        format!("Sources refreshed: {}", summary.sources_refreshed),
        format!("Sources partial: {}", summary.sources_partial),
        format!("Sources failed: {}", summary.sources_failed),
        format!("Sources missing: {}", summary.sources_missing),
        format!("Sessions available: {}", summary.sessions_available),
    ];
    for provider in &summary.provider_summaries {
        lines.push(format!(
            "Provider {}: candidates={}, refreshed={}, partial={}, failed={}, missing={}, sessions={}",
            visible_text(&provider.provider),
            provider.candidate_sources,
            provider.refreshed_sources,
            provider.partial_sources,
            provider.failed_sources,
            provider.missing_sources,
            provider.sessions_available
        ));
        if let Some(failure) = &provider.failure {
            lines.push(format!("  Warning: {}", visible_text(failure)));
        }
    }
    cliclack::note("Sync summary", lines.join("\n")).context("write sync summary")?;
    cliclack::outro(format!("Sync result: {}", visible_text(&summary.message)))
        .context("finish sync report")?;
    Ok(())
}

/// Writes compact synchronization progress to stderr without touching stdout.
struct SyncProgressReporter {
    interactive: bool,
    active: Option<cliclack::ProgressBar>,
}

impl SyncProgressReporter {
    fn new() -> Self {
        let term = env::var("TERM").ok();
        Self {
            interactive: terminal_control_allowed(
                std::io::stderr().is_terminal(),
                term.as_deref(),
                env::var_os("NO_COLOR").is_some(),
            ),
            active: None,
        }
    }

    fn report(&mut self, progress: SyncProgress) {
        match progress {
            SyncProgress::ProviderDiscovering { provider } => {
                if self.interactive {
                    let progress = cliclack::spinner();
                    progress.start(format!(
                        "Provider {}: discovering sources",
                        visible_text(&provider)
                    ));
                    self.active = Some(progress);
                } else {
                    eprintln!(
                        "[~] Provider {}: discovering sources",
                        visible_text(&provider)
                    );
                }
            }
            SyncProgress::ProviderCandidates {
                provider,
                candidate_sources,
            } => {
                if self.interactive {
                    self.stop_active(format!(
                        "Provider {}: discovered {} candidate sources",
                        visible_text(&provider),
                        candidate_sources
                    ));
                    let progress = cliclack::progress_bar(candidate_sources);
                    progress.start(format!(
                        "Provider {}: synchronizing sources",
                        visible_text(&provider)
                    ));
                    self.active = Some(progress);
                } else {
                    eprintln!(
                        "[~] Provider {}: starting {} candidate sources",
                        visible_text(&provider),
                        candidate_sources
                    );
                }
            }
            SyncProgress::SourceStaged {
                provider: _,
                processed_sources,
                candidate_sources: _,
            } => {
                if self.interactive {
                    if let Some(progress) = &self.active {
                        progress.set_position(processed_sources);
                    }
                }
            }
            SyncProgress::ProviderCompleted {
                provider,
                report,
                failure,
            } => {
                if let Some(failure) = failure {
                    self.error_active(format!(
                        "Provider {}: failed ({})",
                        visible_text(&provider),
                        visible_text(&failure)
                    ));
                } else {
                    let message = format!(
                        "Provider {}: candidates={}, refreshed={}, partial={}, failed={}, missing={}",
                        visible_text(&provider),
                        report.candidate_sources,
                        report.refreshed_sources,
                        report.partial_sources,
                        report.failed_sources,
                        report.missing_sources
                    );
                    if self.interactive {
                        self.complete_active(message);
                    } else {
                        eprintln!("[+] {message}");
                    }
                }
            }
        }
    }

    fn stop_active(&mut self, message: String) {
        if let Some(progress) = self.active.take() {
            progress.stop(message);
        } else {
            eprintln!("[~] {message}");
        }
    }

    fn error_active(&mut self, message: String) {
        if let Some(progress) = self.active.take() {
            progress.error(message);
        } else {
            eprintln!("[!] {message}");
        }
    }

    fn complete_active(&mut self, message: String) {
        if let Some(progress) = self.active.take() {
            progress.stop(message);
        } else {
            eprintln!("[+] {message}");
        }
    }
}

/// Returns whether stderr supports the live cursor control used by cliclack.
///
/// A plain report is safer for a terminal without a declared terminal type, a
/// dumb terminal, or when the user has explicitly disabled terminal styling.
fn terminal_control_allowed(stderr_is_terminal: bool, term: Option<&str>, no_color: bool) -> bool {
    terminal_chrome_allowed(stderr_is_terminal, term) && !no_color
}

fn terminal_chrome_allowed(stderr_is_terminal: bool, term: Option<&str>) -> bool {
    stderr_is_terminal && term.is_some_and(|term| !term.trim_ascii().is_empty() && term != "dumb")
}

impl Drop for SyncProgressReporter {
    fn drop(&mut self) {
        if let Some(progress) = self.active.take() {
            progress.clear();
        }
    }
}

async fn print_list(paths: &AppPaths, limit: u32, json: bool) -> anyhow::Result<()> {
    let sessions = list_shell(paths, limit).await?;
    if json {
        println!("{}", serde_json::to_string(&sessions)?);
    } else if sessions.is_empty() {
        println!("No cataloged sessions. Run `agentlog sync` to synchronize sources.");
    } else {
        for session in sessions {
            let title = session
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(&session.session_key);
            let freshness = if session.source_diagnostic_status == "error" {
                "stale/error"
            } else {
                session.source_diagnostic_status.as_str()
            };
            println!(
                "{}  [{}:{}; {}] {}",
                session.id,
                visible_text(&session.provider),
                visible_text(&session.source_format),
                visible_text(freshness),
                visible_text(title)
            );
        }
    }
    Ok(())
}

async fn print_show(paths: &AppPaths, session_id: i64, json: bool) -> anyhow::Result<()> {
    let preview = show_shell(paths, session_id).await?;
    if json {
        println!("{}", serde_json::to_string(&preview)?);
    } else {
        let freshness = &preview.session.source_diagnostic_status;
        println!(
            "Session {} [{}:{}; source status: {}]",
            preview.session.id,
            visible_text(&preview.session.provider),
            visible_text(&preview.session.source_format),
            visible_text(freshness)
        );
        let title = preview
            .session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&preview.session.session_key);
        println!("{}", visible_text(title));
        for item in preview.items {
            match item {
                CatalogItemView::UserText { content } => {
                    print_visible_transcript("user", &content);
                }
                CatalogItemView::AssistantText { content } => {
                    print_visible_transcript("assistant", &content);
                }
                CatalogItemView::ToolMarker { name, status } => {
                    println!(
                        "tool: {} ({})",
                        visible_text(&name),
                        visible_text(status.as_deref().unwrap_or("unknown"))
                    );
                }
            }
        }
        if preview.items_truncated {
            println!("Preview truncated after 80 items.");
        }
    }
    Ok(())
}

fn print_visible_transcript(label: &str, content: &str) {
    if content.is_empty() {
        println!("{label}:");
        return;
    }

    for (index, line) in content.lines().enumerate() {
        let prefix = if index == 0 {
            format!("{label}: ")
        } else {
            "  ".to_owned()
        };
        println!("{prefix}{}", visible_text(line));
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        Cli, Command, PromptEligibility, human_readable_bytes, purge_confirmation_message,
        terminal_chrome_allowed, terminal_control_allowed,
    };

    #[test]
    fn database_sizes_use_readable_binary_units() {
        assert_eq!(human_readable_bytes(0), "0 B");
        assert_eq!(human_readable_bytes(1023), "1023 B");
        assert_eq!(human_readable_bytes(1024), "1.0 KiB");
        assert_eq!(human_readable_bytes(128_339_968), "122.4 MiB");
        assert_eq!(human_readable_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn purge_accepts_preview_and_explicit_confirmation() {
        let preview = Cli::try_parse_from(["agentlog", "purge"]).expect("parse purge preview");
        assert!(matches!(
            preview.command,
            Some(Command::Purge { yes: false })
        ));

        let confirmed =
            Cli::try_parse_from(["agentlog", "purge", "--yes"]).expect("parse confirmed purge");
        assert!(matches!(
            confirmed.command,
            Some(Command::Purge { yes: true })
        ));
    }

    #[test]
    fn purge_rejects_legacy_data_clear_and_dry_run_forms() {
        assert!(Cli::try_parse_from(["agentlog", "data", "clear"]).is_err());
        assert!(Cli::try_parse_from(["agentlog", "purge", "--dry-run"]).is_err());
    }

    #[test]
    fn purge_confirmation_message_names_the_previewed_blast_radius() {
        let summary = crate::app::PurgeSummary {
            database: std::path::PathBuf::from("/tmp/agentlog.sqlite3"),
            sources: 2,
            sessions: 3,
            transcript_items: 5,
            approximate_bytes: 0,
            cleared: false,
        };
        assert_eq!(
            purge_confirmation_message(&summary),
            "Purge 2 catalog sources, 3 sessions, and 5 transcript items now?"
        );
    }

    #[test]
    fn purge_prompt_requires_all_three_standard_streams_to_be_terminals() {
        let eligible = PromptEligibility {
            stdio: [true, true, true],
            foreground: true,
        };
        assert!(eligible.allows_prompt());
        for ineligible in [
            PromptEligibility {
                stdio: [false, true, true],
                ..eligible
            },
            PromptEligibility {
                stdio: [true, false, true],
                ..eligible
            },
            PromptEligibility {
                stdio: [true, true, false],
                ..eligible
            },
            PromptEligibility {
                foreground: false,
                ..eligible
            },
        ] {
            assert!(!ineligible.allows_prompt());
        }
    }

    #[test]
    fn sync_progress_uses_cliclack_only_with_terminal_control() {
        assert!(terminal_control_allowed(
            true,
            Some("xterm-256color"),
            false
        ));
        for (stderr_is_terminal, term, no_color) in [
            (false, Some("xterm-256color"), false),
            (true, None, false),
            (true, Some(""), false),
            (true, Some(" \t"), false),
            (true, Some("dumb"), false),
            (true, Some("xterm-256color"), true),
        ] {
            assert!(
                !terminal_control_allowed(stderr_is_terminal, term, no_color),
                "stderr_tty={stderr_is_terminal}, term={term:?}, no_color={no_color} must use plain progress"
            );
        }
    }

    #[test]
    fn static_cliclack_chrome_requires_a_capable_stderr_terminal() {
        assert!(terminal_chrome_allowed(true, Some("xterm-256color")));
        for (stderr_is_terminal, term) in [
            (false, Some("xterm-256color")),
            (true, None),
            (true, Some("")),
            (true, Some(" \t")),
            (true, Some("dumb")),
        ] {
            assert!(!terminal_chrome_allowed(stderr_is_terminal, term));
        }
    }

    #[test]
    fn bare_root_keeps_the_command_empty_for_help_without_running_sync() {
        let cli = Cli::try_parse_from(["agentlog"]).expect("parse bare root command");
        assert!(cli.command.is_none());
    }

    #[test]
    fn sync_is_accepted_and_scan_is_rejected() {
        let sync = Cli::try_parse_from(["agentlog", "sync"]).expect("parse explicit sync command");
        assert!(matches!(sync.command, Some(Command::Sync { json: false })));
        assert!(Cli::try_parse_from(["agentlog", "scan"]).is_err());
    }
}
