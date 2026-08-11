#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::Command,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tempfile::TempDir;

const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const FIXTURE_TEXT: &[u8] = b"PTY preview fixture";
const BROWSE_MARKER: &[u8] = b"diagnostics";
const PREVIEW_MARKER: &[u8] = b"PgUp/PgDn";
const REFINE_MARKER: &[u8] = b"Refine:";
const HELP_MARKER: &[u8] = b"Help";
const DIAGNOSTICS_MARKER: &[u8] = b"PgUp";
const TERMIOS_RESTORED: &[u8] = b"__AGENTLOG_TERMIOS_RESTORED:1__";
const APP_SUCCEEDED: &[u8] = b"__AGENTLOG_STATUS:0__";

const BROWSE_WITH_TERMIOS_CHECK: &str = r#"
"$1" --home "$2" browse
status=$?
termios=$(stty -a | tr ';\n' '  ')
printf '__AGENTLOG_TERMIOS:%s__\n' "$termios"
case " $termios " in
  *" -icanon "*|*" -echo "*)
    printf '__AGENTLOG_TERMIOS_RESTORED:0__\n'
    exit 1
    ;;
  *" icanon "*)
    case " $termios " in
      *" echo "*) printf '__AGENTLOG_TERMIOS_RESTORED:1__\n' ;;
      *) printf '__AGENTLOG_TERMIOS_RESTORED:0__\n'; exit 1 ;;
    esac
    ;;
  *)
    printf '__AGENTLOG_TERMIOS_RESTORED:0__\n'
    exit 1
    ;;
esac
printf '__AGENTLOG_STATUS:%s__\n' "$status"
exit "$status"
"#;

#[derive(Clone, Copy)]
enum Surface {
    BrowseWide,
    BrowseNarrow,
    Preview,
    Refine,
    Help,
    Diagnostics,
}

impl Surface {
    fn name(self) -> &'static str {
        match self {
            Self::BrowseWide => "Browse at 100x24",
            Self::BrowseNarrow => "Browse at 80x24",
            Self::Preview => "Preview",
            Self::Refine => "Refine",
            Self::Help => "Help",
            Self::Diagnostics => "Diagnostics",
        }
    }

    fn size(self) -> PtySize {
        PtySize {
            rows: 24,
            cols: match self {
                Self::BrowseNarrow => 80,
                _ => 100,
            },
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

struct RunningBrowse {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    reader_result: mpsc::Receiver<(std::io::Result<()>, Vec<u8>)>,
}

impl RunningBrowse {
    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("write PTY input");
        self.writer.flush().expect("flush PTY input");
    }

    fn wait_for_output(&mut self, marker: &[u8], description: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self
                .output
                .lock()
                .expect("lock PTY output")
                .windows(marker.len())
                .any(|window| window == marker)
            {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll browse process") {
                panic!("browse process exited before {description}: {status:?}");
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("terminate stalled browse process");
                let _ = self.child.wait();
                let output = self.output.lock().expect("lock PTY output");
                panic!(
                    "browse process did not render {description}; PTY output: {:?}",
                    String::from_utf8_lossy(&output)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish_after_control_c(mut self, surface: Surface) {
        self.send(&[3]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll Ctrl-C exit") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("terminate stalled browse process");
                let _ = self.child.wait();
                panic!("{} did not exit after Ctrl-C", surface.name());
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "{} should exit normally after Ctrl-C: {status:?}",
            surface.name()
        );

        drop(self.writer);
        drop(self.master);
        let (reader_result, captured) = self
            .reader_result
            .recv_timeout(Duration::from_secs(5))
            .expect("PTY reader finishes after browse exits");
        reader_result.expect("read PTY output");
        for (marker, description) in [
            (ENTER_ALTERNATE_SCREEN, "enter the alternate screen"),
            (HIDE_CURSOR, "hide the cursor"),
            (LEAVE_ALTERNATE_SCREEN, "leave the alternate screen"),
            (SHOW_CURSOR, "show the cursor"),
            (TERMIOS_RESTORED, "restore canonical mode and echo"),
            (APP_SUCCEEDED, "report the successful Agentlog exit"),
        ] {
            assert!(
                captured
                    .windows(marker.len())
                    .any(|window| window == marker),
                "{} should {description}",
                surface.name()
            );
        }
    }
}

fn temporary_directory() -> TempDir {
    TempDir::new()
        // Some sandboxes deny the host's TMPDIR. Cargo's writable target
        // directory remains isolated from the provider fixtures used here.
        .or_else(|_| {
            let target = std::env::current_dir()?.join("target");
            fs::create_dir_all(&target)?;
            TempDir::new_in(target)
        })
        .expect("temporary directory")
}

fn write_provider_fixture(temporary: &TempDir, home: &Path) {
    let gemini_root = temporary.path().join("gemini");
    let source = gemini_root.join("tmp/session.jsonl");
    fs::create_dir_all(source.parent().expect("Gemini source parent"))
        .expect("create Gemini fixture directory");
    fs::write(
        source,
        "{\"sessionId\":\"pty-session\"}\n{\"type\":\"user\",\"content\":\"PTY preview fixture\"}\n",
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

fn seed_catalog(home: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_agentlog"))
        .arg("--home")
        .arg(home)
        .arg("sync")
        .output()
        .expect("synchronize PTY fixture");
    assert!(
        output.status.success(),
        "fixture synchronization failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn start_browse(home: &Path, size: PtySize) -> RunningBrowse {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(size).expect("open pseudoterminal");
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        BROWSE_WITH_TERMIOS_CHECK,
        "agentlog-pty-shell",
        env!("CARGO_BIN_EXE_agentlog"),
        home.to_str().expect("UTF-8 home"),
    ]);
    command.env("TERM", "xterm-256color");
    let child = pair
        .slave
        .spawn_command(command)
        .expect("start browse shell in pseudoterminal");
    drop(pair.slave);

    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = Arc::clone(&output);
    let (reader_done, reader_result) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    thread::spawn(move || {
        let mut captured = Vec::new();
        let result = loop {
            let mut chunk = [0_u8; 1024];
            match reader.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    captured.extend_from_slice(&chunk[..read]);
                    reader_output
                        .lock()
                        .expect("lock PTY output")
                        .clone_from(&captured);
                }
                Err(error) => break Err(error),
            }
        };
        reader_done
            .send((result, captured))
            .expect("send PTY reader result");
    });
    let writer = pair.master.take_writer().expect("take PTY writer");

    RunningBrowse {
        child,
        master: pair.master,
        writer,
        output,
        reader_result,
    }
}

fn verify_control_c_from(surface: Surface, home: &Path) {
    let mut browse = start_browse(home, surface.size());
    browse.wait_for_output(ENTER_ALTERNATE_SCREEN, "raw-mode alternate screen");
    browse.wait_for_output(BROWSE_MARKER, "Browse controls");
    browse.wait_for_output(FIXTURE_TEXT, "seeded Browse session");
    match surface {
        Surface::BrowseWide | Surface::BrowseNarrow => {}
        Surface::Preview => {
            browse.send(b"\r");
            browse.wait_for_output(PREVIEW_MARKER, "Preview surface");
        }
        Surface::Refine => {
            browse.send(b"f");
            browse.wait_for_output(REFINE_MARKER, "Refine surface");
        }
        Surface::Help => {
            browse.send(b"?");
            browse.wait_for_output(HELP_MARKER, "Help surface");
        }
        Surface::Diagnostics => {
            browse.send(b"!");
            browse.wait_for_output(DIAGNOSTICS_MARKER, "Diagnostics surface");
        }
    }
    browse.finish_after_control_c(surface);
}

#[test]
fn control_c_exits_every_accepted_surface_and_restores_terminal_mode() {
    let temporary = temporary_directory();
    let home = temporary.path().join("agentlog");
    fs::create_dir(&home).expect("create Agentlog home");
    write_provider_fixture(&temporary, &home);
    seed_catalog(&home);

    for surface in [
        Surface::BrowseWide,
        Surface::BrowseNarrow,
        Surface::Preview,
        Surface::Refine,
        Surface::Help,
        Surface::Diagnostics,
    ] {
        verify_control_c_from(surface, &home);
    }
}
