use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;

fn temporary_directory() -> TempDir {
    let platform_temp = std::env::temp_dir()
        .canonicalize()
        .expect("resolve platform temporary directory");
    TempDir::new_in(platform_temp).expect("temporary directory")
}

fn agentlog(home: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentlog"))
        .arg("--home")
        .arg(home)
        .args(arguments)
        .output()
        .expect("run agentlog process")
}

fn write_provider_fixture(temporary: &TempDir, home: &Path) {
    let gemini_root = temporary.path().join("gemini");
    let source = gemini_root.join("tmp/session.jsonl");
    fs::create_dir_all(source.parent().expect("Gemini source parent"))
        .expect("create Gemini fixture directory");
    fs::write(
        source,
        "{\"sessionId\":\"purge-process-session\"}\n{\"type\":\"user\",\"content\":\"retained request\"}\n",
    )
    .expect("write Gemini fixture");
    fs::write(
        home.join("config.toml"),
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
    .expect("write isolated provider config");
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn sqlite_file_set_size(database: &Path) -> u64 {
    [database.to_path_buf()]
        .into_iter()
        .chain(
            ["-wal", "-shm"]
                .into_iter()
                .map(|suffix| sqlite_sidecar_path(database, suffix)),
        )
        .map(|path| match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("inspect SQLite file set: {error}"),
        })
        .sum()
}

fn json_session_count(output: &std::process::Output) -> usize {
    assert!(output.status.success());
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .expect("parse list JSON")
        .as_array()
        .expect("list JSON is an array")
        .len()
}

#[tokio::test]
async fn purge_process_previews_and_removes_a_nonempty_catalog() {
    let temporary = temporary_directory();
    let home = temporary.path().join("agentlog");
    fs::create_dir(&home).expect("create Agentlog home");
    write_provider_fixture(&temporary, &home);

    let sync = agentlog(&home, &["sync"]);
    assert!(sync.status.success());
    let list_before = agentlog(&home, &["list", "--json"]);
    assert_eq!(json_session_count(&list_before), 1);

    let database = home.join("agentlog.sqlite3");
    let expected_bytes = sqlite_file_set_size(&database);
    assert!(expected_bytes > 0);

    let preview = agentlog(&home, &["purge"]);
    assert!(preview.status.success());
    assert!(
        preview.stdout.is_empty(),
        "purge does not have a product output"
    );
    let preview_report = String::from_utf8(preview.stderr).expect("UTF-8 preview report");
    assert!(preview_report.contains(&format!(
        "Agentlog-owned database target: {}",
        database.display()
    )));
    assert!(preview_report.contains("Catalog sources: 1"));
    assert!(preview_report.contains("Catalog sessions: 1"));
    assert!(preview_report.contains("Transcript items: 1"));
    let size_line = preview_report
        .lines()
        .find(|line| line.starts_with("Approximate database size: "))
        .expect("purge size report");
    assert!(
        [" B (", " KiB (", " MiB (", " GiB ("]
            .iter()
            .any(|unit| size_line.contains(unit)),
        "size report uses a human-readable IEC unit"
    );
    assert!(size_line.ends_with(&format!("({expected_bytes} bytes)")));
    assert!(preview_report.contains("No changes were made."));
    assert!(preview_report.contains("agentlog purge --yes"));
    assert!(preview_report.contains(
        "Catalog data may not be reconstructible: only provider logs that still exist and remain readable can be synchronized."
    ));

    let list_after_preview = agentlog(&home, &["list", "--json"]);
    assert_eq!(json_session_count(&list_after_preview), 1);
    assert_eq!(list_after_preview.stdout, list_before.stdout);

    let confirmed = agentlog(&home, &["purge", "--yes"]);
    assert!(confirmed.status.success());
    assert!(confirmed.stdout.is_empty());
    let completion_report = String::from_utf8(confirmed.stderr).expect("UTF-8 completion report");
    assert!(completion_report.contains("Purge complete: cleared 1 catalog sources"));
    assert!(completion_report.contains(
        "Catalog data may not be reconstructible: only provider logs that still exist and remain readable can be synchronized."
    ));

    let list_after_purge = agentlog(&home, &["list", "--json"]);
    assert_eq!(json_session_count(&list_after_purge), 0);
    let options = SqliteConnectOptions::new()
        .filename(&database)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open purged catalog read-only");
    let counts = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT
            (SELECT COUNT(*) FROM sources),
            (SELECT COUNT(*) FROM sessions),
            (SELECT COUNT(*) FROM transcript_items)",
    )
    .fetch_one(&pool)
    .await
    .expect("count purged catalog rows");
    assert_eq!(counts, (0, 0, 0));
    pool.close().await;

    assert!(!agentlog(&home, &["data", "clear"]).status.success());
    assert!(!agentlog(&home, &["purge", "--dry-run"]).status.success());
}

#[test]
fn sync_process_keeps_json_stdout_clean_and_bounds_noninteractive_progress() {
    let temporary = temporary_directory();
    let home = temporary.path().join("agentlog");
    fs::create_dir(&home).expect("create Agentlog home");
    write_provider_fixture(&temporary, &home);

    let output = agentlog(&home, &["sync", "--json"]);

    assert!(output.status.success());
    let summary = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .expect("sync JSON remains the complete stdout product");
    assert_eq!(summary["sources_refreshed"], 1);
    let report = String::from_utf8(output.stderr).expect("UTF-8 sync report");
    assert!(report.contains("[~] Provider gemini: starting 1 candidate sources"));
    assert!(report.contains("[~] Provider gemini: discovering sources"));
    assert!(report.contains("[+] Provider gemini: candidates=1, refreshed=1"));
    assert!(
        report.lines().count() <= 18,
        "noninteractive progress reports bounded provider discovery, candidate, and completion events"
    );
}
