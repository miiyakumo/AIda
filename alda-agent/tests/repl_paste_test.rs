use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct PtyProcess {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    transcript: Vec<u8>,
    answered_cursor_queries: usize,
}

impl PtyProcess {
    fn spawn(project: &std::path::Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_alda-agent"));
        command.args(["--project", project.to_str().unwrap()]);
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            writer,
            output,
            transcript: Vec::new(),
            answered_cursor_queries: 0,
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }

    fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .output
                .recv_timeout(remaining.min(Duration::from_millis(20)))
            {
                Ok(chunk) => {
                    self.transcript.extend_from_slice(&chunk);
                    let queries = self
                        .transcript
                        .windows(4)
                        .filter(|window| *window == b"\x1b[6n")
                        .count();
                    while self.answered_cursor_queries < queries {
                        self.send(b"\x1b[1;1R");
                        self.answered_cursor_queries += 1;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self
            .transcript
            .windows(needle.len())
            .any(|window| window == needle)
        {
            assert!(
                Instant::now() < deadline,
                "PTY output did not contain {needle:?}"
            );
            self.pump(Duration::from_millis(50));
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn bracketed_multiline_paste_waits_for_enter_and_submits_once() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("paste-project");
    let mut process = PtyProcess::spawn(&project);
    process.wait_for(b"\x1b[?2004h");

    process.send("\x1b[200~第一行\n\n第二行\x1b[201~".as_bytes());
    process.pump(Duration::from_millis(200));
    let before_submit: Value =
        serde_json::from_slice(&std::fs::read(project.join("project.json")).unwrap()).unwrap();
    assert_eq!(
        before_submit["conversation"]["messages"],
        serde_json::json!([])
    );

    process.send(b"\r");
    let deadline = Instant::now() + Duration::from_secs(3);
    let saved = loop {
        process.pump(Duration::from_millis(50));
        let project_json: Value =
            serde_json::from_slice(&std::fs::read(project.join("project.json")).unwrap()).unwrap();
        if project_json["conversation"]["state"] == "request_pending" {
            break project_json;
        }
        assert!(
            Instant::now() < deadline,
            "multiline request was not persisted"
        );
    };
    assert_eq!(
        saved["conversation"]["messages"],
        serde_json::json!([{"role": "user", "content": "第一行\n\n第二行"}])
    );
}
