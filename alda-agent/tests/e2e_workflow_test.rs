use alda_agent::agent::from_provider_messages;
use alda_agent::agent::{Agent, CreationMode, CreationRequest, ModifyRequest};
use alda_agent::alda::AldaRunner;
use alda_agent::conversation::ConversationState;
use alda_agent::deepseek::DeepSeekClient;
use alda_agent::project::{Project, WorkingScoreKind};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::thread;

fn tool_response(kind: &str, message: &str, code: Option<&str>) -> String {
    let mut arguments = serde_json::json!({ "kind": kind, "message": message });
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
    let client = DeepSeekClient::new(
        "secret-test-value".to_string(),
        base_url,
        "example-model".to_string(),
    )
    .unwrap();
    let (_alda_directory, alda_path) = fake_alda();
    let agent = Agent::new(client, AldaRunner::new(alda_path.clone()));
    let project_directory = tempfile::tempdir().unwrap();
    let root = project_directory.path().join("mechanical-drive-poem");
    let source = include_str!("fixtures/mechanical-drive-poem.txt");
    let mut project =
        Project::load_or_create(root.clone(), "mechanical-drive-poem", source).unwrap();
    project
        .configure("full", Some(180.0), Vec::new(), Vec::new())
        .unwrap();

    let plan = agent
        .create(CreationRequest {
            source_material: source.to_string(),
            instructions: "创作约三分钟的完整纯器乐曲".to_string(),
            creative_strategy: String::new(),
            mode: CreationMode::FullPiece,
            target_duration_secs: Some(180.0),
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            max_rounds: 3,
        })
        .await
        .unwrap();
    assert!(!plan.success);
    assert!(plan.interpretation.contains("机械脉冲"));
    project
        .replace_conversation(
            from_provider_messages(plan.conversation),
            ConversationState::Ready,
        )
        .unwrap();

    let draft_result = agent
        .create(CreationRequest {
            source_material: source.to_string(),
            instructions: "先做核心材料草稿".to_string(),
            creative_strategy: String::new(),
            mode: CreationMode::FullPiece,
            target_duration_secs: Some(180.0),
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            max_rounds: 3,
        })
        .await
        .unwrap();
    assert!(draft_result.success);
    project
        .save_working_score(
            draft_result.alda_code.as_deref().unwrap(),
            WorkingScoreKind::Draft,
            "核心材料",
            &draft_result.checks,
        )
        .unwrap();
    assert_eq!(project.current_version(), 0);
    assert_eq!(project.versions().len(), 0);

    let feedback = "发展中段并完成明亮结尾";
    let candidate = agent
        .modify(ModifyRequest {
            source_material: source.to_string(),
            current_alda: project.working_code().unwrap(),
            feedback: feedback.to_string(),
            creative_strategy: String::new(),
            mode: CreationMode::FullPiece,
            target_duration_secs: Some(180.0),
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            max_rounds: 3,
        })
        .await
        .unwrap();
    assert!(candidate.success);
    project
        .save_working_score(
            candidate.alda_code.as_deref().unwrap(),
            WorkingScoreKind::Candidate,
            feedback,
            &candidate.checks,
        )
        .unwrap();
    assert_eq!(project.current_version(), 0);
    drop(project);

    let mut project = Project::load_or_create(root.clone(), "ignored", "ignored").unwrap();
    assert_eq!(
        project.working_score().unwrap().kind,
        WorkingScoreKind::Candidate
    );
    assert_eq!(project.accept_working_score().unwrap(), 1);
    assert_eq!(project.current_version(), 1);
    assert!(project.working_score().is_none());
    let alda_export = project.export_alda().unwrap();
    let midi_export = project.midi_export_path().unwrap();
    AldaRunner::new(alda_path)
        .export_midi(&project.current_version_path().unwrap(), &midi_export)
        .unwrap();
    assert!(alda_export.is_file());
    assert!(midi_export.is_file());
    drop(project);

    let restarted = Project::load_or_create(root, "ignored", "ignored").unwrap();
    assert_eq!(restarted.current_version(), 1);
    assert_eq!(restarted.versions().len(), 1);
    assert!(!restarted.conversation().messages().is_empty());
    let metadata = fs::read_to_string(restarted.root().join("project.json")).unwrap();
    assert!(!metadata.contains("secret-test-value"));
}
