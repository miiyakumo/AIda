use alda_agent::agent::{AgentEvent, AgentReporter};
use alda_agent::alda::AldaRunner;
use alda_agent::application::{ActionResult, Application};
use alda_agent::command::{ProjectAction, UserAction};
use alda_agent::config::ModelConfig;
use alda_agent::instructions::ProjectPreferences;
use alda_agent::project::{Project, WorkingScoreKind};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::thread;

fn tool_response(kind: &str, message: &str, code: Option<&str>) -> String {
    let mut arguments = serde_json::json!({ "kind": kind, "message": message });
    if kind == "plan" {
        arguments["plan"] = serde_json::json!({
            "core_material": "机械脉冲与明亮上行动机",
            "form": "引子—呈示—发展—结尾",
            "orchestration": "钢琴与弦乐",
            "development": "通过节奏加密和音区上移逐步推进"
        });
    }
    if let Some(code) = code {
        arguments["alda_code"] = serde_json::Value::String(code.to_string());
    }
    let arguments = arguments.to_string();
    let chunk = serde_json::json!({
        "choices": [{
            "delta": {
                "content": "机械运动、冰冷沉眠与最终加速升华；钢琴表现脉冲，弦乐表现明亮上升。",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": "submit_result", "arguments": arguments }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n")
}

fn response_server(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len() < header_end + length {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    format!("http://{address}")
}

fn fake_alda() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("alda");
    let json = r#"{"events":[{"offset":0,"duration":180000,"audible-duration":180000,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in parse) printf '%s\\n' '{json}' ;; export) while [ \"$#\" -gt 0 ]; do if [ \"$1\" = -o ]; then shift; : > \"$1\"; exit 0; fi; shift; done; exit 1 ;; play|stop) exit 0 ;; *) exit 1 ;; esac\n"
    );
    fs::write(&executable, script).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    (directory, executable)
}

struct TestReporter;

impl AgentReporter for TestReporter {
    fn report(&mut self, _event: AgentEvent) {}
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn progressive_workflow_only_versions_an_accepted_candidate() {
    let draft = "piano: (tempo 120) c1 *4";
    let developed = "piano: (tempo 120) c1 d1 e1 *120";
    let base_url = response_server(vec![
        tool_response("plan", "先建立机械脉冲，再发展中段和明亮结尾。", None),
        tool_response("draft", "核心机械脉冲草稿。", Some(draft)),
        tool_response("candidate", "已发展成完整候选。", Some(developed)),
    ]);
    let (_alda_directory, alda_path) = fake_alda();
    let project_directory = tempfile::tempdir().unwrap();
    let root = project_directory.path().join("mechanical-drive-poem");
    let source = include_str!("fixtures/mechanical-drive-poem.txt");
    let mut project =
        Project::load_or_create(root.clone(), "mechanical-drive-poem", source).unwrap();
    project
        .configure(&ProjectPreferences {
            target_duration_secs: Some(alda_agent::instructions::DurationConstraint::exact(180.0)),
            ..ProjectPreferences::default()
        })
        .unwrap();
    let mut model = ModelConfig::default();
    model.set_model("example-model").unwrap();
    model.set_base_url(&base_url).unwrap();
    model.set_api_key("secret-test-value").unwrap();
    model.save(&root).unwrap();
    let mut application =
        Application::from_project(project, Some(AldaRunner::new(alda_path.clone())));
    let mut reporter = TestReporter;

    let plan = application
        .execute(
            UserAction::Agent("创作约三分钟的完整纯器乐曲".to_string()),
            &mut reporter,
        )
        .await
        .unwrap();
    assert!(matches!(
        plan,
        ActionResult::AgentCompleted { success: false, .. }
    ));

    let draft_result = application
        .execute(
            UserAction::Agent("先做核心材料草稿".to_string()),
            &mut reporter,
        )
        .await
        .unwrap();
    assert!(matches!(
        draft_result,
        ActionResult::AgentCompleted { success: true, .. }
    ));
    assert_eq!(application.project_view().current_version, None);
    assert_eq!(
        application.project_view().working_score.as_deref(),
        Some("草稿")
    );

    let feedback = "发展中段并完成明亮结尾";
    let candidate = application
        .execute(UserAction::Agent(feedback.to_string()), &mut reporter)
        .await
        .unwrap();
    assert!(matches!(
        candidate,
        ActionResult::AgentCompleted { success: true, .. }
    ));
    assert_eq!(application.project_view().current_version, None);
    assert_eq!(
        application.project_view().working_score.as_deref(),
        Some("完整候选")
    );
    drop(application);

    let project = Project::load_or_create(root.clone(), "ignored", "ignored").unwrap();
    assert_eq!(
        project.working_score().unwrap().kind,
        WorkingScoreKind::Candidate
    );
    let mut application = Application::from_project(project, Some(AldaRunner::new(alda_path)));
    let accepted = application
        .execute(UserAction::Project(ProjectAction::Accept), &mut reporter)
        .await
        .unwrap();
    assert!(matches!(accepted, ActionResult::Message(message) if message.contains("v1")));
    assert_eq!(application.project_view().current_version, Some(1));
    assert!(application.project_view().working_score.is_none());
    drop(application);

    let restarted = Project::load_or_create(root, "ignored", "ignored").unwrap();
    assert_eq!(restarted.current_version(), 1);
    assert_eq!(restarted.versions().len(), 1);
    assert!(!restarted.conversation().messages().is_empty());
    let metadata = fs::read_to_string(restarted.root().join("project.json")).unwrap();
    assert!(!metadata.contains("secret-test-value"));
}
