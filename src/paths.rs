//! Agentlog-owned paths and the deliberately small local configuration file.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

use crate::providers::{
    ProviderRootError, claude::ClaudeProvider, codex::CodexProvider, cursor::CursorProvider,
    gemini::GeminiProvider, kimi::KimiProvider, opencode::OpenCodeProvider,
};

const DEFAULT_DATA_HOME: &str = ".local/share/agentlog";
const CONFIG_FILE: &str = "config.toml";
const DATABASE_FILE: &str = "agentlog.sqlite3";

/// Locations that Agentlog owns and may create or modify.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    home: PathBuf,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
    provider_roots: ProviderRoots,
}

impl AppPaths {
    /// Resolves the platform-owned data location, or an explicit override.
    /// `AGENTLOG_HOME` has the same meaning as `--home`.
    ///
    /// # Errors
    ///
    /// Returns an error when no home directory is available, config is invalid,
    /// or a configured provider root is relative.
    pub fn resolve(home_override: Option<PathBuf>) -> Result<Self, PathError> {
        let home = resolve_agentlog_home(
            home_override,
            env::var_os("AGENTLOG_HOME"),
            env::var_os("XDG_DATA_HOME"),
            env::var_os("HOME"),
        )?;

        if home.as_os_str().is_empty() {
            return Err(PathError::EmptyHome);
        }

        let config_path = home.join(CONFIG_FILE);
        let provider_roots = Config::load(&config_path)?;

        Ok(Self {
            config_path,
            data_dir: home.clone(),
            database_path: home.join(DATABASE_FILE),
            provider_roots,
            home,
        })
    }

    /// Creates only directories owned by Agentlog. Provider sources are never
    /// created, inspected, or changed here.
    ///
    /// # Errors
    ///
    /// Returns an error when the Agentlog-owned directory cannot be created.
    pub fn ensure_data_dir(&self) -> Result<(), PathError> {
        fs::create_dir_all(&self.data_dir).map_err(|source| PathError::CreateDirectory {
            path: self.data_dir.clone(),
            source,
        })
    }

    /// The root that contains Agentlog config and data. Exposed for diagnostics.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Explicit provider-owned roots from the optional config. The six
    /// collectors use them only as read-only discovery roots.
    #[must_use]
    pub fn provider_roots(&self) -> &ProviderRoots {
        &self.provider_roots
    }

    /// Returns the read-only Codex root. Agentlog's `--home` setting never
    /// changes this provider-owned location.
    ///
    /// The precedence is explicit config, `CODEX_HOME`, then `~/.codex`.
    ///
    /// # Errors
    ///
    /// Returns an error only when no configured or environment root exists and
    /// the operating-system home directory is unavailable.
    pub fn codex_root(&self) -> Result<PathBuf, PathError> {
        resolve_codex_root(
            self.provider_roots.codex_root(),
            env::var_os("CODEX_HOME"),
            env::var_os("HOME"),
        )
    }

    /// Returns the read-only Claude configuration root.
    ///
    /// Claude Code stores project transcripts below `~/.claude/projects` by
    /// default. `CLAUDE_CONFIG_DIR` follows the CLI's configuration-root
    /// convention, so it is used before the operating-system default.
    /// Agentlog's `--home` setting never changes this provider-owned location.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured or environment root exists and the
    /// operating-system home directory is unavailable.
    pub fn claude_root(&self) -> Result<PathBuf, PathError> {
        resolve_claude_root(
            self.provider_roots.claude_root(),
            env::var_os("CLAUDE_CONFIG_DIR"),
            env::var_os("HOME"),
        )
    }

    /// Returns the read-only `OpenCode` data roots.
    ///
    /// `OpenCode` keeps its local catalog below the XDG data directory by
    /// default. Agentlog's `--home` setting never changes this
    /// provider-owned location.
    ///
    /// An explicit config root or a valid absolute
    /// `XDG_DATA_HOME/opencode` is authoritative. Without either, Agentlog uses
    /// the native default for the current OS: Application Support on macOS or
    /// the XDG HOME fallback on Linux.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured or environment root exists and the
    /// operating-system home directory is unavailable.
    pub fn opencode_roots(&self) -> Result<Vec<PathBuf>, PathError> {
        resolve_opencode_roots(
            self.provider_roots.opencode_root(),
            env::var_os("XDG_DATA_HOME"),
            env::var_os("HOME"),
        )
    }

    /// Returns the read-only `Gemini` CLI configuration root.
    ///
    /// The precedence is explicit config, `GEMINI_CLI_HOME/.gemini`, then
    /// `~/.gemini`. Agentlog's `--home` setting never changes this
    /// provider-owned location.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured or environment root exists and the
    /// operating-system home directory is unavailable.
    pub fn gemini_root(&self) -> Result<PathBuf, PathError> {
        resolve_gemini_root(
            self.provider_roots.gemini_root(),
            env::var_os("GEMINI_CLI_HOME"),
            env::var_os("HOME"),
        )
    }

    /// Returns the read-only `Cursor` configuration root.
    ///
    /// The precedence is explicit config, then `~/.cursor`. Agentlog's
    /// `--home` setting never changes this provider-owned location.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured root exists and the operating-system
    /// home directory is unavailable.
    pub fn cursor_root(&self) -> Result<PathBuf, PathError> {
        resolve_cursor_root(self.provider_roots.cursor_root(), env::var_os("HOME"))
    }

    /// Returns the read-only Kimi Code configuration root.
    ///
    /// The precedence is explicit config, `KIMI_CODE_HOME`, then
    /// `~/.kimi-code`. Agentlog's `--home` setting never changes this
    /// provider-owned location.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured or environment root exists and the
    /// operating-system home directory is unavailable.
    pub fn kimi_root(&self) -> Result<PathBuf, PathError> {
        resolve_kimi_root(
            self.provider_roots.kimi_root(),
            env::var_os("KIMI_CODE_HOME"),
            env::var_os("HOME"),
        )
    }
}

fn resolve_codex_root(
    configured: Option<&Path>,
    codex_home: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    CodexProvider::resolve_from(configured, codex_home, os_home)
        .map(|provider| provider.root().to_path_buf())
        .map_err(Into::into)
}

fn resolve_claude_root(
    configured: Option<&Path>,
    claude_config_dir: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    ClaudeProvider::resolve_from(configured, claude_config_dir, os_home)
        .map(|provider| provider.root().to_path_buf())
        .map_err(Into::into)
}

fn resolve_opencode_roots(
    configured: Option<&Path>,
    xdg_data_home: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<Vec<PathBuf>, PathError> {
    OpenCodeProvider::resolve_from(configured, xdg_data_home, os_home)
        .map(|provider| provider.roots().to_vec())
        .map_err(Into::into)
}

fn resolve_gemini_root(
    configured: Option<&Path>,
    gemini_cli_home: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    GeminiProvider::resolve_from(configured, gemini_cli_home, os_home)
        .map(|provider| provider.root().to_path_buf())
        .map_err(Into::into)
}
fn resolve_cursor_root(
    configured: Option<&Path>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    CursorProvider::resolve_from(configured, os_home)
        .map(|provider| provider.root().to_path_buf())
        .map_err(Into::into)
}
fn resolve_kimi_root(
    configured: Option<&Path>,
    kimi_code_home: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    KimiProvider::resolve_from(configured, kimi_code_home, os_home)
        .map(|provider| provider.root().to_path_buf())
        .map_err(Into::into)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Agentlog supports only macOS and Linux");

fn resolve_agentlog_home(
    home_override: Option<PathBuf>,
    agentlog_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    os_home: Option<OsString>,
) -> Result<PathBuf, PathError> {
    if let Some(home) = home_override.or_else(|| agentlog_home.map(PathBuf::from)) {
        return if home.as_os_str().is_empty() {
            Err(PathError::EmptyHome)
        } else {
            Ok(home)
        };
    }

    if let Some(data_home) = xdg_data_home.map(PathBuf::from)
        && !data_home.as_os_str().is_empty()
        && data_home.is_absolute()
    {
        return Ok(data_home.join("agentlog"));
    }

    let os_home = os_home
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or(PathError::HomeUnavailable)?;
    Ok(os_home.join(DEFAULT_DATA_HOME))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    providers: ProviderRoots,
}

impl Config {
    fn load(path: &Path) -> Result<ProviderRoots, PathError> {
        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str::<Self>(&contents)
                .map_err(|source| PathError::InvalidConfig {
                    path: path.to_path_buf(),
                    source,
                })?
                .providers
                .validate(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProviderRoots::default())
            }
            Err(source) => Err(PathError::ReadConfig {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRoots {
    #[serde(default, rename = "codex_root")]
    codex: Option<PathBuf>,
    #[serde(default, rename = "claude_root")]
    claude: Option<PathBuf>,
    #[serde(default, rename = "opencode_root")]
    opencode: Option<PathBuf>,
    #[serde(default, rename = "gemini_root")]
    gemini: Option<PathBuf>,
    #[serde(default, rename = "cursor_root")]
    cursor: Option<PathBuf>,
    #[serde(default, rename = "kimi_root")]
    kimi: Option<PathBuf>,
}

impl ProviderRoots {
    fn validate(self) -> Result<Self, PathError> {
        for (provider, root) in [
            ("codex", &self.codex),
            ("claude", &self.claude),
            ("opencode", &self.opencode),
            ("gemini", &self.gemini),
            ("cursor", &self.cursor),
            ("kimi", &self.kimi),
        ] {
            if let Some(root) = root
                && !root.is_absolute()
            {
                return Err(PathError::RelativeProviderRoot {
                    provider,
                    path: root.clone(),
                });
            }
        }

        Ok(self)
    }

    #[must_use]
    pub fn codex_root(&self) -> Option<&Path> {
        self.codex.as_deref()
    }

    #[must_use]
    pub fn claude_root(&self) -> Option<&Path> {
        self.claude.as_deref()
    }

    #[must_use]
    pub fn opencode_root(&self) -> Option<&Path> {
        self.opencode.as_deref()
    }

    #[must_use]
    pub fn gemini_root(&self) -> Option<&Path> {
        self.gemini.as_deref()
    }

    #[must_use]
    pub fn cursor_root(&self) -> Option<&Path> {
        self.cursor.as_deref()
    }

    #[must_use]
    pub fn kimi_root(&self) -> Option<&Path> {
        self.kimi.as_deref()
    }
}

/// Errors while resolving Agentlog-owned local state.
#[derive(Debug, Error)]
pub enum PathError {
    #[error("Agentlog home cannot be empty")]
    EmptyHome,
    #[error("cannot resolve the default Agentlog home because HOME is unavailable")]
    HomeUnavailable,
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
    #[error("cannot read Agentlog config at {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Agentlog config at {path} is invalid: {source}")]
    InvalidConfig {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("providers.{provider}_root must be an absolute path, got {path}")]
    RelativeProviderRoot {
        provider: &'static str,
        path: PathBuf,
    },
    #[error("cannot create Agentlog data directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<ProviderRootError> for PathError {
    fn from(error: ProviderRootError) -> Self {
        match error {
            ProviderRootError::CodexHomeUnavailable => Self::CodexHomeUnavailable,
            ProviderRootError::InvalidCodexRoot { path } => Self::InvalidCodexRoot { path },
            ProviderRootError::ClaudeHomeUnavailable => Self::ClaudeHomeUnavailable,
            ProviderRootError::InvalidClaudeRoot { path } => Self::InvalidClaudeRoot { path },
            ProviderRootError::OpenCodeHomeUnavailable => Self::OpenCodeHomeUnavailable,
            ProviderRootError::InvalidOpenCodeRoot { path } => Self::InvalidOpenCodeRoot { path },
            ProviderRootError::GeminiHomeUnavailable => Self::GeminiHomeUnavailable,
            ProviderRootError::InvalidGeminiRoot { path } => Self::InvalidGeminiRoot { path },
            ProviderRootError::CursorHomeUnavailable => Self::CursorHomeUnavailable,
            ProviderRootError::InvalidCursorRoot { path } => Self::InvalidCursorRoot { path },
            ProviderRootError::KimiHomeUnavailable => Self::KimiHomeUnavailable,
            ProviderRootError::InvalidKimiRoot { path } => Self::InvalidKimiRoot { path },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{
        AppPaths, resolve_agentlog_home, resolve_claude_root, resolve_codex_root,
        resolve_cursor_root, resolve_gemini_root, resolve_kimi_root, resolve_opencode_roots,
    };

    #[test]
    fn agentlog_home_precedence_and_xdg_defaults_are_table_driven() {
        struct Case {
            name: &'static str,
            explicit: Option<&'static str>,
            environment: Option<&'static str>,
            xdg: Option<&'static str>,
            home: Option<&'static str>,
            expected: Result<&'static str, &'static str>,
        }

        let cases = [
            Case {
                name: "explicit CLI home wins",
                explicit: Some("/cli/home"),
                environment: Some("/environment/home"),
                xdg: Some("/xdg/data"),
                home: Some("/Users/example"),
                expected: Ok("/cli/home"),
            },
            Case {
                name: "environment home wins over defaults",
                explicit: None,
                environment: Some("/environment/home"),
                xdg: Some("/xdg/data"),
                home: Some("/Users/example"),
                expected: Ok("/environment/home"),
            },
            Case {
                name: "macOS uses absolute XDG data home",
                explicit: None,
                environment: None,
                xdg: Some("/Users/example/.xdg/data"),
                home: Some("/Users/example"),
                expected: Ok("/Users/example/.xdg/data/agentlog"),
            },
            Case {
                name: "Linux uses absolute XDG data home",
                explicit: None,
                environment: None,
                xdg: Some("/xdg/data"),
                home: Some("/home/example"),
                expected: Ok("/xdg/data/agentlog"),
            },
            Case {
                name: "empty XDG data home uses the XDG HOME fallback",
                explicit: None,
                environment: None,
                xdg: Some(""),
                home: Some("/home/example"),
                expected: Ok("/home/example/.local/share/agentlog"),
            },
            Case {
                name: "relative XDG data home uses the XDG HOME fallback",
                explicit: None,
                environment: None,
                xdg: Some("relative/data"),
                home: Some("/home/example"),
                expected: Ok("/home/example/.local/share/agentlog"),
            },
            Case {
                name: "missing operating-system home fails",
                explicit: None,
                environment: None,
                xdg: None,
                home: None,
                expected: Err("HOME is unavailable"),
            },
        ];

        for case in cases {
            let result = resolve_agentlog_home(
                case.explicit.map(PathBuf::from),
                case.environment.map(OsString::from),
                case.xdg.map(OsString::from),
                case.home.map(OsString::from),
            );
            match case.expected {
                Ok(expected) => assert_eq!(
                    result.expect(case.name),
                    Path::new(expected),
                    "{}",
                    case.name
                ),
                Err(expected) => assert!(
                    result.expect_err(case.name).to_string().contains(expected),
                    "{}",
                    case.name
                ),
            }
        }
    }

    #[test]
    fn explicit_home_keeps_database_inside_agentlog_owned_directory() {
        let temporary = TempDir::new().expect("temporary directory");
        let paths =
            AppPaths::resolve(Some(temporary.path().join("agentlog"))).expect("paths resolve");

        assert_eq!(paths.home(), paths.data_dir);
        assert_eq!(paths.database_path, paths.data_dir.join("agentlog.sqlite3"));
        assert_eq!(paths.config_path, paths.data_dir.join("config.toml"));
    }

    #[test]
    fn config_accepts_explicit_provider_roots_without_changing_owned_database_path() {
        let temporary = TempDir::new().expect("temporary directory");
        let home = temporary.path().join("agentlog");
        fs::create_dir(&home).expect("create home");
        fs::write(
            home.join("config.toml"),
            "[providers]\ncodex_root = '/private/tmp/codex'\n",
        )
        .expect("write config");

        let paths = AppPaths::resolve(Some(home.clone())).expect("paths resolve");

        assert_eq!(paths.database_path, home.join("agentlog.sqlite3"));
        assert_eq!(
            paths.provider_roots().codex_root(),
            Some(Path::new("/private/tmp/codex"))
        );
    }

    #[test]
    fn config_rejects_unrelated_configuration() {
        let temporary = TempDir::new().expect("temporary directory");
        let home = temporary.path().join("agentlog");
        fs::create_dir(&home).expect("create home");
        fs::write(home.join("config.toml"), "[enrichment]\nenabled = true\n")
            .expect("write config");

        let error = AppPaths::resolve(Some(home)).expect_err("unrelated config must fail");

        assert!(error.to_string().contains("invalid"));
    }

    #[test]
    fn config_rejects_relative_provider_roots() {
        let temporary = TempDir::new().expect("temporary directory");
        let home = temporary.path().join("agentlog");
        fs::create_dir(&home).expect("create home");
        fs::write(
            home.join("config.toml"),
            "[providers]\nclaude_root = 'relative/path'\n",
        )
        .expect("write config");

        let error = AppPaths::resolve(Some(home)).expect_err("relative root must fail");

        assert!(error.to_string().contains("providers.claude_root"));
    }

    #[test]
    fn configured_codex_root_is_independent_from_agentlog_home() {
        let temporary = TempDir::new().expect("temporary directory");
        let home = temporary.path().join("agentlog");
        fs::create_dir(&home).expect("create home");
        fs::write(
            home.join("config.toml"),
            "[providers]\ncodex_root = '/private/tmp/codex-history'\n",
        )
        .expect("write config");

        let paths = AppPaths::resolve(Some(home)).expect("paths resolve");

        assert_eq!(
            paths.codex_root().expect("configured root"),
            Path::new("/private/tmp/codex-history")
        );
    }

    #[test]
    fn codex_root_precedence_and_validation_do_not_mutate_process_environment() {
        assert_eq!(
            resolve_codex_root(
                Some(Path::new("/configured/codex")),
                Some(OsString::from("/environment/codex")),
                Some(OsString::from("/Users/example")),
            )
            .expect("configured root"),
            Path::new("/configured/codex")
        );
        assert_eq!(
            resolve_codex_root(
                None,
                Some(OsString::from("/environment/codex")),
                Some(OsString::from("/Users/example")),
            )
            .expect("environment root"),
            Path::new("/environment/codex")
        );
        assert_eq!(
            resolve_codex_root(None, None, Some(OsString::from("/Users/example")))
                .expect("default root"),
            Path::new("/Users/example/.codex")
        );
        assert!(resolve_codex_root(None, Some(OsString::new()), None).is_err());
        assert!(resolve_codex_root(None, Some(OsString::from("relative/codex")), None).is_err());
    }

    #[test]
    fn claude_root_precedence_matches_claude_config_dir_convention() {
        assert_eq!(
            resolve_claude_root(
                Some(Path::new("/configured/claude")),
                Some(OsString::from("/environment/claude")),
                Some(OsString::from("/Users/example")),
            )
            .expect("configured root"),
            Path::new("/configured/claude")
        );
        assert_eq!(
            resolve_claude_root(
                None,
                Some(OsString::from("/environment/claude")),
                Some(OsString::from("/Users/example")),
            )
            .expect("environment root"),
            Path::new("/environment/claude")
        );
        assert_eq!(
            resolve_claude_root(None, None, Some(OsString::from("/Users/example")))
                .expect("default root"),
            Path::new("/Users/example/.claude")
        );
        assert!(resolve_claude_root(None, Some(OsString::new()), None).is_err());
        assert!(resolve_claude_root(None, Some(OsString::from("relative/claude")), None).is_err());
    }

    #[test]
    fn opencode_root_precedence_matches_xdg_data_convention() {
        assert_eq!(
            resolve_opencode_roots(
                Some(Path::new("/configured/opencode")),
                Some(OsString::from("/xdg/data")),
                Some(OsString::from("/Users/example")),
            )
            .expect("configured root"),
            vec![PathBuf::from("/configured/opencode")]
        );
        assert_eq!(
            resolve_opencode_roots(
                None,
                Some(OsString::from("/xdg/data")),
                Some(OsString::from("/Users/example")),
            )
            .expect("XDG root"),
            vec![PathBuf::from("/xdg/data/opencode")]
        );
        assert_eq!(
            resolve_opencode_roots(None, None, Some(OsString::from("/Users/example")))
                .expect("default root"),
            vec![if cfg!(target_os = "macos") {
                PathBuf::from("/Users/example/Library/Application Support/opencode")
            } else {
                PathBuf::from("/Users/example/.local/share/opencode")
            }]
        );
        assert!(resolve_opencode_roots(None, Some(OsString::new()), None).is_err());
        assert!(
            resolve_opencode_roots(None, Some(OsString::from("relative/opencode")), None).is_err()
        );
    }

    #[test]
    fn gemini_root_precedence_and_relative_values_are_rejected() {
        assert_eq!(
            resolve_gemini_root(
                Some(Path::new("/configured/gemini")),
                Some(OsString::from("/environment/gemini")),
                Some(OsString::from("/Users/example")),
            )
            .expect("configured root"),
            Path::new("/configured/gemini")
        );
        assert_eq!(
            resolve_gemini_root(
                None,
                Some(OsString::from("/environment/gemini")),
                Some(OsString::from("/Users/example")),
            )
            .expect("environment root"),
            Path::new("/environment/gemini/.gemini")
        );
        assert_eq!(
            resolve_gemini_root(None, None, Some(OsString::from("/Users/example")))
                .expect("default root"),
            Path::new("/Users/example/.gemini")
        );
        assert!(resolve_gemini_root(None, Some(OsString::from("relative/gemini")), None).is_err());
    }

    #[test]
    fn cursor_root_precedence_and_relative_values_are_rejected() {
        assert_eq!(
            resolve_cursor_root(
                Some(Path::new("/configured/cursor")),
                Some(OsString::from("/Users/example")),
            )
            .expect("configured root"),
            Path::new("/configured/cursor")
        );
        assert_eq!(
            resolve_cursor_root(None, Some(OsString::from("/Users/example")))
                .expect("default root"),
            Path::new("/Users/example/.cursor")
        );
        assert!(resolve_cursor_root(Some(Path::new("relative/cursor")), None).is_err());
    }

    #[test]
    fn kimi_root_precedence_and_relative_values_are_rejected() {
        assert_eq!(
            resolve_kimi_root(
                Some(Path::new("/configured/kimi")),
                Some(OsString::from("/environment/kimi")),
                Some(OsString::from("/Users/example"))
            )
            .expect("configured"),
            Path::new("/configured/kimi")
        );
        assert_eq!(
            resolve_kimi_root(
                None,
                Some(OsString::from("/environment/kimi")),
                Some(OsString::from("/Users/example"))
            )
            .expect("environment"),
            Path::new("/environment/kimi")
        );
        assert_eq!(
            resolve_kimi_root(None, None, Some(OsString::from("/Users/example"))).expect("default"),
            Path::new("/Users/example/.kimi-code")
        );
        assert!(resolve_kimi_root(None, Some(OsString::from("relative/kimi")), None).is_err());
    }
}
