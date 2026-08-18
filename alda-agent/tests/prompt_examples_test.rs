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
fn prompt_invalid_alda_examples_are_rejected_by_alda_2_3_3() {
    let blocks = fenced_blocks("alda-invalid");
    assert!(
        !blocks.is_empty(),
        "protocol.md 至少需要一个 ```alda-invalid 示例"
    );
    for (index, block) in blocks.iter().enumerate() {
        let output = parse_alda(block);
        assert!(
            !output.status.success(),
            "标记为 alda-invalid 的示例 #{index} 实际可以解析\ncode:\n{block}"
        );
    }
}

#[test]
fn prompt_repeat_spacing_and_alias_continuation_are_documented() {
    assert!(
        PROTOCOL.contains("`*` 与次数之间有没有空格都可以"),
        "protocol.md 必须说明 * 与次数之间有没有空格都可以"
    );
    assert!(
        !PROTOCOL.contains("不能有空格"),
        "protocol.md 不得再声称 * 与次数之间不能有空格"
    );
    assert!(
        PROTOCOL.contains("后续续写该实例时"),
        "protocol.md 必须说明别名声明后续写方式"
    );
    assert!(
        PROTOCOL.contains("violin-1:"),
        "protocol.md 必须给出别名续写示例 violin-1:"
    );
    assert!(
        !PROTOCOL.contains("`violin`、`cello`"),
        "protocol.md 不得再声称 violin、cello 等别名都会导致语法错误"
    );
}

#[test]
fn workflow_respects_explicit_full_composition_requests() {
    let workflow = include_str!("../skills/progressive-composition/SKILL.md");
    assert!(workflow.contains("编写曲目"));
    assert!(workflow.contains("“编曲”“作曲”“写曲”“写一首”“开始创作”"));
    assert!(workflow.contains("直接生成完整候选"));
    assert!(workflow.contains("没有额外约束"));
    assert!(workflow.contains("未经宿主确认"));
    assert!(PROTOCOL.contains("core_material"));
    assert!(PROTOCOL.contains("需要用户回复时必须使用 `clarification`"));
    assert!(PROTOCOL.contains("与音乐任务无关的品牌推广"));
    assert!(PROTOCOL.contains("不得形成连续澄清循环"));
    assert!(PROTOCOL.contains("具体商业品牌名称"));
    assert!(PROTOCOL.contains("取消该目标"));
    assert!(PROTOCOL.contains("若宿主报告参数截断"));
    assert!(PROTOCOL.contains("不得原样重发"));
    assert!(PROTOCOL.contains("其他参数错误按宿主指出的字段修正"));
    assert!(PROTOCOL.contains("不得声称未经宿主校验的精确时长"));
}

#[test]
fn delegation_prompt_leaves_task_selection_to_composer() {
    let workflow = include_str!("../skills/progressive-composition/SKILL.md");
    assert!(PROTOCOL.contains("可以调用 `delegate`"));
    assert!(PROTOCOL.contains("是否委派、委派什么和调用几次由你"));
    assert!(workflow.contains("不要预设固定 Worker、段落分组或委派流程"));
    assert!(!PROTOCOL.contains("Intro/A/A2/Coda"));
    assert!(!workflow.contains("Intro/A/A2/Coda"));
}
