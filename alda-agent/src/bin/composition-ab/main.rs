mod domain;
mod protocol;

use crate::agent::GenerationStats;
use crate::alda::{AldaCheck, AldaRunner, CheckStatus, ScoreInfo};
use crate::audio::AudioRenderer;
use crate::composition::{SectionArtifact, assemble_sections, verify_timeline};
use crate::deepseek::DeepSeekClient;
use crate::instructions::DurationConstraint;
use crate::project::{FormPlan, FormSection, MaterialAction, SectionEnergy};
use anyhow::{Context, Result, bail};
use domain::{
    BudgetCompilation, ComposerPlan, ReviewReport, SectionFamily, WorkerSubmission, validate_review,
};
use protocol::{RoleSession, RoleStats};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct CompositionAbResult {
    _directory: tempfile::TempDir,
    pub alda_code: String,
    pub checks: Vec<AldaCheck>,
    pub form_plan: FormPlan,
    pub summary: String,
    pub stats: GenerationStats,
    midi_path: std::path::PathBuf,
    wav_path: std::path::PathBuf,
}

impl CompositionAbResult {
    #[must_use]
    pub fn midi_path(&self) -> &Path {
        &self.midi_path
    }

    #[must_use]
    pub fn wav_path(&self) -> &Path {
        &self.wav_path
    }
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

#[allow(clippy::too_many_lines)]
pub async fn run(
    task: &str,
    duration: f64,
    included_instruments: &[String],
    excluded_instruments: &[String],
    client: DeepSeekClient,
    alda: AldaRunner,
    renderer: AudioRenderer,
) -> Result<CompositionAbResult> {
    if task.trim().is_empty() {
        bail!("composition-ab 创作要求不能为空");
    }
    if !duration.is_finite() || duration <= 0.0 {
        bail!("composition-ab 目标时长必须是正有限数");
    }
    let directory = tempfile::tempdir()?;
    let output = directory.path();
    let mut composer = composer_session(client.clone(), task, duration);
    let plan = match composer.submit(validate_plan).await {
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
    let _timeline = verify_timeline(&spec, &assembly, &probe_score)?;
    let score_path = output.join("score.alda");
    fs::write(&score_path, &assembly.alda_source)?;
    let score = alda.parse(&score_path)?;
    let checks = alda.validate(
        &score_path,
        included_instruments,
        excluded_instruments,
        Some(DurationConstraint::exact(duration)),
        10.0,
    );
    if checks.iter().any(|check| check.status == CheckStatus::Fail) {
        write_json(&output.join("candidate-checks.json"), &checks)?;
        bail!("composition-ab 完整候选未通过项目 Alda/时长检查");
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
    let midi_path = output.join("score.mid");
    let wav_path = output.join("score.wav");
    renderer
        .render_score_async(
            alda,
            score_path.clone(),
            midi_path.clone(),
            wav_path.clone(),
        )
        .await?;
    if !review.approved {
        bail!("只读 Reviewer 提出了阻断性问题；保留已渲染产物供审计");
    }

    let stats = combined_stats([
        composer.stats(),
        theme.stats,
        development.stats,
        reviewer.stats(),
    ]);
    let form_plan = project_form_plan(&plan, &budget);
    let summary = format!("{}（Reviewer：{}）", plan.title, review.summary);
    Ok(CompositionAbResult {
        _directory: directory,
        alda_code: assembly.alda_source,
        checks,
        form_plan,
        summary,
        stats,
        midi_path,
        wav_path,
    })
}

fn validate_plan(plan: &ComposerPlan) -> Result<()> {
    plan.validate()?;
    if plan.sections.len() != 4 || !(3..=4).contains(&plan.parts.len()) {
        bail!("composition-ab 的 Composer 必须提交恰好 4 个段落和 3–4 个声部");
    }
    for family in [SectionFamily::Theme, SectionFamily::Development] {
        if plan
            .sections
            .iter()
            .filter(|section| section.family == family)
            .count()
            != 2
        {
            bail!("composition-ab 的 theme 与 development 必须各有恰好 2 个段落");
        }
    }
    Ok(())
}

fn combined_stats(stats: [RoleStats; 4]) -> GenerationStats {
    GenerationStats {
        model_calls: stats.iter().map(|value| value.model_calls).sum(),
        tool_turns: 0,
        protocol_recoveries: stats.iter().map(|value| value.protocol_recoveries).sum(),
        submissions: stats.len() + stats.iter().map(|value| value.revisions).sum::<usize>(),
    }
}

fn project_form_plan(plan: &ComposerPlan, budget: &BudgetCompilation) -> FormPlan {
    let target_duration_secs = budget.planned_duration_secs;
    let last_index = plan.sections.len() - 1;
    let sections = plan
        .sections
        .iter()
        .zip(&budget.sections)
        .enumerate()
        .map(|(index, (section, budget))| FormSection {
            id: section.id.clone(),
            target_start_secs: beat_seconds(budget.planned_start_beats, plan.tempo_bpm),
            target_end_secs: beat_seconds(budget.planned_end_beats, plan.tempo_bpm),
            function: format!(
                "{}；{}；{}",
                section.harmonic_plan, section.texture, section.material_plan
            ),
            material_action: if index == 0 {
                MaterialAction::Introduce
            } else if index == last_index {
                MaterialAction::Close
            } else if section.family == SectionFamily::Development {
                MaterialAction::Develop
            } else {
                MaterialAction::Reprise
            },
            energy: match index {
                0 => SectionEnergy::Low,
                1 => SectionEnergy::Medium,
                index if index == last_index => SectionEnergy::Peak,
                _ => SectionEnergy::High,
            },
        })
        .collect();
    FormPlan {
        target_duration_secs,
        sections,
    }
}

fn beat_seconds(beat: crate::composition::Beat, tempo_bpm: u32) -> f64 {
    f64::from(beat.numerator) / f64::from(beat.denominator) * 60.0 / f64::from(tempo_bpm)
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
        .submit(|value: &WorkerSubmission| validate_worker(value, &plan, family))
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
            .submit(|value: &WorkerSubmission| validate_worker(value, &plan, family))
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

fn validate_worker(
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
        bail!("composition-ab 的反复乐句禁止使用会跨反复累积的 < 或 >；请改用绝对 o4/o5/o6")
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
        "你是唯一的 Composer。像音乐家一样设计全曲的速度、拍号、乐句网格、主题、和声、织体、曲式、配器与发展关系；不要写 Alda，也不要手算或提交任何绝对拍数。请恰好使用 4 个段落和 3–4 个声部形成完整曲式，theme 与 development 各分配恰好 2 个段落；仅用正 length_weight 表达段落相对比例。instrument 必须使用 Alda stock instrument 的安全名称，例如 flute、violin、cello 或 piano。调用 submit_composer_plan 时直接提交顶层对象，不要包装在 plan 等字段中；顶层必须完整包含 title、tempo_bpm、meter、phrase_grid_bars、parts、motifs、sections。",
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

fn beat_label(beat: crate::composition::Beat) -> String {
    if beat.denominator == 1 {
        beat.numerator.to_string()
    } else {
        format!("{}/{}", beat.numerator, beat.denominator)
    }
}

fn exact_repeat_count(
    duration: crate::composition::Beat,
    phrase: crate::composition::Beat,
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
