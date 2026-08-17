use crate::alda::{AldaRunner, ScoreInfo};
use anyhow::{Context, Result, bail};
use hound::{SampleFormat, WavReader};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const SILENCE_PEAK_THRESHOLD: f64 = 1.0e-5;
const SILENCE_RMS_THRESHOLD: f64 = 1.0e-6;
const SILENCE_WINDOW_MS: u64 = 100;
const MAX_REPORTED_SILENCE_INTERVALS: usize = 16;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SilenceInterval {
    pub start_ms: f64,
    pub end_ms: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SilenceDiagnostics {
    pub leading_silence_ms: f64,
    pub trailing_silence_ms: f64,
    pub max_internal_silence_ms: f64,
    pub silent_ratio: f64,
    pub intervals: Vec<SilenceInterval>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WavInfo {
    pub duration_secs: f64,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub peak: f64,
    pub rms: f64,
    pub silent: bool,
    pub silence: SilenceDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactReport {
    pub alda_path: PathBuf,
    pub midi_path: PathBuf,
    pub wav_path: PathBuf,
    pub parsed_duration_secs: f64,
    pub part_count: usize,
    pub event_count: usize,
    pub instruments: Vec<String>,
    pub wav: WavInfo,
}

#[derive(Debug, Clone)]
pub struct AudioRenderer {
    fluidsynth_path: PathBuf,
    soundfont_path: PathBuf,
}

impl AudioRenderer {
    pub fn discover() -> Result<Self> {
        let fluidsynth_path = find_program("fluidsynth")
            .ok_or_else(|| anyhow::anyhow!("未找到 fluidsynth；请运行 scripts/install-linux.sh"))?;
        let soundfont_path = find_soundfont().ok_or_else(|| {
            anyhow::anyhow!(
                "未找到 General MIDI SoundFont；请安装 fluid-soundfont-gm，或设置 ALDA_AGENT_SOUNDFONT"
            )
        })?;
        Ok(Self {
            fluidsynth_path,
            soundfont_path,
        })
    }

    #[must_use]
    pub fn new(fluidsynth_path: PathBuf, soundfont_path: PathBuf) -> Self {
        Self {
            fluidsynth_path,
            soundfont_path,
        }
    }

    #[must_use]
    pub fn fluidsynth_path(&self) -> &Path {
        &self.fluidsynth_path
    }

    #[must_use]
    pub fn soundfont_path(&self) -> &Path {
        &self.soundfont_path
    }

    pub fn render_midi(&self, midi_path: &Path, wav_path: &Path) -> Result<WavInfo> {
        if !midi_path.is_file() {
            bail!("MIDI 文件不存在: {}", midi_path.display());
        }
        if let Some(parent) = wav_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("无法创建目录 {}", parent.display()))?;
        }
        let output = Command::new(&self.fluidsynth_path)
            .args(["-q", "-ni", "-F"])
            .arg(wav_path)
            .args(["-r", &DEFAULT_SAMPLE_RATE.to_string()])
            .arg(&self.soundfont_path)
            .arg(midi_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("无法启动 {}", self.fluidsynth_path.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("FluidSynth 渲染失败: {}", stderr.trim());
        }
        let info = inspect_wav(wav_path)?;
        if info.frames == 0 || info.duration_secs <= 0.0 {
            bail!("WAV 没有有效音频帧: {}", wav_path.display());
        }
        if info.silent {
            bail!(
                "WAV 被判定为静音（peak={:.6}）: {}",
                info.peak,
                wav_path.display()
            );
        }
        Ok(info)
    }

    pub fn render_score(
        &self,
        alda: &AldaRunner,
        score_path: &Path,
        midi_path: &Path,
        wav_path: &Path,
    ) -> Result<ArtifactReport> {
        if let Some(parent) = midi_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("无法创建目录 {}", parent.display()))?;
        }
        let score = alda.parse(score_path)?;
        alda.export_midi(score_path, midi_path)?;
        let wav = self.render_midi(midi_path, wav_path)?;
        Ok(build_report(score_path, midi_path, wav_path, score, wav))
    }

    pub async fn render_score_async(
        &self,
        alda: AldaRunner,
        score_path: PathBuf,
        midi_path: PathBuf,
        wav_path: PathBuf,
    ) -> Result<ArtifactReport> {
        let renderer = self.clone();
        tokio::task::spawn_blocking(move || {
            renderer.render_score(&alda, &score_path, &midi_path, &wav_path)
        })
        .await
        .context("等待音频渲染任务失败")?
    }
}

fn build_report(
    score_path: &Path,
    midi_path: &Path,
    wav_path: &Path,
    score: ScoreInfo,
    wav: WavInfo,
) -> ArtifactReport {
    ArtifactReport {
        alda_path: score_path.to_path_buf(),
        midi_path: midi_path.to_path_buf(),
        wav_path: wav_path.to_path_buf(),
        parsed_duration_secs: score.duration_ms / 1000.0,
        part_count: score.part_count,
        event_count: score.event_count,
        instruments: score.instruments,
        wav,
    }
}

#[allow(clippy::cast_precision_loss)]
pub fn inspect_wav(path: &Path) -> Result<WavInfo> {
    let mut reader =
        WavReader::open(path).with_context(|| format!("无法读取 WAV {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        bail!("WAV 头无效: {}", path.display());
    }

    let mut samples = SampleAccumulator::new(spec.sample_rate, spec.channels);
    match spec.sample_format {
        SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                samples.push(f64::from(sample?));
            }
        }
        SampleFormat::Int => {
            let scale = 2_f64.powi(i32::from(spec.bits_per_sample.saturating_sub(1)));
            for sample in reader.samples::<i32>() {
                samples.push(f64::from(sample?) / scale);
            }
        }
    }
    let analysis = samples.finish();
    Ok(WavInfo {
        duration_secs: analysis.frames as f64 / f64::from(spec.sample_rate),
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames: analysis.frames,
        peak: analysis.peak,
        rms: analysis.rms,
        silent: analysis.sample_count == 0 || analysis.peak <= SILENCE_PEAK_THRESHOLD,
        silence: build_silence_diagnostics(
            &analysis.silent_windows,
            analysis.frames,
            spec.sample_rate,
        ),
    })
}

#[derive(Debug)]
struct SampleAccumulator {
    channels: u64,
    samples_per_window: u64,
    sample_count: u64,
    peak: f64,
    squares: f64,
    window_sample_count: u64,
    window_peak: f64,
    window_squares: f64,
    window_start_frame: u64,
    silent_windows: Vec<(u64, u64)>,
}

#[derive(Debug)]
struct SampleAnalysis {
    sample_count: u64,
    frames: u64,
    peak: f64,
    rms: f64,
    silent_windows: Vec<(u64, u64)>,
}

#[allow(clippy::cast_precision_loss)]
impl SampleAccumulator {
    fn new(sample_rate: u32, channels: u16) -> Self {
        let frames_per_window = (u64::from(sample_rate) * SILENCE_WINDOW_MS / 1_000).max(1);
        let channels = u64::from(channels);
        Self {
            channels,
            samples_per_window: frames_per_window * channels,
            sample_count: 0,
            peak: 0.0,
            squares: 0.0,
            window_sample_count: 0,
            window_peak: 0.0,
            window_squares: 0.0,
            window_start_frame: 0,
            silent_windows: Vec::new(),
        }
    }

    fn push(&mut self, value: f64) {
        let value = value.clamp(-1.0, 1.0);
        self.sample_count += 1;
        self.peak = self.peak.max(value.abs());
        self.squares += value * value;
        self.window_sample_count += 1;
        self.window_peak = self.window_peak.max(value.abs());
        self.window_squares += value * value;
        if self.window_sample_count == self.samples_per_window {
            self.finish_window(self.sample_count / self.channels);
        }
    }

    fn finish(mut self) -> SampleAnalysis {
        let frames = self.sample_count / self.channels;
        if self.window_sample_count > 0 {
            self.finish_window(frames);
        }
        let rms = if self.sample_count == 0 {
            0.0
        } else {
            (self.squares / self.sample_count as f64).sqrt()
        };
        SampleAnalysis {
            sample_count: self.sample_count,
            frames,
            peak: self.peak,
            rms,
            silent_windows: self.silent_windows,
        }
    }

    fn finish_window(&mut self, end_frame: u64) {
        let rms = (self.window_squares / self.window_sample_count as f64).sqrt();
        if self.window_peak <= SILENCE_PEAK_THRESHOLD && rms <= SILENCE_RMS_THRESHOLD {
            self.silent_windows
                .push((self.window_start_frame, end_frame));
        }
        self.window_start_frame = end_frame;
        self.window_sample_count = 0;
        self.window_peak = 0.0;
        self.window_squares = 0.0;
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_silence_diagnostics(
    silent_windows: &[(u64, u64)],
    total_frames: u64,
    sample_rate: u32,
) -> SilenceDiagnostics {
    let mut merged = Vec::<(u64, u64)>::new();
    for &(start, end) in silent_windows {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }

    let leading_frames = merged
        .first()
        .filter(|(start, _)| *start == 0)
        .map_or(0, |(_, end)| *end);
    let trailing_frames = merged
        .last()
        .filter(|(_, end)| *end == total_frames)
        .map_or(0, |(start, _)| total_frames - *start);
    let max_internal_frames = merged
        .iter()
        .filter(|(start, end)| *start > 0 && *end < total_frames)
        .map(|(start, end)| end - start)
        .max()
        .unwrap_or(0);
    let silent_frames = merged.iter().map(|(start, end)| end - start).sum::<u64>();

    let mut reported = merged.clone();
    if reported.len() > MAX_REPORTED_SILENCE_INTERVALS {
        reported.sort_unstable_by_key(|(start, end)| std::cmp::Reverse(end - start));
        reported.truncate(MAX_REPORTED_SILENCE_INTERVALS);
        reported.sort_unstable_by_key(|(start, _)| *start);
    }

    let frames_to_ms = |frames: u64| frames as f64 * 1_000.0 / f64::from(sample_rate);
    SilenceDiagnostics {
        leading_silence_ms: frames_to_ms(leading_frames),
        trailing_silence_ms: frames_to_ms(trailing_frames),
        max_internal_silence_ms: frames_to_ms(max_internal_frames),
        silent_ratio: if total_frames == 0 {
            0.0
        } else {
            silent_frames as f64 / total_frames as f64
        },
        intervals: reported
            .into_iter()
            .map(|(start, end)| SilenceInterval {
                start_ms: frames_to_ms(start),
                end_ms: frames_to_ms(end),
            })
            .collect(),
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

#[must_use]
pub fn find_soundfont() -> Option<PathBuf> {
    let configured = std::env::var_os("ALDA_AGENT_SOUNDFONT").map(PathBuf::from);
    configured
        .into_iter()
        .chain(
            [
                "/usr/share/soundfonts/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/FluidR3_GM.sf2",
                "/usr/share/soundfonts/default.sf2",
                "/usr/local/share/soundfonts/FluidR3_GM.sf2",
            ]
            .into_iter()
            .map(PathBuf::from),
        )
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};

    const TEST_SAMPLE_RATE: u32 = 8_000;

    fn assert_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn inspects_non_silent_pcm_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: TEST_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for index in 0..16_000 {
            let value = if index % 2 == 0 {
                8_000_i16
            } else {
                -8_000_i16
            };
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();

        let info = inspect_wav(&path).unwrap();
        assert!((info.duration_secs - 1.0).abs() < 0.001);
        assert_eq!(info.frames, 8_000);
        assert!(!info.silent);
        assert!(info.peak > 0.2);
        assert!(info.rms > 0.2);
        assert_eq!(info.silence.intervals, Vec::new());
        assert_near(info.silence.silent_ratio, 0.0);
    }

    #[test]
    fn identifies_silent_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: TEST_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for _ in 0..8_000 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let info = inspect_wav(&path).unwrap();
        assert!(info.silent);
        assert_near(info.silence.leading_silence_ms, 1_000.0);
        assert_near(info.silence.trailing_silence_ms, 1_000.0);
        assert_near(info.silence.max_internal_silence_ms, 0.0);
        assert_near(info.silence.silent_ratio, 1.0);
        assert_eq!(
            info.silence.intervals,
            vec![SilenceInterval {
                start_ms: 0.0,
                end_ms: 1_000.0
            }]
        );
    }

    #[test]
    fn reports_leading_internal_and_trailing_silence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gaps.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: TEST_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for &(windows, sample) in &[
            (2, 0_i16),
            (1, 8_000_i16),
            (3, 0_i16),
            (1, -8_000_i16),
            (2, 0_i16),
        ] {
            for _ in 0..windows * 800 {
                writer.write_sample(sample).unwrap();
            }
        }
        writer.finalize().unwrap();

        let info = inspect_wav(&path).unwrap();
        assert!(!info.silent);
        assert_near(info.duration_secs, 0.9);
        assert_near(info.silence.leading_silence_ms, 200.0);
        assert_near(info.silence.trailing_silence_ms, 200.0);
        assert_near(info.silence.max_internal_silence_ms, 300.0);
        assert_near(info.silence.silent_ratio, 7.0 / 9.0);
        assert_eq!(
            info.silence.intervals,
            vec![
                SilenceInterval {
                    start_ms: 0.0,
                    end_ms: 200.0,
                },
                SilenceInterval {
                    start_ms: 300.0,
                    end_ms: 600.0,
                },
                SilenceInterval {
                    start_ms: 700.0,
                    end_ms: 900.0,
                },
            ]
        );
    }

    #[test]
    fn analyzes_stereo_by_frames_and_keeps_a_partial_final_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo-partial.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: TEST_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for frame in 0..2_000 {
            let left = if frame < 1_600 { 8_000_i16 } else { 0_i16 };
            writer.write_sample(left).unwrap();
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();

        let info = inspect_wav(&path).unwrap();
        assert_eq!(info.frames, 2_000);
        assert_near(info.duration_secs, 0.25);
        assert_near(info.silence.leading_silence_ms, 0.0);
        assert_near(info.silence.trailing_silence_ms, 50.0);
        assert_near(info.silence.silent_ratio, 0.2);
        assert_eq!(
            info.silence.intervals,
            vec![SilenceInterval {
                start_ms: 200.0,
                end_ms: 250.0,
            }]
        );
    }

    #[test]
    fn analyzes_float_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("float.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: TEST_SAMPLE_RATE,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for _ in 0..800 {
            writer.write_sample(0.0_f32).unwrap();
        }
        for _ in 0..800 {
            writer.write_sample(0.25_f32).unwrap();
        }
        writer.finalize().unwrap();

        let info = inspect_wav(&path).unwrap();
        assert!(!info.silent);
        assert_near(info.peak, 0.25);
        assert_near(info.silence.leading_silence_ms, 100.0);
        assert_near(info.silence.silent_ratio, 0.5);
    }
}
