use crate::agent::{AgentEvent, AgentReporter};
use crate::alda::{AldaCheck, CancellationToken, CheckStatus};
use crate::application::{ActionResult, Application, ProjectView};
use crate::command::{
    ALDA_COMMANDS, CONFIG_COMMANDS, PROJECT_COMMANDS, SKILL_COMMANDS, TOP_LEVEL_COMMANDS,
    contains_inline_api_key, parse,
};
use crate::command::{ProjectAction, UserAction};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reedline::{
    ColumnarMenu, Completer, Emacs, FileBackedHistory, History, HistoryItem, HistoryItemId,
    HistorySessionId, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu,
    SearchQuery, Signal, Span, Suggestion, default_emacs_keybindings,
};
use std::borrow::Cow;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub async fn run_repl(project_dir: PathBuf, name: String) -> Result<()> {
    let mut application = Application::open(project_dir.clone(), &name)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_terminal(&mut application, project_dir).await
    } else {
        run_plain(
            &mut application,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        )
        .await
    }
}

async fn run_terminal(application: &mut Application, project_dir: PathBuf) -> Result<()> {
    let history_path = project_dir.join(".repl-history");
    sanitize_history_file(&history_path)?;
    let history = FileBackedHistory::with_file(500, history_path)
        .map(|history| SensitiveHistory { inner: history });
    let versions = Arc::new(RwLock::new(Vec::new()));
    let working_available = Arc::new(AtomicBool::new(false));
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("commands".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let menu = ColumnarMenu::default().with_name("commands");
    let mut editor = Reedline::create()
        .use_bracketed_paste(true)
        .with_transient_prompt(Box::new(InputPrompt::history()))
        .with_completer(Box::new(CommandCompleter {
            versions: Arc::clone(&versions),
            working_available: Arc::clone(&working_available),
        }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));
    match history {
        Ok(history) => editor = editor.with_history(Box::new(history)),
        Err(error) => eprintln!("! 无法加载项目输入历史，将使用会话内历史：{error}"),
    }
    let mut basic_input = false;
    loop {
        let project = application.project_view();
        working_available.store(project.working_score.is_some(), Ordering::Relaxed);
        if let Ok(mut candidates) = versions.write() {
            *candidates = project
                .versions
                .iter()
                .map(|version| format!("v{}", version.version))
                .collect();
        }
        let conversation = application.conversation_view();
        let prompt = InputPrompt::active(&project, &conversation.next_step);
        let signal = if basic_input {
            read_basic_terminal_line(&prompt)
        } else {
            match editor.read_line(&prompt) {
                Err(error) if is_cursor_position_timeout(&error) => {
                    basic_input = true;
                    eprintln!(
                        "\n! 当前终端未响应光标位置查询，已切换到基础输入模式；当前会话和生成结果不受影响。"
                    );
                    read_basic_terminal_line(&prompt)
                }
                result => result.map_err(anyhow::Error::from),
            }
        };
        match signal {
            Ok(Signal::Success(line)) => {
                if !execute_line(application, &line, &mut TerminalReporter::new(true)).await? {
                    break;
                }
            }
            Ok(Signal::CtrlC) => println!("! 已清空当前输入"),
            Ok(Signal::CtrlD) => break,
            Ok(_) => {}
            Err(error) => return Err(error).context("终端输入失败"),
        }
    }
    Ok(())
}

fn is_cursor_position_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Other
        && error.to_string() == "The cursor position could not be read within a normal duration"
}

fn read_basic_terminal_line(prompt: &InputPrompt) -> Result<Signal> {
    let mut output = std::io::stdout().lock();
    write!(
        output,
        "{}{}",
        prompt.render_prompt_left(),
        prompt.render_prompt_indicator(PromptEditMode::Default)
    )?;
    output.flush()?;
    drop(output);

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(Signal::CtrlD);
    }
    while matches!(line.chars().last(), Some('\n' | '\r')) {
        line.pop();
    }
    Ok(Signal::Success(line))
}

#[derive(Clone)]
struct InputPrompt {
    context: String,
}

impl InputPrompt {
    fn active(project: &ProjectView, status: &str) -> Self {
        Self {
            context: format!("\n项目 · {}\n状态 · {status}\n", project_summary(project)),
        }
    }

    fn history() -> Self {
        Self {
            context: String::new(),
        }
    }
}

impl Prompt for InputPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.context)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("› ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("· ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let failed = match history_search.status {
            PromptHistorySearchStatus::Failing => "无匹配 ",
            PromptHistorySearchStatus::Passing => "",
        };
        Cow::Owned(format!("({failed}历史搜索：{}) ", history_search.term))
    }
}

async fn run_plain<R: BufRead, W: Write>(
    application: &mut Application,
    mut reader: R,
    mut writer: W,
) -> Result<()> {
    writeln!(writer, "{}", project_summary(&application.project_view()))?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let result = match parse(&line) {
            Ok(action) => {
                let mut reporter = PlainReporter {
                    writer: &mut writer,
                    model_open: false,
                };
                application.execute(action, &mut reporter).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(ActionResult::Quit) => break,
            Ok(result) => render_result(&result, &mut writer)?,
            Err(error) => writeln!(writer, "错误：{error:#}\n下一步：输入 /help 查看可用操作。")?,
        }
    }
    Ok(())
}

async fn execute_line(
    application: &mut Application,
    line: &str,
    reporter: &mut TerminalReporter,
) -> Result<bool> {
    let mut action = match parse(line) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("错误：{error}");
            return Ok(true);
        }
    };
    action = match action {
        UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::PromptModel)) => {
            UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::Model(
                prompt_plain_value("模型名称：")?,
            )))
        }
        UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::PromptUrl)) => {
            UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::Url(
                prompt_plain_value("API Base URL：")?,
            )))
        }
        other => other,
    };
    if matches!(
        action,
        UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::ApiKey(
            None
        )))
    ) {
        let key = rpassword::prompt_password("模型密钥（输入隐藏）：")?;
        action = UserAction::Project(ProjectAction::Config(crate::command::ConfigAction::ApiKey(
            Some(key),
        )));
    }
    let direct_alda = matches!(
        &action,
        UserAction::Alda(_) | UserAction::Project(ProjectAction::Adopt(_))
    );
    let cancellation = CancellationToken::default();
    application.set_cancellation(cancellation.clone());
    let alda_active = Arc::clone(&reporter.alda_active);
    let result = {
        let operation = application.execute(action, reporter);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => Some(result),
            signal = tokio::signal::ctrl_c() => {
                signal.context("无法监听 Ctrl+C")?;
                cancellation.cancel();
                if direct_alda || alda_active.load(Ordering::SeqCst) {
                    let _ = operation.await;
                }
                None
            }
        }
    };
    reporter.finish_operation();
    let Some(result) = result else {
        eprintln!(
            "! 已取消当前操作；当前有效版本未改变。\n  下一步：重新输入要求，或试听当前版本。"
        );
        return Ok(true);
    };
    match result {
        Ok(ActionResult::Quit) => Ok(false),
        Ok(result) => {
            let mut output = std::io::stdout().lock();
            render_result(&result, &mut output)?;
            Ok(true)
        }
        Err(error) => {
            eprintln!("错误：{error:#}\n下一步：输入 /help 查看可用操作。");
            Ok(true)
        }
    }
}

fn prompt_plain_value(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    let value = value.trim().to_string();
    if value.is_empty() {
        anyhow::bail!("输入不能为空");
    }
    Ok(value)
}

fn render_result(result: &ActionResult, writer: &mut impl Write) -> Result<()> {
    match result {
        ActionResult::Message(message) => writeln!(writer, "{message}")?,
        ActionResult::Checks(checks) => render_checks(checks, writer)?,
        ActionResult::AgentCompleted {
            kind: crate::agent::AgentResultKind::Draft,
            success: true,
            rounds,
            ..
        } => writeln!(
            writer,
            "✓ 已更新草稿 · {rounds} 轮完成 · 当前有效版本未改变\n  下一步：/alda play work 试听，或继续输入发展要求。"
        )?,
        ActionResult::AgentCompleted {
            kind: crate::agent::AgentResultKind::Candidate,
            success: true,
            rounds,
            ..
        } => writeln!(
            writer,
            "✓ 完整候选已就绪 · {rounds} 轮完成 · 尚未创建版本\n  下一步：/alda play work 试听，再 /project accept 或继续修改。"
        )?,
        ActionResult::AgentCompleted {
            needs_input: true, ..
        } => writeln!(writer, "? 等待补充信息 · 直接回答上面的问题")?,
        ActionResult::AgentCompleted {
            kind: crate::agent::AgentResultKind::Plan,
            ..
        } => writeln!(writer, "✓ 已提出创作计划 · 当前乐谱和版本未改变")?,
        ActionResult::AgentCompleted {
            kind: crate::agent::AgentResultKind::Answer,
            ..
        } => writeln!(writer, "✓ 已回答 · 当前乐谱和版本未改变")?,
        ActionResult::AgentCompleted {
            rounds,
            working_score_status,
            ..
        } => writeln!(
            writer,
            "! {rounds} 轮后修正仍未完成；新候选未保存，{working_score_status}；当前有效版本未改变。\n  下一步：输入“继续修正”，也可以提出新的要求。"
        )?,
        ActionResult::None | ActionResult::Quit => {}
    }
    Ok(())
}

fn project_summary(view: &ProjectView) -> String {
    [
        view.name.clone(),
        view.current_version
            .map_or_else(|| "尚无版本".to_string(), |version| format!("v{version}")),
        if view.mode == "improv" {
            "即兴片段"
        } else {
            "完整曲目"
        }
        .to_string(),
    ]
    .join(" · ")
}

fn render_checks(checks: &[AldaCheck], writer: &mut impl Write) -> Result<()> {
    for check in checks {
        let icon = match check.status {
            CheckStatus::Pass => "✓",
            CheckStatus::Fail => "!",
            CheckStatus::Unchecked => "-",
        };
        writeln!(writer, "{icon} {} · {}", check.name, check.detail)?;
    }
    Ok(())
}

struct PlainReporter<'a, W> {
    writer: &'a mut W,
    model_open: bool,
}

struct SensitiveHistory {
    inner: FileBackedHistory,
}

impl History for SensitiveHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        if contains_inline_api_key(&item.command_line) {
            Ok(item)
        } else {
            self.inner.save(item)
        }
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        self.inner.clear()
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(id)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.inner.sync()
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

fn sanitize_history_file(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("无法读取项目输入历史 {}", path.display()))?;
    let retained = contents
        .lines()
        .filter(|line| !contains_inline_api_key(line))
        .collect::<Vec<_>>();
    if retained.len() == contents.lines().count() {
        return Ok(());
    }
    let sanitized = if retained.is_empty() {
        String::new()
    } else {
        format!("{}\n", retained.join("\n"))
    };
    let temporary = path.with_extension(format!("history-sanitize-{}", std::process::id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("无法创建脱敏历史 {}", temporary.display()))?;
        file.write_all(sanitized.as_bytes())
            .with_context(|| format!("无法写入脱敏历史 {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("无法更新脱敏历史 {}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
impl<W: Write> AgentReporter for PlainReporter<'_, W> {
    fn report(&mut self, event: AgentEvent) {
        if let AgentEvent::ModelText(text) = &event {
            if !self.model_open {
                let _ = write!(self.writer, "模型\n  ");
                self.model_open = true;
            }
            let _ = write!(self.writer, "{}", text.replace('\n', "\n  "));
            let _ = self.writer.flush();
        } else {
            if self.model_open {
                let _ = writeln!(self.writer);
                self.model_open = false;
            }
            let _ = render_event(&event, self.writer);
        }
    }
}

struct TerminalReporter {
    spinner: Option<ProgressBar>,
    animate: bool,
    alda_active: Arc<AtomicBool>,
    model_open: bool,
}
impl TerminalReporter {
    fn new(animate: bool) -> Self {
        Self {
            spinner: None,
            animate: animate && std::env::var_os("NO_COLOR").is_none(),
            alda_active: Arc::new(AtomicBool::new(false)),
            model_open: false,
        }
    }
    fn stage(&mut self, message: &str, activity: &'static str) {
        self.finish_spinner();
        eprintln!("◇ {message}");
        if self.animate {
            let spinner = ProgressBar::new_spinner();
            spinner.set_draw_target(ProgressDrawTarget::stderr());
            spinner.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
            spinner.enable_steady_tick(Duration::from_millis(90));
            spinner.set_message(activity);
            self.spinner = Some(spinner);
        }
    }
    fn finish_spinner(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
    }
    fn close_model(&mut self) {
        if self.model_open {
            eprintln!();
            self.model_open = false;
        }
    }
    fn finish_operation(&mut self) {
        self.close_model();
        self.finish_spinner();
        self.alda_active.store(false, Ordering::SeqCst);
    }
}
impl AgentReporter for TerminalReporter {
    fn report(&mut self, event: AgentEvent) {
        match &event {
            AgentEvent::PrivacyNotice => {
                self.close_model();
                eprintln!("! 隐私提示 · 创作要求、当前乐谱和校验错误将发送到配置的模型服务");
            }
            AgentEvent::RoundStarted { attempt } => {
                self.close_model();
                self.stage(
                    &format!("Agent · 第 {attempt} 次提交 · 等待模型"),
                    "等待模型响应",
                );
            }
            AgentEvent::ToolContinuationStarted { turn } => {
                self.close_model();
                self.stage(
                    &format!("Agent · 工具返回后继续 · 第 {turn} 次往返"),
                    "等待模型响应",
                );
            }
            AgentEvent::ToolProtocolRetry { call_count } => {
                self.close_model();
                self.finish_spinner();
                eprintln!("↻ Agent · 自动恢复工具协议 · 拒绝了 {call_count} 个并行调用");
            }
            AgentEvent::ToolCallMissingRetry => {
                self.close_model();
                self.finish_spinner();
                eprintln!("↻ Agent · 自动恢复工具协议 · 模型未调用工具");
            }
            AgentEvent::ToolArgumentsRetry { tool_name } => {
                self.close_model();
                self.finish_spinner();
                eprintln!("↻ Agent · 自动恢复工具参数 · {tool_name} 参数不完整或无效");
            }
            AgentEvent::ValidationStarted { attempt } => {
                self.close_model();
                self.alda_active.store(true, Ordering::SeqCst);
                self.stage(
                    &format!("Tool · 候选校验 · 第 {attempt} 次提交"),
                    "正在运行 Alda",
                );
            }
            AgentEvent::ValidationCompleted(_) => {
                self.close_model();
                self.alda_active.store(false, Ordering::SeqCst);
                self.finish_spinner();
                let mut out = std::io::stderr().lock();
                let _ = render_event(&event, &mut out);
            }
            AgentEvent::ModelText(text) => {
                self.finish_spinner();
                let mut out = std::io::stderr().lock();
                if !self.model_open {
                    let _ = write!(out, "模型\n  ");
                    self.model_open = true;
                }
                let _ = write!(out, "{}", text.replace('\n', "\n  "));
                let _ = out.flush();
            }
            AgentEvent::RevisionStarted { .. } => {
                self.close_model();
                self.finish_spinner();
                let mut out = std::io::stderr().lock();
                let _ = render_event(&event, &mut out);
            }
        }
    }
}
impl Drop for TerminalReporter {
    fn drop(&mut self) {
        self.finish_operation();
    }
}

fn render_event(event: &AgentEvent, writer: &mut impl Write) -> Result<()> {
    match event {
        AgentEvent::PrivacyNotice => writeln!(
            writer,
            "! 隐私提示 · 创作要求、当前乐谱和校验错误将发送到配置的模型服务"
        )?,
        AgentEvent::RoundStarted { attempt } => {
            writeln!(writer, "◇ Agent · 第 {attempt} 次提交 · 等待模型")?;
        }
        AgentEvent::ToolContinuationStarted { turn } => writeln!(
            writer,
            "◇ Agent · 工具返回后继续 · 第 {turn} 次往返 · 等待模型"
        )?,
        AgentEvent::ToolProtocolRetry { call_count } => writeln!(
            writer,
            "↻ Agent · 自动恢复工具协议 · 拒绝了 {call_count} 个并行调用"
        )?,
        AgentEvent::ToolCallMissingRetry => {
            writeln!(writer, "↻ Agent · 自动恢复工具协议 · 模型未调用工具")?;
        }
        AgentEvent::ToolArgumentsRetry { tool_name } => writeln!(
            writer,
            "↻ Agent · 自动恢复工具参数 · {tool_name} 参数不完整或无效"
        )?,
        AgentEvent::ModelText(text) => {
            if !text.trim().is_empty() {
                writeln!(writer, "模型\n  {}", text.trim().replace('\n', "\n  "))?;
            }
        }
        AgentEvent::ValidationStarted { attempt } => {
            writeln!(writer, "◇ Tool · 候选校验 · 第 {attempt} 次提交")?;
        }
        AgentEvent::ValidationCompleted(checks) => render_checks(checks, writer)?,
        AgentEvent::RevisionStarted {
            next_attempt,
            failures,
        } => writeln!(
            writer,
            "↻ Agent · 继续自动修正 · 下一次提交 {next_attempt} · {failures} 项未通过"
        )?,
    }
    Ok(())
}

struct CommandCompleter {
    versions: Arc<RwLock<Vec<String>>>,
    working_available: Arc<AtomicBool>,
}
impl Completer for CommandCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let prefix = &line[..pos];
        if prefix.starts_with("/alda check --file ") || prefix.starts_with("/project adopt ") {
            let start = prefix.rfind(' ').map_or(0, |index| index + 1);
            return path_candidates(&prefix[start..])
                .into_iter()
                .map(|candidate| suggestion(candidate, start, pos))
                .collect();
        }
        if let Some((lead, fragment, supports_working)) = target_fragment(prefix) {
            let mut candidates = self
                .versions
                .read()
                .map_or_else(|_| Vec::new(), |versions| versions.clone());
            if supports_working && self.working_available.load(Ordering::Relaxed) {
                candidates.push("work".to_string());
            }
            return candidates
                .into_iter()
                .filter(|candidate| candidate.starts_with(fragment))
                .map(|candidate| suggestion(candidate, lead, pos))
                .collect();
        }
        let (candidates, start) = if let Some(rest) = prefix.strip_prefix("/alda ") {
            (ALDA_COMMANDS, pos - rest.len())
        } else if let Some(rest) = prefix.strip_prefix("/project skills ") {
            (SKILL_COMMANDS, pos - rest.len())
        } else if let Some(rest) = prefix.strip_prefix("/project config ") {
            (CONFIG_COMMANDS, pos - rest.len())
        } else if let Some(rest) = prefix.strip_prefix("/project ") {
            (PROJECT_COMMANDS, pos - rest.len())
        } else {
            (TOP_LEVEL_COMMANDS, 0)
        };
        let fragment = &prefix[start..];
        candidates
            .iter()
            .filter(|candidate| candidate.starts_with(fragment))
            .map(|candidate| suggestion((*candidate).to_string(), start, pos))
            .collect()
    }
}

fn target_fragment(prefix: &str) -> Option<(usize, &str, bool)> {
    let supports_working = ["/alda play ", "/alda check "]
        .iter()
        .any(|command| prefix.starts_with(command));
    let supports_versions = supports_working
        || ["/alda export ", "/project switch "]
            .iter()
            .any(|command| prefix.starts_with(command));
    supports_versions.then(|| {
        let start = prefix.rfind(' ').map_or(0, |index| index + 1);
        (start, &prefix[start..], supports_working)
    })
}

fn path_candidates(fragment: &str) -> Vec<String> {
    let path = std::path::Path::new(fragment);
    let explicit_parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let directory = explicit_parent.unwrap_or_else(|| std::path::Path::new("."));
    let file_prefix = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    std::fs::read_dir(directory).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.starts_with(file_prefix) {
                        return None;
                    }
                    let candidate = explicit_parent.map_or_else(
                        || std::path::PathBuf::from(&name),
                        |parent| parent.join(&name),
                    );
                    let mut value = candidate.to_string_lossy().into_owned();
                    if entry.path().is_dir() {
                        value.push('/');
                    }
                    Some(value)
                })
                .collect()
        },
    )
}

fn suggestion(value: String, start: usize, end: usize) -> Suggestion {
    Suggestion {
        value,
        span: Span::new(start, end),
        append_whitespace: true,
        ..Suggestion::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_view() -> ProjectView {
        ProjectView {
            name: "poem".into(),
            first_request: None,
            current_version: Some(2),
            working_score: None,
            versions: vec![],
            mode: "full".into(),
            target_duration_secs: Some(crate::instructions::DurationConstraint::exact(180.0)),
            included_instruments: vec!["piano".into()],
            excluded_instruments: vec!["violin".into()],
            enabled_advisory_skills: vec![],
            model_name: Some("example-model".into()),
            model_url: Some("https://api.example.com".into()),
            model_key_configured: true,
            alda_available: true,
            model_configured: true,
            model_service_status: "最近成功".into(),
        }
    }

    #[test]
    fn summary_keeps_project_context() {
        assert_eq!(project_summary(&project_view()), "poem · v2 · 完整曲目");
    }

    #[test]
    fn working_score_is_completed_only_for_commands_that_support_it() {
        let versions = Arc::new(RwLock::new(vec!["v1".to_string()]));
        let working_available = Arc::new(AtomicBool::new(true));
        let mut completer = CommandCompleter {
            versions,
            working_available: Arc::clone(&working_available),
        };

        let play = completer.complete("/alda play w", "/alda play w".len());
        assert_eq!(
            play.into_iter()
                .map(|suggestion| suggestion.value)
                .collect::<Vec<_>>(),
            ["work"]
        );
        assert!(
            completer
                .complete("/project switch w", "/project switch w".len())
                .is_empty()
        );

        working_available.store(false, Ordering::Relaxed);
        assert!(
            completer
                .complete("/alda check w", "/alda check w".len())
                .is_empty()
        );
    }

    #[test]
    fn active_prompt_separates_project_status_and_input() {
        let prompt = InputPrompt::active(&project_view(), "已发起播放 v1 · /alda stop 停止");

        assert_eq!(
            prompt.render_prompt_left(),
            "\n项目 · poem · v2 · 完整曲目\n状态 · 已发起播放 v1 · /alda stop 停止\n"
        );
        assert_eq!(prompt.render_prompt_indicator(PromptEditMode::Emacs), "› ");
        assert_eq!(prompt.render_prompt_multiline_indicator(), "· ");
    }

    #[test]
    fn submitted_prompt_keeps_only_the_user_input_marker() {
        let prompt = InputPrompt::history();

        assert_eq!(prompt.render_prompt_left(), "");
        assert_eq!(prompt.render_prompt_right(), "");
        assert_eq!(prompt.render_prompt_indicator(PromptEditMode::Emacs), "› ");
    }

    #[test]
    fn sanitizes_only_inline_api_keys_from_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".repl-history");
        std::fs::write(
            &path,
            "/project\n/project config key secret-value\n/project config key\n自然语言要求\n",
        )
        .unwrap();

        sanitize_history_file(&path).unwrap();

        let history = std::fs::read_to_string(path).unwrap();
        assert_eq!(history, "/project\n/project config key\n自然语言要求\n");
        assert!(!history.contains("secret-value"));
    }

    #[test]
    fn sensitive_history_never_persists_inline_api_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".repl-history");
        let inner = FileBackedHistory::with_file(10, path.clone()).unwrap();
        let mut history = SensitiveHistory { inner };

        history
            .save(HistoryItem::from_command_line("/project"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line(
                " /project config key secret-value",
            ))
            .unwrap();
        history.sync().unwrap();

        let persisted = std::fs::read_to_string(path).unwrap();
        assert_eq!(persisted, "/project\n");
        assert!(!persisted.contains("secret-value"));
    }
}
