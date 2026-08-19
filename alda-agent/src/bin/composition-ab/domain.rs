use crate::composition::{Beat, CompositionSpec, PartSpec, SectionArtifact, SectionContract};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionFamily {
    Theme,
    Development,
}

impl SectionFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Development => "development",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meter {
    pub numerator: u32,
    pub denominator: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposerPlan {
    pub title: String,
    pub tempo_bpm: u32,
    pub meter: Meter,
    pub phrase_grid_bars: u32,
    pub parts: Vec<PartSpec>,
    pub motifs: Vec<MotifPlan>,
    pub sections: Vec<MusicalSectionPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotifPlan {
    pub id: String,
    pub material: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicalSectionPlan {
    pub id: String,
    pub family: SectionFamily,
    pub length_weight: u32,
    pub tonal_center: String,
    pub harmonic_plan: String,
    pub texture: String,
    pub material_plan: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetCompilation {
    pub target_duration_secs: f64,
    pub planned_duration_secs: f64,
    pub duration_difference_secs: f64,
    pub grid_beats: Beat,
    pub total_grids: u32,
    pub sections: Vec<SectionBudget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionBudget {
    pub section_id: String,
    pub duration_beats: Beat,
    pub planned_start_beats: Beat,
    pub planned_end_beats: Beat,
}

impl ComposerPlan {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.tempo_bpm == 0 {
            bail!("标题不能为空且 tempo_bpm 必须大于 0");
        }
        if self.meter.numerator == 0
            || self.meter.denominator == 0
            || !self.meter.denominator.is_power_of_two()
        {
            bail!("拍号 numerator 必须大于 0，denominator 必须是正的 2 次幂");
        }
        if self.phrase_grid_bars == 0 {
            bail!("phrase_grid_bars 必须大于 0");
        }
        if !(2..=8).contains(&self.parts.len()) {
            bail!("计划必须包含 2–8 个声部");
        }
        if !(3..=10).contains(&self.sections.len()) {
            bail!("计划必须包含 3–10 个段落");
        }

        let mut part_ids = BTreeSet::new();
        for part in &self.parts {
            validate_id(&part.id)?;
            validate_instrument(&part.instrument)?;
            if !part_ids.insert(part.id.as_str()) {
                bail!("声部必须具有唯一 ID");
            }
        }

        if self.motifs.is_empty() {
            bail!("计划必须至少包含一个动机");
        }
        let mut motif_ids = BTreeSet::new();
        for motif in &self.motifs {
            validate_id(&motif.id)?;
            if !motif_ids.insert(motif.id.as_str())
                || motif.material.trim().is_empty()
                || motif.role.trim().is_empty()
            {
                bail!("动机必须具有唯一 ID、非空 material 和非空 role");
            }
        }

        let mut section_ids = BTreeSet::new();
        let mut families = BTreeSet::new();
        for section in &self.sections {
            validate_id(&section.id)?;
            if !section_ids.insert(section.id.as_str()) {
                bail!("段落 ID 重复：{}", section.id);
            }
            if section.length_weight == 0 {
                bail!("段落 {} 的 length_weight 必须大于 0", section.id);
            }
            if [
                &section.tonal_center,
                &section.harmonic_plan,
                &section.texture,
                &section.material_plan,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
            {
                bail!("段落 {} 的音乐计划字段不能为空", section.id);
            }
            families.insert(section.family);
        }
        if families.len() != 2 {
            bail!("计划必须同时包含 theme 和 development 段落家族");
        }
        Ok(())
    }

    pub fn compile_budget(&self, target_duration_secs: f64) -> Result<BudgetCompilation> {
        self.validate()?;
        if !target_duration_secs.is_finite() || target_duration_secs <= 0.0 {
            bail!("目标时长必须是正有限数");
        }

        let grid_numerator = u64::from(self.meter.numerator)
            .checked_mul(4)
            .and_then(|value| value.checked_mul(u64::from(self.phrase_grid_bars)))
            .context("乐句网格拍数溢出")?;
        let grid_denominator = u64::from(self.meter.denominator);
        let (grid_numerator, grid_denominator) = reduce(grid_numerator, grid_denominator);
        let grid_beats = beat(grid_numerator, grid_denominator)?;
        let grid_beats_value = f64::from(grid_beats.numerator) / f64::from(grid_beats.denominator);
        let ideal_grids =
            target_duration_secs * f64::from(self.tempo_bpm) / 60.0 / grid_beats_value;
        if !ideal_grids.is_finite() || ideal_grids > f64::from(u32::MAX) {
            bail!("目标时长对应的乐句网格数超出支持范围");
        }
        let lower = ideal_grids.floor();
        let nearest = if ideal_grids - lower > 0.5 {
            lower + 1.0
        } else {
            lower
        };
        let minimum = u32::try_from(self.sections.len()).context("段落数超出支持范围")?;
        // The finite, positive and u32 upper-bound checks above make this cast exact in range.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let total_grids = (nearest as u32).max(minimum);

        let remaining = total_grids - minimum;
        let weight_sum = self.sections.iter().try_fold(0_u64, |sum, section| {
            sum.checked_add(u64::from(section.length_weight))
                .context("段落权重之和溢出")
        })?;
        let mut grid_counts = Vec::with_capacity(self.sections.len());
        let mut remainders = Vec::with_capacity(self.sections.len());
        let mut allocated = 0_u32;
        for (index, section) in self.sections.iter().enumerate() {
            let weighted = u64::from(remaining)
                .checked_mul(u64::from(section.length_weight))
                .context("预算权重乘积溢出")?;
            let quotient = u32::try_from(weighted / weight_sum).context("预算网格数溢出")?;
            allocated = allocated.checked_add(quotient).context("预算网格数溢出")?;
            grid_counts.push(1 + quotient);
            remainders.push((weighted % weight_sum, index));
        }
        remainders.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        for &(_, index) in remainders.iter().take((remaining - allocated) as usize) {
            grid_counts[index] += 1;
        }

        let mut start_numerator = 0_u64;
        let mut sections = Vec::with_capacity(self.sections.len());
        for (section, grids) in self.sections.iter().zip(grid_counts) {
            let duration_numerator = grid_numerator
                .checked_mul(u64::from(grids))
                .context("段落预算拍数溢出")?;
            let end_numerator = start_numerator
                .checked_add(duration_numerator)
                .context("计划拍位溢出")?;
            sections.push(SectionBudget {
                section_id: section.id.clone(),
                duration_beats: beat(duration_numerator, grid_denominator)?,
                planned_start_beats: beat(start_numerator, grid_denominator)?,
                planned_end_beats: beat(end_numerator, grid_denominator)?,
            });
            start_numerator = end_numerator;
        }
        let planned_duration_secs =
            f64::from(total_grids) * grid_beats_value * 60.0 / f64::from(self.tempo_bpm);
        Ok(BudgetCompilation {
            target_duration_secs,
            planned_duration_secs,
            duration_difference_secs: planned_duration_secs - target_duration_secs,
            grid_beats,
            total_grids,
            sections,
        })
    }

    pub fn composition_spec(&self, budget: &BudgetCompilation) -> Result<CompositionSpec> {
        if budget.sections.len() != self.sections.len() {
            bail!("预算段落数与 Composer 计划不一致");
        }
        let sections = self
            .sections
            .iter()
            .zip(&budget.sections)
            .map(|(planned, budget)| {
                if planned.id != budget.section_id {
                    bail!("预算段落顺序与 Composer 计划不一致");
                }
                Ok(SectionContract {
                    id: planned.id.clone(),
                    duration_beats: budget.duration_beats,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CompositionSpec {
            tempo_bpm: self.tempo_bpm,
            parts: self.parts.clone(),
            sections,
        })
    }

    pub fn family_spec(
        &self,
        budget: &BudgetCompilation,
        family: SectionFamily,
    ) -> Result<CompositionSpec> {
        let mut spec = self.composition_spec(budget)?;
        spec.sections.retain(|section| {
            self.sections
                .iter()
                .any(|planned| planned.id == section.id && planned.family == family)
        });
        Ok(spec)
    }

    pub fn family_budgets<'a>(
        &'a self,
        budget: &'a BudgetCompilation,
        family: SectionFamily,
    ) -> Vec<&'a SectionBudget> {
        self.sections
            .iter()
            .zip(&budget.sections)
            .filter_map(|(section, budget)| (section.family == family).then_some(budget))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSubmission {
    pub family: SectionFamily,
    pub sections: Vec<SectionArtifact>,
}

impl WorkerSubmission {
    pub fn validate_contract(&self, plan: &ComposerPlan, family: SectionFamily) -> Result<()> {
        if self.family != family {
            bail!("Worker 返回了错误的段落家族");
        }
        let expected_sections = plan
            .sections
            .iter()
            .filter(|section| section.family == family)
            .map(|section| section.id.as_str())
            .collect::<BTreeSet<_>>();
        let actual_sections = self
            .sections
            .iter()
            .map(|section| section.section_id.as_str())
            .collect::<BTreeSet<_>>();
        if expected_sections != actual_sections || actual_sections.len() != self.sections.len() {
            bail!("Worker 必须且只能提交被分配家族的每个段落一次");
        }
        let expected_parts = plan
            .parts
            .iter()
            .map(|part| part.id.as_str())
            .collect::<BTreeSet<_>>();
        for section in &self.sections {
            let actual_parts = section
                .parts
                .iter()
                .map(|part| part.part_id.as_str())
                .collect::<BTreeSet<_>>();
            if expected_parts != actual_parts || actual_parts.len() != section.parts.len() {
                bail!(
                    "段落 {} 必须且只能提交计划中的全部声部一次",
                    section.section_id
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub approved: bool,
    #[serde(default)]
    pub blocking_findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub musical_observations: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub family: SectionFamily,
    pub section_id: String,
    pub issue: String,
    pub evidence: String,
}

pub fn validate_review(report: &ReviewReport, plan: &ComposerPlan) -> Result<()> {
    if report.summary.trim().is_empty() {
        bail!("Reviewer summary 不能为空");
    }
    if report.approved != report.blocking_findings.is_empty() {
        bail!("approved 必须与 blocking_findings 是否为空一致");
    }
    for finding in &report.blocking_findings {
        let valid = plan
            .sections
            .iter()
            .any(|section| section.id == finding.section_id && section.family == finding.family);
        if !valid || finding.issue.trim().is_empty() || finding.evidence.trim().is_empty() {
            bail!("Reviewer finding 必须引用真实段落并包含问题与证据");
        }
    }
    Ok(())
}

fn beat(numerator: u64, denominator: u64) -> Result<Beat> {
    let (numerator, denominator) = reduce(numerator, denominator);
    Ok(Beat::new(
        u32::try_from(numerator).context("拍数分子超出支持范围")?,
        u32::try_from(denominator).context("拍数分母超出支持范围")?,
    ))
}

const fn reduce(numerator: u64, denominator: u64) -> (u64, u64) {
    let divisor = gcd(numerator, denominator);
    (numerator / divisor, denominator / divisor)
}

const fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 { 1 } else { left }
}

fn validate_id(id: &str) -> Result<()> {
    let mut bytes = id.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("ID 必须匹配 [a-z][a-z0-9_]*，实际为 {id:?}");
    }
    Ok(())
}

fn validate_instrument(instrument: &str) -> Result<()> {
    let mut bytes = instrument.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("Alda stock instrument 必须是安全 token，实际为 {instrument:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(weights: &[u32]) -> ComposerPlan {
        ComposerPlan {
            title: "test".to_string(),
            tempo_bpm: 120,
            meter: Meter {
                numerator: 4,
                denominator: 4,
            },
            phrase_grid_bars: 1,
            parts: vec![
                PartSpec {
                    id: "lead".to_string(),
                    instrument: "flute".to_string(),
                },
                PartSpec {
                    id: "bass".to_string(),
                    instrument: "cello".to_string(),
                },
            ],
            motifs: vec![MotifPlan {
                id: "motive".to_string(),
                material: "short rising cell".to_string(),
                role: "primary".to_string(),
            }],
            sections: weights
                .iter()
                .enumerate()
                .map(|(index, weight)| MusicalSectionPlan {
                    id: format!("s{index}"),
                    family: if index == 1 {
                        SectionFamily::Development
                    } else {
                        SectionFamily::Theme
                    },
                    length_weight: *weight,
                    tonal_center: "C".to_string(),
                    harmonic_plan: "functional".to_string(),
                    texture: "two voices".to_string(),
                    material_plan: "develop motive".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn budget_uses_nearest_grid_and_breaks_half_downward() {
        let plan = plan(&[1, 1, 1]);
        let budget = plan.compile_budget(11.0).unwrap();

        // One grid is 4 beats = 2 seconds. 11 seconds is exactly 5.5 grids.
        assert_eq!(budget.total_grids, 5);
        assert!((budget.planned_duration_secs - 10.0).abs() < f64::EPSILON);
        assert!((budget.duration_difference_secs + 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_guarantees_one_grid_then_uses_largest_remainder_stably() {
        let plan = plan(&[1, 1, 1]);
        let budget = plan.compile_budget(16.0).unwrap();
        let durations = budget
            .sections
            .iter()
            .map(|section| section.duration_beats)
            .collect::<Vec<_>>();

        // Eight grids: one each, then five distributed 2, 2, 1 by original order.
        assert_eq!(
            durations,
            vec![Beat::whole(12), Beat::whole(12), Beat::whole(8)]
        );
        assert_eq!(budget.sections[2].planned_end_beats, Beat::whole(32));
    }

    #[test]
    fn budget_handles_fractional_quarter_note_beats() {
        let mut plan = plan(&[1, 2, 1]);
        plan.meter = Meter {
            numerator: 6,
            denominator: 8,
        };
        plan.phrase_grid_bars = 2;
        let budget = plan.compile_budget(18.0).unwrap();

        assert_eq!(budget.grid_beats, Beat::whole(6));
        assert_eq!(budget.total_grids, 6);
        assert_eq!(budget.sections[0].duration_beats, Beat::whole(12));
        assert_eq!(budget.sections[1].duration_beats, Beat::whole(12));
        assert_eq!(budget.sections[2].duration_beats, Beat::whole(12));
    }

    #[test]
    fn budget_is_clamped_to_one_grid_per_section() {
        let budget = plan(&[1, 2, 3]).compile_budget(1.0).unwrap();

        assert_eq!(budget.total_grids, 3);
        assert!(
            budget
                .sections
                .iter()
                .all(|section| section.duration_beats == Beat::whole(4))
        );
    }

    #[test]
    fn composer_rejects_unsafe_instrument_and_nonstandard_meter_denominator() {
        let mut composer = plan(&[1, 1, 1]);
        composer.parts[0].instrument = "flute:\nviolin".to_string();
        assert!(
            composer
                .validate()
                .unwrap_err()
                .to_string()
                .contains("安全 token")
        );

        let mut composer = plan(&[1, 1, 1]);
        composer.meter.denominator = 3;
        assert!(
            composer
                .validate()
                .unwrap_err()
                .to_string()
                .contains("2 次幂")
        );
    }
}
