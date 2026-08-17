use crate::alda::{AldaRunner, ScoreInfo};
use anyhow::{Context, Result, bail};
use hound::{SampleFormat, WavReader};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const SILENCE_PEAK_THRESHOLD: f64 = 1.0e-5;

#[derive(Debug, Clone, Serialize)]
pub struct WavInfo {
    pub duration_secs: f64,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub peak: f64,
    pub rms: f64,
    pub silent: bool,
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

    let mut count = 0_u64;
    let mut peak = 0.0_f64;
    let mut squares = 0.0_f64;
    match spec.sample_format {
        SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                accumulate(f64::from(sample?), &mut count, &mut peak, &mut squares);
            }
        }
        SampleFormat::Int => {
            let scale = 2_f64.powi(i32::from(spec.bits_per_sample.saturating_sub(1)));
            for sample in reader.samples::<i32>() {
                accumulate(
                    f64::from(sample?) / scale,
                    &mut count,
                    &mut peak,
                    &mut squares,
                );
            }
        }
    }
    let frames = count / u64::from(spec.channels);
    let rms = if count == 0 {
        0.0
    } else {
        (squares / count as f64).sqrt()
    };
    Ok(WavInfo {
        duration_secs: frames as f64 / f64::from(spec.sample_rate),
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames,
        peak,
        rms,
        silent: count == 0 || peak <= SILENCE_PEAK_THRESHOLD,
    })
}

fn accumulate(value: f64, count: &mut u64, peak: &mut f64, squares: &mut f64) {
    let value = value.clamp(-1.0, 1.0);
    *count += 1;
    *peak = peak.max(value.abs());
    *squares += value * value;
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

    #[test]
    fn inspects_non_silent_pcm_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 8_000,
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
    }

    #[test]
    fn identifies_silent_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for _ in 0..8_000 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        assert!(inspect_wav(&path).unwrap().silent);
    }
}
