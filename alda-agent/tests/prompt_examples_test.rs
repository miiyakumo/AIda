use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PROTOCOL: &str = include_str!("../prompts/protocol.md");
const ALDA_REFERENCE: &str = include_str!("../prompts/alda-reference.md");

fn fenced_blocks(language: &str) -> Vec<String> {
    let marker = format!("```{language}");
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut current = String::new();

    for line in format!("{PROTOCOL}\n{ALDA_REFERENCE}").lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == marker {
                in_block = true;
                current.clear();
            }
        } else if trimmed == "```" {
            in_block = false;
            blocks.push(current.trim_end().to_string());
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    assert!(!in_block, "protocol.md 存在未闭合的 ```{language} 代码块");
    blocks
}

fn alda_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("ALDA_BIN") {
        let path = PathBuf::from(path);
        assert!(
            path.exists(),
            "ALDA_BIN 指定的 Alda 不存在: {}",
            path.display()
        );
        return path;
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join("alda");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = Path::new(&home).join(".local").join("bin").join("alda");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("未找到 Alda 可执行文件；请安装 Alda 2.3.3 或设置 ALDA_BIN");
}

fn parse_alda(code: &str) -> Output {
    let mut child = Command::new(alda_binary())
        .args(["parse", "-v", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("无法启动 alda parse");
    child
        .stdin
        .take()
        .expect("缺少 alda parse stdin")
        .write_all(code.as_bytes())
        .expect("无法写入 alda parse stdin");
    child.wait_with_output().expect("无法等待 alda parse")
}

#[test]
fn prompt_valid_alda_examples_parse_with_alda_2_3_3() {
    let blocks = fenced_blocks("alda");
    assert!(!blocks.is_empty(), "protocol.md 至少需要一个 ```alda 示例");
    for (index, block) in blocks.iter().enumerate() {
        assert!(
            !block.contains("time-signature"),
            "有效示例 #{index} 不得包含 time-signature"
        );
        let output = parse_alda(block);
        assert!(
            output.status.success(),
            "有效 Alda 示例 #{index} 解析失败\nstderr:\n{}\ncode:\n{block}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn core_protocol_routes_dsl_facts_to_on_demand_docs() {
    assert!(
        PROTOCOL.contains("调用 `lookup_alda_docs`"),
        "核心协议必须把不确定的 Alda 事实路由到按需文档查询"
    );
    assert!(
        !PROTOCOL.contains("```alda"),
        "核心协议不应重新内嵌 Alda 示例"
    );
    assert!(
        ALDA_REFERENCE.contains("后续用别名继续"),
        "精简参考应保留高频别名陷阱"
    );
    assert!(
        ALDA_REFERENCE.contains("详细规则以官方快照对应章节为准"),
        "精简参考不能冒充完整 DSL 规范"
    );
}

#[test]
fn workflow_respects_explicit_full_composition_requests() {
    let workflow = include_str!("../skills/progressive-composition/SKILL.md");
    assert!(workflow.contains("完整候选"));
    assert!(workflow.contains("明确要求先看方案"));
    assert!(workflow.contains("不要要求用户逐段试听或逐段确认"));
    assert!(PROTOCOL.contains("“编曲”“作曲”“写曲”“开始创作”"));
    assert!(PROTOCOL.contains("持续工作到提交 `candidate`"));
    assert!(PROTOCOL.contains("可采用合理默认值"));
    assert!(PROTOCOL.contains("core_material"));
    assert!(PROTOCOL.contains("才用 `clarification`"));
    assert!(PROTOCOL.contains("与音乐任务无关的品牌推广"));
    assert!(PROTOCOL.contains("不得形成连续澄清循环"));
    assert!(PROTOCOL.contains("具体商业品牌名称"));
    assert!(PROTOCOL.contains("除非用户明确取消"));
    assert!(PROTOCOL.contains("若宿主报告参数截断"));
    assert!(PROTOCOL.contains("不得原样重发"));
    assert!(PROTOCOL.contains("其他参数错误按宿主指出的字段修正"));
    assert!(PROTOCOL.contains("不得提前声称未经宿主确认的精确时长"));
}

#[test]
fn delegation_prompt_defines_composer_and_host_escalation_boundaries() {
    let workflow = include_str!("../skills/progressive-composition/SKILL.md");
    assert!(PROTOCOL.contains("`delegate`："));
    assert!(PROTOCOL.contains("连续失败触发诊断升级"));
    assert!(PROTOCOL.contains("进入 `diagnose_only` 后只能委派独立 Reviewer"));
    assert!(workflow.contains("可按需调用 `delegate`"));
    assert!(workflow.contains("预设固定 Worker 或段落分组"));
    assert!(workflow.contains("宿主触发诊断升级时必须遵循 Reviewer 边界"));
    assert!(!PROTOCOL.contains("Intro/A/A2/Coda"));
    assert!(!workflow.contains("Intro/A/A2/Coda"));
}
