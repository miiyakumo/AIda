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
    cursor_query_limit: Option<usize>,
}

impl PtyProcess {
    fn spawn(project: &std::path::Path) -> Self {
        Self::spawn_with_size(project, 30, 100)
    }

    fn spawn_with_size(project: &std::path::Path, rows: u16, cols: u16) -> Self {
        Self::spawn_with_cursor_query_limit(project, rows, cols, None)
    }

    fn spawn_with_cursor_query_limit(
        project: &std::path::Path,
        rows: u16,
        cols: u16,
        cursor_query_limit: Option<usize>,
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
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
            cursor_query_limit,
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
                    while self.answered_cursor_queries < queries
                        && self
                            .cursor_query_limit
                            .is_none_or(|limit| self.answered_cursor_queries < limit)
                    {
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

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.transcript).into_owned()
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

#[test]
fn terminal_prompt_separates_context_and_keeps_only_submitted_input_in_history() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("layout-project");
    let mut process = PtyProcess::spawn(&project);
    process.wait_for("项目 · layout-project · 尚无版本 · 完整曲目".as_bytes());
    process.wait_for("状态 · 仅本地 · 模型配置不可用".as_bytes());
    process.wait_for("› ".as_bytes());

    process.send(b"/project\r");
    process.wait_for("项目：layout-project".as_bytes());
    process.send(b"\x04");
    process.pump(Duration::from_millis(200));

    let transcript = process.text();
    assert!(transcript.contains("项目 · layout-project · 尚无版本 · 完整曲目"));
    assert!(transcript.contains("状态 · 仅本地 · 模型配置不可用"));
    assert!(transcript.contains("项目：layout-project"));
    assert!(transcript.matches("项目 · layout-project").count() >= 2);
}

#[test]
fn narrow_terminal_keeps_project_status_and_input_markers() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("narrow-project");
    let mut process = PtyProcess::spawn_with_size(&project, 30, 32);
    process.wait_for("项目 · narrow-project".as_bytes());
    process.wait_for("状态 · 仅本地".as_bytes());
    process.wait_for("› ".as_bytes());
}

#[test]
fn cursor_query_timeout_falls_back_without_ending_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("cursor-timeout-project");
    let mut process = PtyProcess::spawn_with_cursor_query_limit(&project, 30, 100, Some(1));
    process.wait_for("› ".as_bytes());

    process.send(b"/project\r");
    process.wait_for("项目：cursor-timeout-project".as_bytes());
    process.wait_for("已切换到基础输入模式".as_bytes());
    process.send(b"/help\n");
    process.wait_for("自然语言输入".as_bytes());
    process.send(b"/quit\n");
}
