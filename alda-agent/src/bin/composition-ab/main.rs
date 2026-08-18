mod domain;
mod protocol;

use alda_agent::agent::{Agent, CreationRequest, RunPolicy};
use alda_agent::alda::{AldaRunner, ScoreInfo, find_alda};
use alda_agent::audio::{ArtifactReport, AudioRenderer};
use alda_agent::composition::{
    SectionArtifact, TimelineVerification, assemble_sections, verify_timeline,
};
use alda_agent::config::ModelConfig;
use alda_agent::deepseek::DeepSeekClient;
use alda_agent::instructions::{
    CompiledInstructions, CreationMode, DurationConstraint, InstructionProfile, ProjectPreferences,
};
use alda_agent::skills::SkillCatalog;
use anyhow::{Context, Result, bail};
use clap::Parser;
use domain::{
    BudgetCompilation, ComposerPlan, ReviewReport, SectionFamily, WorkerSubmission, validate_review,
};
use protocol::{RoleSession, RoleStats};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "composition-ab",
    about = "隔离运行单 Agent 与角色工作流 A/B 实验"
)]
struct Args {
    /// 两个实验臂共同使用的创作任务文件。
    #[arg(short, long)]
    file: PathBuf,
    /// 目标时长（秒）。
    #[arg(long, default_value_t = 300.0)]
    duration: f64,
    /// 新建的实验输出目录；为保护已有产物，不允许覆盖。
    #[arg(short, long)]
    output: PathBuf,
    /// model.json 所在目录。
    #[arg(long, default_value = ".")]
    config_root: PathBuf,
}

#[derive(Serialize)]
struct ExperimentReport {
    task: String,
    task_sha256: String,
    target_duration_secs: f64,
    model: String,
    base_url: String,
    run_order: [&'static str; 2],
    baseline: ArmOutcome<BaselineReport>,
    roles: ArmOutcome<RolesReport>,
}

#[derive(Serialize)]
struct ArmOutcome<T: Serialize> {
    success: bool,
    elapsed_secs: f64,
    result: Option<T>,
    error: Option<String>,
}

impl<T: Serialize> ArmOutcome<T> {
    fn from_result(result: Result<T>, elapsed_secs: f64) -> Self {
        match result {
            Ok(result) => Self {
                success: true,
                elapsed_secs,
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                success: false,
                elapsed_secs,
                result: None,
                error: Some(format!("{error:#}")),
            },
        }
    }
}

#[derive(Serialize)]
struct BaselineReport {
    elapsed_secs: f64,
    stats: alda_agent::agent::GenerationStats,
    interpretation: String,
    checks: Vec<alda_agent::alda::AldaCheck>,
    artifact: ArtifactReport,
}

#[derive(Serialize)]
struct RolesReport {
    elapsed_secs: f64,
    budget: BudgetCompilation,
    composer_stats: RoleStats,
    theme_worker_stats: RoleStats,
    development_worker_stats: RoleStats,
    reviewer_stats: RoleStats,
    timeline: TimelineVerification,
    score: ScoreInfo,
    checks: Vec<alda_agent::alda::AldaCheck>,
    artifact: ArtifactReport,
    review: ReviewReport,
}

struct WorkerResult {
    submission: WorkerSubmission,
    stats: RoleStats,
}

enum WorkerOutcome {
    Success(WorkerResult),
    Failure {
        stats: RoleStats,
        last_submission: Option<WorkerSubmission>,
        error: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    let task = fs::read_to_string(&args.file)
        .with_context(|| format!("无法读取任务文件 {}", args.file.display()))?;
    if task.trim().is_empty() {
        bail!("任务文件不能为空");
    }
    create_output_tree(&args.output)?;
    fs::write(args.output.join("task.txt"), &task)?;

    let config = ModelConfig::load(&args.config_root)?.resolve()?;
    let alda_path = find_alda().context("未找到 alda")?;
    let alda = AldaRunner::new(alda_path);
    let renderer = AudioRenderer::discover()?;
    let client = DeepSeekClient::new(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
    )?;

    let task_sha256 = format!("{:x}", Sha256::digest(task.as_bytes()));
    let baseline_started = Instant::now();
    let baseline_result = run_baseline(
        &task,
        args.duration,
        &args.output.join("baseline"),
        client.clone(),
        alda.clone(),
        renderer.clone(),
    )
    .await;
    let baseline =
        ArmOutcome::from_result(baseline_result, baseline_started.elapsed().as_secs_f64());
    let roles_started = Instant::now();
    let roles_result = run_roles(
        &task,
        args.duration,
        &args.output.join("roles"),
        client,
        alda,
        renderer,
    )
    .await;
    let roles = ArmOutcome::from_result(roles_result, roles_started.elapsed().as_secs_f64());
    let complete = baseline.success && roles.success;
    write_json(
        &args.output.join("report.json"),
        &ExperimentReport {
            task,
            task_sha256,
            target_duration_secs: args.duration,
            model: config.model,
            base_url: config.base_url,
            run_order: ["baseline", "roles"],
            baseline,
            roles,
        },
    )?;
    if !complete {
        bail!(
            "A/B 至少一个实验臂失败；已将可审计结果保存到 {}",
            args.output.join("report.json").display()
        );
    }
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    if !args.duration.is_finite() || args.duration <= 0.0 {
        bail!("duration 必须是正有限数");
    }
    if args.output.exists() {
        bail!("输出目录已存在，拒绝覆盖：{}", args.output.display());
    }
    Ok(())
}

fn create_output_tree(root: &Path) -> Result<()> {
    for child in ["baseline", "roles"] {
        fs::create_dir_all(root.join(child))?;
    }
    Ok(())
}

async fn run_baseline(
    task: &str,
    duration: f64,
    output: &Path,
    client: DeepSeekClient,
    alda: AldaRunner,
    renderer: AudioRenderer,
) -> Result<BaselineReport> {
    let preferences = preferences(duration);
    let catalog = SkillCatalog::discover(None, None)?;
    let instructions =
        CompiledInstructions::compile(&catalog, &InstructionProfile::default(), &preferences)?;
    let agent = Agent::new(client, alda.clone()).with_audio_renderer(renderer.clone());
    let started = Instant::now();
    let result = agent
        .create_candidate(CreationRequest {
            source_material: task.to_string(),
            instructions: "请直接完成一首完整曲目并提交 candidate。".to_string(),
            compiled_instructions: instructions,
            run_policy: RunPolicy {
                max_elapsed: Duration::from_secs(30 * 60),
                max_model_calls: 24,
                max_protocol_recoveries: 8,
            },
        })
        .await?;
    let elapsed_secs = started.elapsed().as_secs_f64();
    if !result.success {
        if let Some(source) = result.alda_code.as_deref() {
            fs::write(output.join("failed-score.alda"), source)?;
        }
        write_json(
            &output.join("failure.json"),
            &json!({
                "elapsed_secs": elapsed_secs,
                "stats": result.stats,
                "kind": format!("{:?}", result.kind),
                "interpretation": result.interpretation,
                "checks": result.checks,
            }),
        )?;
        bail!("baseline 未生成通过检查的完整候选");
    }
    let source = result
        .alda_code
        .as_deref()
        .context("baseline 缺少 Alda 源码")?;
    let score_path = output.join("score.alda");
    fs::write(&score_path, source)?;
    let staged = result
        .candidate_artifacts
        .as_ref()
        .context("baseline 成功候选缺少已验证的 MIDI/WAV")?;
    let midi_path = output.join("score.mid");
    let wav_path = output.join("score.wav");
    fs::copy(staged.midi_path(), &midi_path)?;
    fs::copy(staged.wav_path(), &wav_path)?;
    let mut artifact = staged.report().clone();
    artifact.alda_path = score_path;
    artifact.midi_path = midi_path;
    artifact.wav_path = wav_path;
    Ok(BaselineReport {
        elapsed_secs,
        stats: result.stats,
        interpretation: result.interpretation,
        checks: result.checks,
        artifact,
    })
}

#[allow(clippy::too_many_lines)]
async fn run_roles(
    task: &str,
    duration: f64,
    output: &Path,
    client: DeepSeekClient,
    alda: AldaRunner,
    renderer: AudioRenderer,
) -> Result<RolesReport> {
    let started = Instant::now();
    let mut composer = composer_session(client.clone(), task, duration);
    let plan = match composer.submit(validate_experiment_plan).await {
        Ok(plan) => plan,
        Err(error) => {
            write_role_failure(
                &output.join("composer-outcome.json"),
                composer.stats(),
                &error,
            )?;
            return Err(error.context("Composer 失败"));
        }
    };
    write_json(&output.join("composer.json"), &plan)?;
    let budget = plan.compile_budget(duration)?;
    write_json(&output.join("budget.json"), &budget)?;
    write_json(
        &output.join("composer-outcome.json"),
        &json!({
            "success": true,
            "stats": composer.stats(),
            "submission": plan,
            "budget": budget,
        }),
    )?;

    let theme = run_worker(
        client.clone(),
        alda.clone(),
        plan.clone(),
        budget.clone(),
        SectionFamily::Theme,
    );
    let development = run_worker(
        client.clone(),
        alda.clone(),
        plan.clone(),
        budget.clone(),
        SectionFamily::Development,
    );
    let (theme, development) = tokio::join!(theme, development);
    write_worker_outcome(output, "theme", &theme)?;
    write_worker_outcome(output, "development", &development)?;
    let theme = worker_result(theme, "theme")?;
    let development = worker_result(development, "development")?;
    write_json(&output.join("theme-worker.json"), &theme.submission)?;
    write_json(
        &output.join("development-worker.json"),
        &development.submission,
    )?;

    let artifacts = merge_submissions(&plan, &theme.submission, &development.submission)?;
    let spec = plan.composition_spec(&budget)?;
    let assembly = assemble_sections(&spec, &artifacts)?;
    fs::write(output.join("probe.alda"), &assembly.probe_source)?;
    let probe_score = alda.parse(&output.join("probe.alda"))?;
    let timeline = verify_timeline(&spec, &assembly, &probe_score)?;
    let score_path = output.join("score.alda");
    fs::write(&score_path, &assembly.alda_source)?;
    let score = alda.parse(&score_path)?;
    let checks = alda.validate(
        &score_path,
        &[],
        &[],
        Some(DurationConstraint::exact(duration)),
        10.0,
    );
    if checks
        .iter()
        .any(|check| check.status == alda_agent::alda::CheckStatus::Fail)
    {
        write_json(&output.join("candidate-checks.json"), &checks)?;
        bail!("角色工作流完整候选未通过与 baseline 等价的 Alda/时长检查");
    }

    let mut reviewer = reviewer_session(client, &plan, &budget, &assembly.alda_source, &score)?;
    let review = match reviewer
        .submit(|report: &ReviewReport| validate_review(report, &plan))
        .await
    {
        Ok(review) => review,
        Err(error) => {
            write_role_failure(
                &output.join("reviewer-outcome.json"),
                reviewer.stats(),
                &error,
            )?;
            return Err(error.context("Reviewer 失败"));
        }
    };
    write_json(&output.join("reviewer.json"), &review)?;
    write_json(
        &output.join("reviewer-outcome.json"),
        &json!({
            "success": true,
            "stats": reviewer.stats(),
            "submission": review,
        }),
    )?;
    let artifact = renderer
        .render_score_async(
            alda,
            score_path,
            output.join("score.mid"),
            output.join("score.wav"),
        )
        .await?;
    if !review.approved {
        bail!("只读 Reviewer 提出了阻断性问题；保留已渲染产物供审计");
    }

    Ok(RolesReport {
        elapsed_secs: started.elapsed().as_secs_f64(),
        budget,
        composer_stats: composer.stats(),
        theme_worker_stats: theme.stats,
        development_worker_stats: development.stats,
        reviewer_stats: reviewer.stats(),
        timeline,
        score,
        checks,
        artifact,
        review,
    })
}

fn validate_experiment_plan(plan: &ComposerPlan) -> Result<()> {
    plan.validate()?;
    if plan.sections.len() != 4 || !(3..=4).contains(&plan.parts.len()) {
        bail!("本次 A/B 的 Composer 必须提交恰好 4 个段落和 3–4 个声部");
    }
    for family in [SectionFamily::Theme, SectionFamily::Development] {
        if plan
            .sections
            .iter()
            .filter(|section| section.family == family)
            .count()
            != 2
        {
            bail!("本次 A/B 的 theme 与 development 必须各有恰好 2 个段落");
        }
    }
    Ok(())
}

fn write_worker_outcome(output: &Path, family: &str, outcome: &WorkerOutcome) -> Result<()> {
    let value = match outcome {
        WorkerOutcome::Success(worker) => json!({
            "success": true,
            "stats": worker.stats,
            "submission": worker.submission,
        }),
        WorkerOutcome::Failure {
            stats,
            last_submission,
            error,
        } => json!({
            "success": false,
            "stats": stats,
            "last_submission": last_submission,
            "error": error,
        }),
    };
    write_json(
        &output.join(format!("{family}-worker-outcome.json")),
        &value,
    )
}

fn worker_result(outcome: WorkerOutcome, family: &str) -> Result<WorkerResult> {
    match outcome {
        WorkerOutcome::Success(result) => Ok(result),
        WorkerOutcome::Failure { error, .. } => bail!("{family} Worker 失败：{error}"),
    }
}

fn write_role_failure(path: &Path, stats: RoleStats, error: &anyhow::Error) -> Result<()> {
    write_json(
        path,
        &json!({
            "success": false,
            "stats": stats,
            "last_submission": null,
            "error": format!("{error:#}"),
        }),
    )
}

async fn run_worker(
    client: DeepSeekClient,
    alda: AldaRunner,
    plan: ComposerPlan,
    budget: BudgetCompilation,
    family: SectionFamily,
) -> WorkerOutcome {
    let mut session = match worker_session(client, &plan, &budget, family) {
        Ok(session) => session,
        Err(error) => {
            return WorkerOutcome::Failure {
                stats: RoleStats::default(),
                last_submission: None,
                error: format!("{error:#}"),
            };
        }
    };
    let mut submission = match session
        .submit(|value: &WorkerSubmission| validate_experiment_worker(value, &plan, family))
        .await
    {
        Ok(submission) => submission,
        Err(error) => {
            return WorkerOutcome::Failure {
                stats: session.stats(),
                last_submission: None,
                error: format!("{error:#}"),
            };
        }
    };
    if let Err(error) = verify_worker(&alda, &plan, &budget, &submission, family) {
        let required_submission_template = match required_worker_template(&plan, &budget, family) {
            Ok(template) => template,
            Err(template_error) => {
                return WorkerOutcome::Failure {
                    stats: session.stats(),
                    last_submission: Some(submission),
                    error: format!("无法重新生成 Worker 交付骨架：{template_error:#}"),
                };
            }
        };
        session.feedback(format!(
            "宿主真实 Alda 验证失败：{error:#}。请立即只调用 submit_section_family 一次，修正整个 {} 家族并重新提交；这是唯一一次技术返工。必须重新提交下面骨架中的每个段落和每个声部，不能只提交局部补丁。严格遵守每段的 phrase_beats 与 repeat_count：每个 body 只能写成 `oN [oN 恰好 phrase_beats 拍的音乐]*repeat_count`，方括号外除开头 oN 外不得有事件；反复体必须以绝对 oN 重置八度，体内禁用会跨反复累积的 < 和 >。不要输出解释文字。\nrequired_submission_template:\n{}",
            family.as_str(),
            serde_json::to_string_pretty(&required_submission_template)
                .expect("JSON value serialization cannot fail")
        ));
        submission = match session
            .submit(|value: &WorkerSubmission| validate_experiment_worker(value, &plan, family))
            .await
        {
            Ok(submission) => submission,
            Err(repair_error) => {
                return WorkerOutcome::Failure {
                    stats: session.stats(),
                    last_submission: Some(submission),
                    error: format!("Worker 技术返工提交失败：{repair_error:#}"),
                };
            }
        };
        if let Err(repair_error) = verify_worker(&alda, &plan, &budget, &submission, family) {
            return WorkerOutcome::Failure {
                stats: session.stats(),
                last_submission: Some(submission),
                error: format!("Worker 技术返工后仍未通过：{repair_error:#}"),
            };
        }
    }
    WorkerOutcome::Success(WorkerResult {
        submission,
        stats: session.stats(),
    })
}

fn validate_experiment_worker(
    submission: &WorkerSubmission,
    plan: &ComposerPlan,
    family: SectionFamily,
) -> Result<()> {
    submission.validate_contract(plan, family)?;
    if submission.sections.iter().any(|section| {
        section
            .parts
            .iter()
            .any(|part| part.alda_sequence_body.contains(['<', '>']))
    }) {
        bail!("本次 A/B 的反复乐句禁止使用会跨反复累积的 < 或 >；请改用绝对 o4/o5/o6")
    }
    Ok(())
}

fn verify_worker(
    alda: &AldaRunner,
    plan: &ComposerPlan,
    budget: &BudgetCompilation,
    submission: &WorkerSubmission,
    family: SectionFamily,
) -> Result<()> {
    let spec = plan.family_spec(budget, family)?;
    let assembly = assemble_sections(&spec, &submission.sections)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("worker-probe.alda");
    fs::write(&path, &assembly.probe_source)?;
    let score = alda.parse(&path)?;
    verify_timeline(&spec, &assembly, &score)?;
    Ok(())
}

fn merge_submissions(
    plan: &ComposerPlan,
    theme: &WorkerSubmission,
    development: &WorkerSubmission,
) -> Result<Vec<SectionArtifact>> {
    let mut artifacts = Vec::with_capacity(plan.sections.len());
    for section in &plan.sections {
        let source = match section.family {
            SectionFamily::Theme => &theme.sections,
            SectionFamily::Development => &development.sections,
        };
        let artifact = source
            .iter()
            .find(|artifact| artifact.section_id == section.id)
            .with_context(|| format!("缺少段落 {}", section.id))?;
        artifacts.push(artifact.clone());
    }
    Ok(artifacts)
}

fn composer_session(client: DeepSeekClient, task: &str, duration: f64) -> RoleSession {
    RoleSession::new(
        client,
        "你是唯一的 Composer。像音乐家一样设计全曲的速度、拍号、乐句网格、主题、和声、织体、曲式、配器与发展关系；不要写 Alda，也不要手算或提交任何绝对拍数。请恰好使用 4 个段落和 3–4 个声部形成完整曲式，theme 与 development 各分配恰好 2 个段落；仅用正 length_weight 表达段落相对比例。instrument 必须使用 Alda stock instrument 的安全名称，例如 flute、violin、cello 或 piano。",
        format!(
            "共同任务如下，目标时长约 {duration} 秒。宿主会按你的 tempo、meter、phrase_grid_bars 和相对权重确定性计算只读段落预算：\n\n{task}"
        ),
        "submit_composer_plan",
        "提交完整、声明性的作曲计划。",
        composer_schema(),
    )
}

fn worker_session(
    client: DeepSeekClient,
    plan: &ComposerPlan,
    budget: &BudgetCompilation,
    family: SectionFamily,
) -> Result<RoleSession> {
    let assigned = plan
        .sections
        .iter()
        .filter(|section| section.family == family)
        .collect::<Vec<_>>();
    let assigned_budgets = plan.family_budgets(budget, family);
    let required_submission_template = required_worker_template(plan, budget, family)?;
    let request = json!({
        "composer_plan": plan,
        "assigned_sections": assigned,
        "readonly_section_budgets": assigned_budgets,
        "required_submission_template": required_submission_template,
        "family": family,
    });
    Ok(RoleSession::new(
        client,
        "你是 Alda 段落 Worker。你只实现分配给你的段落家族，但每个段落必须实现 Composer 声明的全部声部。readonly_section_budgets 由宿主确定且不可修改；duration_beats 以四分音符为一拍，每个声部片段从局部 0 拍开始，必须恰好填满该段预算。required_submission_template 是必须完整填充的交付骨架：每段已经给出 phrase_beats 与 repeat_count；该段每个 body 必须且只能写成 `oN [oN 恰好 phrase_beats 拍的原生 Alda 音乐]*repeat_count`，方括号外除开头 oN 外不得再写任何事件。方括号内第一个事件必须再次用绝对 oN 重置八度；体内需要换音区时也只用绝对 o4/o5/o6，禁用 < 和 >，否则八度状态会跨反复累积并越出 MIDI 范围。你只需创作一个 phrase_grid_bars 小节的乐句并按指定次数反复，不要展开整段，也不要自行重算总拍数。提交时保留每个段落和每个声部并替换空字符串；duration_beats、phrase_beats、repeat_count 只是只读核算信息，不属于工具提交字段；返工时仍须提交整个家族。完成后立即只调用 submit_section_family 一次，不要输出解释文字。Alda 中 c4 表示四分音符 C，不表示 C4 音高；八度只能用独立的 o4/o5/o6 事件，禁止把 d5、c6 等写成音高。数字只能作 1、2、4、8、16、32 这些合法时值、绝对八度或方括号后的指定 repeat_count。升降号只能写 c+、b-；B-flat 必须写 b-，绝不能写 bb。和弦用斜杠，如 c4/e/g。c1/r1=4 拍、c2/r2=2 拍、c4/r4=1 拍、两个连续八分音符 c8 d 合计 1 拍；省略时值会继承前一事件。逐小节核算方括号内的乐句，总计必须恰好等于模板给出的 phrase_beats。alda_sequence_body 只能使用可直接放进宿主 variable sequence 的 notes、rests、chords、内联 sequences 和 repeats；不能声明变量、instrument part、voice、tempo、metric modulation 或 Marker，也不能引用任何名称或写解释性注释。",
        serde_json::to_string_pretty(&request)?,
        "submit_section_family",
        "提交一个段落家族的全部全声部 Alda 片段。",
        worker_schema(),
    ))
}

fn required_worker_template(
    plan: &ComposerPlan,
    budget: &BudgetCompilation,
    family: SectionFamily,
) -> Result<Value> {
    let assigned = plan
        .sections
        .iter()
        .filter(|section| section.family == family)
        .collect::<Vec<_>>();
    let assigned_budgets = plan.family_budgets(budget, family);
    if assigned.len() != assigned_budgets.len() {
        bail!("Worker 段落与预算数量不一致");
    }
    let phrase_beats = budget.grid_beats;
    let phrase_beats_label = beat_label(phrase_beats);
    let sections = assigned
        .iter()
        .zip(&assigned_budgets)
        .map(|(section, section_budget)| {
            let repeat_count = exact_repeat_count(section_budget.duration_beats, phrase_beats)?;
            Ok(json!({
                "section_id": section.id,
                "duration_beats": section_budget.duration_beats,
                "phrase_beats": phrase_beats,
                "repeat_count": repeat_count,
                "required_body_shape": format!(
                    "oN [oN exactly {phrase_beats_label} quarter-note beats of music; no < or >]*{repeat_count}"
                ),
                "parts": plan.parts.iter().map(|part| json!({
                    "part_id": part.id,
                    "alda_sequence_body": "",
                })).collect::<Vec<_>>(),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "family": family,
        "sections": sections,
    }))
}

fn beat_label(beat: alda_agent::composition::Beat) -> String {
    if beat.denominator == 1 {
        beat.numerator.to_string()
    } else {
        format!("{}/{}", beat.numerator, beat.denominator)
    }
}

fn exact_repeat_count(
    duration: alda_agent::composition::Beat,
    phrase: alda_agent::composition::Beat,
) -> Result<u32> {
    let numerator = u64::from(duration.numerator)
        .checked_mul(u64::from(phrase.denominator))
        .context("Worker repeat_count 分子溢出")?;
    let denominator = u64::from(duration.denominator)
        .checked_mul(u64::from(phrase.numerator))
        .context("Worker repeat_count 分母溢出")?;
    if denominator == 0 || numerator % denominator != 0 {
        bail!("段落预算不是乐句网格的整数倍");
    }
    u32::try_from(numerator / denominator).context("Worker repeat_count 超出支持范围")
}

fn reviewer_session(
    client: DeepSeekClient,
    plan: &ComposerPlan,
    budget: &BudgetCompilation,
    source: &str,
    score: &ScoreInfo,
) -> Result<RoleSession> {
    let request = json!({
        "composer_plan": plan,
        "section_budgets": budget,
        "assembled_alda": source,
        "parsed_score": score,
    });
    Ok(RoleSession::new(
        client,
        "你是只读音乐 Reviewer。只能依据计划、最终 Alda 和真实解析摘要审查：主题是否可辨、家族衔接、对位/织体、发展与高潮是否实现。不能改源码、调用其他工具或代替宿主接受作品。只有有具体段落和证据的问题才可列为 blocking。",
        serde_json::to_string_pretty(&request)?,
        "submit_review",
        "提交只读结构化审查报告。",
        review_schema(),
    ))
}

fn preferences(duration: f64) -> ProjectPreferences {
    ProjectPreferences {
        mode: CreationMode::Full,
        target_duration_secs: Some(DurationConstraint::exact(duration)),
        included_instruments: Vec::new(),
        excluded_instruments: Vec::new(),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn composer_schema() -> Value {
    json!({
        "type": "object",
        "required": ["title", "tempo_bpm", "meter", "phrase_grid_bars", "parts", "motifs", "sections"],
        "properties": {
            "title": {"type": "string"},
            "tempo_bpm": {"type": "integer", "minimum": 1},
            "meter": {"type": "object", "required": ["numerator", "denominator"], "properties": {
                "numerator": {"type": "integer", "minimum": 1},
                "denominator": {"type": "integer", "minimum": 1}
            }},
            "phrase_grid_bars": {"type": "integer", "minimum": 1},
            "parts": {"type": "array", "items": {"type": "object", "required": ["id", "instrument"], "properties": {"id": {"type": "string"}, "instrument": {"type": "string"}}}},
            "motifs": {"type": "array", "items": {"type": "object", "required": ["id", "material", "role"], "properties": {"id": {"type": "string"}, "material": {"type": "string"}, "role": {"type": "string"}}}},
            "sections": {"type": "array", "items": section_plan_schema()}
        }
    })
}

fn section_plan_schema() -> Value {
    json!({
        "type": "object",
        "required": ["id", "family", "length_weight", "tonal_center", "harmonic_plan", "texture", "material_plan"],
        "properties": {
            "id": {"type": "string"}, "family": {"type": "string", "enum": ["theme", "development"]},
            "length_weight": {"type": "integer", "minimum": 1}, "tonal_center": {"type": "string"},
            "harmonic_plan": {"type": "string"}, "texture": {"type": "string"}, "material_plan": {"type": "string"}
        }
    })
}

fn worker_schema() -> Value {
    json!({
        "type": "object", "required": ["family", "sections"],
        "properties": {
            "family": {"type": "string", "enum": ["theme", "development"]},
            "sections": {"type": "array", "items": {"type": "object", "required": ["section_id", "parts"], "properties": {
                "section_id": {"type": "string"},
                "parts": {"type": "array", "items": {"type": "object", "required": ["part_id", "alda_sequence_body"], "properties": {
                    "part_id": {"type": "string"},
                    "alda_sequence_body": {"type": "string"}
                }}}
            }}}
        }
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object", "required": ["approved", "blocking_findings", "musical_observations", "summary"],
        "properties": {
            "approved": {"type": "boolean"}, "summary": {"type": "string"},
            "musical_observations": {"type": "array", "items": {"type": "string"}},
            "blocking_findings": {"type": "array", "items": {"type": "object", "required": ["family", "section_id", "issue", "evidence"], "properties": {
                "family": {"type": "string", "enum": ["theme", "development"]}, "section_id": {"type": "string"}, "issue": {"type": "string"}, "evidence": {"type": "string"}
            }}}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_directory_must_be_new() {
        let directory = tempfile::tempdir().unwrap();
        let args = Args {
            file: PathBuf::from("task.txt"),
            duration: 300.0,
            output: directory.path().to_path_buf(),
            config_root: PathBuf::from("."),
        };
        assert!(validate_args(&args).is_err());
    }

    #[test]
    fn role_schemas_are_objects() {
        for schema in [composer_schema(), worker_schema(), review_schema()] {
            assert_eq!(schema["type"], "object");
        }
    }

    #[test]
    fn role_schemas_do_not_expose_composer_durations_or_anchors() {
        let composer = composer_schema().to_string();
        let worker = worker_schema().to_string();

        assert!(!composer.contains("duration_beats"));
        assert!(!composer.contains("anchor"));
        assert!(!worker.contains("anchor"));
        assert!(!worker.contains("events"));
        assert!(worker.contains("alda_sequence_body"));
    }
}
