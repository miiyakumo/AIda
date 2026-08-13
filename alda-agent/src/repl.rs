use crate::agent::{AgentEvent, AgentReporter};
use crate::alda::{AldaCheck, CancellationToken, CheckStatus};
use crate::application::{ActionResult, Application, ProjectView};
use crate::command::{
    ALDA_COMMANDS, CONFIG_COMMANDS, PROJECT_COMMANDS, TOP_LEVEL_COMMANDS, contains_inline_api_key,
    parse,
};
use crate::command::{ProjectAction, UserAction};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reedline::{
    ColumnarMenu, Completer, DefaultPrompt, DefaultPromptSegment, Emacs, FileBackedHistory,
    History, HistoryItem, HistoryItemId, HistorySessionId, KeyCode, KeyModifiers, MenuBuilder,
    Reedline, ReedlineEvent, ReedlineMenu, SearchQuery, Signal, Span, Suggestion,
    default_emacs_keybindings,
};
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
        .with_completer(Box::new(CommandCompleter {
            versions: Arc::clone(&versions),
        }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));
    match history {
        Ok(history) => editor = editor.with_history(Box::new(history)),
        Err(error) => eprintln!("! 无法加载项目输入历史，将使用会话内历史：{error}"),
    }
    loop {
        let project = application.project_view();
        if let Ok(mut candidates) = versions.write() {
            *candidates = project
                .versions
                .iter()
                .map(|version| format!("v{}", version.version))
                .collect();
        }
        let conversation = application.conversation_view();
        println!("{}\n{}", project_summary(&project), conversation.next_step);
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(String::new()),
            DefaultPromptSegment::Empty,
        );
        match editor.read_line(&prompt) {
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

fn render_result(result: &ActionResult, writer: &mut impl Write) -> Result<()> {
    match result {
        ActionResult::Message(message) => writeln!(writer, "{message}")?,
        ActionResult::Checks(checks) => render_checks(checks, writer)?,
        ActionResult::AgentCompleted {
            version: Some(version),
            rounds,
            ..
        } => writeln!(
            writer,
            "✓ 已保存 v{version} · {rounds} 轮完成\n  下一步：/alda play 试听，或直接输入修改要求。"
        )?,
        ActionResult::AgentCompleted {
            needs_input: true, ..
        } => writeln!(writer, "? 等待补充信息 · 直接回答上面的问题")?,
        ActionResult::AgentCompleted { rounds, .. } => writeln!(
            writer,
            "! {rounds} 轮后修正仍未完成；当前有效版本未改变。\n  下一步：输入“继续修正”，也可以提出新的要求。"
        )?,
        ActionResult::None | ActionResult::Quit => {}
    }
    Ok(())
}

fn project_summary(view: &ProjectView) -> String {
    let mut parts = vec![
        view.name.clone(),
        view.current_version
            .map_or_else(|| "尚无版本".to_string(), |version| format!("v{version}")),
        if view.mode == "improv" {
            "即兴片段"
        } else {
            "完整曲目"
        }
        .to_string(),
    ];
    if let Some(duration) = view.target_duration_secs {
        parts.push(format!("{duration} 秒"));
    }
    parts.extend(
        view.included_instruments
            .iter()
            .map(|value| format!("+{value}")),
    );
    parts.extend(
        view.excluded_instruments
            .iter()
            .map(|value| format!("-{value}")),
    );
    parts.join(" · ")
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
            AgentEvent::RoundStarted { round, max_rounds } => {
                self.close_model();
                self.stage(
                    &format!("Agent · 生成第 {round}/{max_rounds} 轮 · 等待模型"),
                    "等待模型响应",
                );
            }
            AgentEvent::ValidationStarted { round, max_rounds } => {
                self.close_model();
                self.alda_active.store(true, Ordering::SeqCst);
                self.stage(
                    &format!("Tool · Alda 校验 · 第 {round}/{max_rounds} 轮"),
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
        AgentEvent::RoundStarted { round, max_rounds } => writeln!(
            writer,
            "◇ Agent · 生成第 {round}/{max_rounds} 轮 · 等待模型"
        )?,
        AgentEvent::ModelText(text) => {
            if !text.trim().is_empty() {
                writeln!(writer, "模型\n  {}", text.trim().replace('\n', "\n  "))?;
            }
        }
        AgentEvent::ValidationStarted { round, max_rounds } => {
            writeln!(writer, "◇ Tool · Alda 校验 · 第 {round}/{max_rounds} 轮")?;
        }
        AgentEvent::ValidationCompleted(checks) => render_checks(checks, writer)?,
        AgentEvent::RevisionStarted {
            next_round,
            max_rounds,
            failures,
        } => writeln!(
            writer,
            "↻ Agent · 自动修正第 {next_round}/{max_rounds} 轮 · {failures} 项未通过"
        )?,
    }
    Ok(())
}

struct CommandCompleter {
    versions: Arc<RwLock<Vec<String>>>,
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
        if let Some((lead, fragment)) = version_fragment(prefix) {
            return self
                .versions
                .read()
                .map_or_else(|_| Vec::new(), |versions| versions.clone())
                .into_iter()
                .filter(|candidate| candidate.starts_with(fragment))
                .map(|candidate| suggestion(candidate, lead, pos))
                .collect();
        }
        let (candidates, start) = if let Some(rest) = prefix.strip_prefix("/alda ") {
            (ALDA_COMMANDS, pos - rest.len())
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

fn version_fragment(prefix: &str) -> Option<(usize, &str)> {
    let supports_versions = [
        "/alda play ",
        "/alda check ",
        "/alda export ",
        "/project switch ",
    ]
    .iter()
    .any(|command| prefix.starts_with(command));
    supports_versions.then(|| {
        let start = prefix.rfind(' ').map_or(0, |index| index + 1);
        (start, &prefix[start..])
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
    #[test]
    fn summary_keeps_project_context() {
        let view = ProjectView {
            name: "poem".into(),
            first_request: None,
            current_version: Some(2),
            versions: vec![],
            mode: "full".into(),
            target_duration_secs: Some(180.0),
            included_instruments: vec!["piano".into()],
            excluded_instruments: vec!["violin".into()],
            creative_strategy: None,
            model_name: Some("example-model".into()),
            model_url: Some("https://api.example.com".into()),
            model_key_configured: true,
            alda_available: true,
            model_configured: true,
            model_service_status: "最近成功".into(),
        };
        assert_eq!(
            project_summary(&view),
            "poem · v2 · 完整曲目 · 180 秒 · +piano · -violin"
        );
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
