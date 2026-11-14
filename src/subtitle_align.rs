//! Cancellable, local speech-recognition subtitle alignment.
//!
//! Audio samples are decoded with FFmpeg and transcribed by whisper.cpp. The
//! recognizer never receives a URL and the original subtitle is never edited.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
const SAMPLE_SECONDS: f64 = 35.0;
const SAMPLE_COUNT: usize = 6;

#[derive(Debug, Clone)]
pub struct Request {
    pub video: PathBuf,
    pub audio_stream: u64,
    pub subtitle: SubtitleInput,
    pub model: PathBuf,
    pub duration_secs: f64,
    pub cache_dir: PathBuf,
    pub language: String,
}

#[derive(Debug, Clone)]
pub enum SubtitleInput {
    External(PathBuf),
    Embedded { stream: u64 },
}

#[derive(Debug, Clone)]
pub enum Event {
    Progress {
        stage: String,
        done: usize,
        total: usize,
    },
    Ready(Preview),
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Preview {
    pub path: PathBuf,
    pub accepted_path: PathBuf,
    pub offset_secs: f64,
    pub scale: f64,
    pub matches: usize,
    pub confidence: f64,
}

pub struct Worker {
    cancel: Arc<AtomicBool>,
    pub events: mpsc::Receiver<Event>,
}

impl Worker {
    pub fn start(request: Request) -> Result<Self, String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, events) = mpsc::channel();
        thread::Builder::new()
            .name("wim-subtitle-align".into())
            .spawn(move || {
                let work = WorkDir::create(&request.cache_dir);
                let mut work = match work {
                    Ok(work) => work,
                    Err(error) => {
                        let _ = tx.send(Event::Failed(match error {
                            WorkError::Failed(message) => message,
                            WorkError::Cancelled => "alignment cancelled".into(),
                        }));
                        return;
                    }
                };
                let result = run(request, &work, &worker_cancel, &tx);
                match result {
                    Ok(preview) => {
                        work.keep = true;
                        if tx.send(Event::Ready(preview.clone())).is_err() {
                            discard_preview(&preview);
                        }
                    }
                    Err(WorkError::Cancelled) => {
                        let _ = tx.send(Event::Cancelled);
                    }
                    Err(WorkError::Failed(message)) => {
                        let _ = tx.send(Event::Failed(message));
                    }
                }
            })
            .map_err(|error| format!("could not start alignment worker: {}", error.kind()))?;
        Ok(Self { cancel, events })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug)]
enum WorkError {
    Cancelled,
    Failed(String),
}

impl From<std::io::Error> for WorkError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(format!("subtitle alignment I/O failed: {}", error.kind()))
    }
}

struct WorkDir {
    path: PathBuf,
    keep: bool,
}

impl WorkDir {
    fn create(cache: &Path) -> Result<Self, WorkError> {
        let root = cache.join("wim").join("subtitle-align");
        fs::create_dir_all(&root)?;
        for _ in 0..8 {
            let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, keep: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(WorkError::Failed(
            "could not allocate subtitle work directory".into(),
        ))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn accept_preview(preview: &Preview) -> Result<PathBuf, String> {
    let parent = preview
        .accepted_path
        .parent()
        .ok_or_else(|| "invalid subtitle cache destination".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create subtitle cache: {}", error.kind()))?;
    let temp = preview.accepted_path.with_extension("srt.pending");
    fs::copy(&preview.path, &temp)
        .map_err(|error| format!("cannot preserve aligned subtitle: {}", error.kind()))?;
    fs::rename(&temp, &preview.accepted_path)
        .map_err(|error| format!("cannot install aligned subtitle: {}", error.kind()))?;
    Ok(preview.accepted_path.clone())
}

pub fn discard_preview(preview: &Preview) {
    if let Some(parent) = preview.path.parent() {
        let _ = fs::remove_dir_all(parent);
    }
}

fn run(
    request: Request,
    work: &WorkDir,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<Event>,
) -> Result<Preview, WorkError> {
    if !request.video.is_file() {
        return Err(WorkError::Failed(
            "speech alignment currently requires a local video file".into(),
        ));
    }
    if !request.model.is_file() {
        return Err(WorkError::Failed(
            "Whisper model file is missing; set whisper_model_file in config".into(),
        ));
    }
    if !request.duration_secs.is_finite() || request.duration_secs < 2.0 {
        return Err(WorkError::Failed("video duration is unavailable".into()));
    }
    check_cancel(cancel)?;

    let subtitle_path = match &request.subtitle {
        SubtitleInput::External(path) => path.clone(),
        SubtitleInput::Embedded { stream } => {
            let extracted = work.path.join("source.srt");
            let mut status = Command::new("ffmpeg");
            status
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-protocol_whitelist",
                    "file,pipe",
                    "-i",
                ])
                .arg(&request.video)
                .arg("-map")
                .arg(format!("0:{stream}"))
                .args(["-c:s", "srt"])
                .arg(&extracted)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            run_command(&mut status, cancel, "extracting selected subtitle track")?;
            extracted
        }
    };
    let input_srt = if subtitle_path.extension().and_then(|s| s.to_str()) == Some("srt") {
        fs::read_to_string(&subtitle_path).map_err(|error| {
            WorkError::Failed(format!("cannot read selected subtitle: {}", error.kind()))
        })?
    } else {
        let converted = work.path.join("converted.srt");
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-v",
                "error",
                "-y",
                "-protocol_whitelist",
                "file,pipe",
                "-i",
            ])
            .arg(&subtitle_path)
            .args(["-f", "srt"])
            .arg(&converted)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_command(&mut command, cancel, "converting subtitle format")?;
        fs::read_to_string(converted).map_err(|error| {
            WorkError::Failed(format!("cannot read selected subtitle: {}", error.kind()))
        })?
    };
    let original = parse_srt(&input_srt).ok_or_else(|| {
        WorkError::Failed("selected subtitle is not a readable text subtitle".into())
    })?;
    if original.len() < 4 {
        return Err(WorkError::Failed(
            "selected subtitle has too few text cues to align reliably".into(),
        ));
    }

    let spans = sample_spans(request.duration_secs);
    let mut recognized = Vec::new();
    for (index, (start, duration)) in spans.iter().copied().enumerate() {
        check_cancel(cancel)?;
        let _ = tx.send(Event::Progress {
            stage: "ASR sample".into(),
            done: index,
            total: spans.len(),
        });
        let wav = work.path.join(format!("sample-{index}.wav"));
        let mut extract = Command::new("ffmpeg");
        extract
            .args([
                "-v",
                "error",
                "-y",
                "-protocol_whitelist",
                "file,pipe",
                "-ss",
            ])
            .arg(format!("{start:.3}"))
            .arg("-i")
            .arg(&request.video)
            .arg("-map")
            .arg(format!("0:{}", request.audio_stream))
            .arg("-t")
            .arg(format!("{duration:.3}"))
            .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "wav"])
            .arg(&wav)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_command(&mut extract, cancel, "extracting video audio")?;

        let output_prefix = work.path.join(format!("recognized-{index}"));
        let mut recognize = Command::new("whisper-cli");
        recognize
            .arg("-m")
            .arg(&request.model)
            .arg("-f")
            .arg(&wav)
            .args(["-osrt", "-np", "-sns", "-nf", "-l"])
            .arg(&request.language)
            .arg("-of")
            .arg(&output_prefix)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_command(&mut recognize, cancel, "running local speech recognition")?;
        let srt = fs::read_to_string(output_prefix.with_extension("srt"))
            .map_err(|_| WorkError::Failed("Whisper did not produce a transcript".into()))?;
        let mut cues = parse_srt(&srt)
            .ok_or_else(|| WorkError::Failed("Whisper returned an unreadable transcript".into()))?;
        for cue in &mut cues {
            cue.start += start;
            cue.end += start;
        }
        recognized.extend(cues);
        let _ = tx.send(Event::Progress {
            stage: "matched cues".into(),
            done: index + 1,
            total: spans.len(),
        });
    }
    check_cancel(cancel)?;
    let fit = fit_timing(&original, &recognized).ok_or_else(|| {
        WorkError::Failed(
            "speech match was too weak or inconsistent; no timing change suggested".into(),
        )
    })?;
    let output = work.path.join("preview.srt");
    fs::write(&output, write_srt(&original, fit.scale, fit.offset))?;
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    request.video.hash(&mut identity);
    match &request.subtitle {
        SubtitleInput::External(path) => {
            0u8.hash(&mut identity);
            path.hash(&mut identity);
        }
        SubtitleInput::Embedded { stream } => {
            1u8.hash(&mut identity);
            stream.hash(&mut identity);
        }
    }
    request.audio_stream.hash(&mut identity);
    request.model.hash(&mut identity);
    request.language.hash(&mut identity);
    let accepted = request
        .cache_dir
        .join("wim")
        .join("aligned-subtitles")
        .join(format!("{:016x}.srt", identity.finish()));
    Ok(Preview {
        path: output,
        accepted_path: accepted,
        offset_secs: fit.offset,
        scale: fit.scale,
        matches: fit.matches,
        confidence: fit.confidence,
    })
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), WorkError> {
    if cancel.load(Ordering::Acquire) {
        Err(WorkError::Cancelled)
    } else {
        Ok(())
    }
}

fn run_command(
    command: &mut Command,
    cancel: &AtomicBool,
    description: &str,
) -> Result<(), WorkError> {
    let mut child = command.spawn().map_err(|_| {
        WorkError::Failed(format!(
            "could not start {description}; install FFmpeg and whisper.cpp"
        ))
    })?;
    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WorkError::Cancelled);
        }
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(_) => return Err(WorkError::Failed(format!("failed while {description}"))),
            None => thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[derive(Debug, Clone)]
struct Cue {
    start: f64,
    end: f64,
    text: String,
}

fn parse_srt(input: &str) -> Option<Vec<Cue>> {
    let mut cues = Vec::new();
    for block in input.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines().map(str::trim).filter(|line| !line.is_empty());
        let Some(first) = lines.next() else { continue };
        let timestamp = if first.contains("-->") {
            first
        } else {
            let Some(timestamp) = lines.next() else {
                continue;
            };
            timestamp
        };
        let Some((start, end)) = timestamp.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (
            parse_timestamp(start.trim()),
            end.split_whitespace().next().and_then(parse_timestamp),
        ) else {
            continue;
        };
        let text = lines.collect::<Vec<_>>().join(" ");
        if end > start && !normalize(&text).is_empty() {
            cues.push(Cue { start, end, text });
        }
    }
    (!cues.is_empty()).then_some(cues)
}

fn parse_timestamp(value: &str) -> Option<f64> {
    let value = value.replace(',', ".");
    let mut pieces = value.split(':');
    let hours: f64 = pieces.next()?.parse().ok()?;
    let minutes: f64 = pieces.next()?.parse().ok()?;
    let seconds: f64 = pieces.next()?.parse().ok()?;
    (pieces.next().is_none()
        && hours.is_finite()
        && minutes.is_finite()
        && seconds.is_finite()
        && hours >= 0.0
        && minutes < 60.0
        && seconds < 60.0)
        .then_some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn normalize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() > 1)
        .map(str::to_owned)
        .collect()
}

fn sample_spans(duration: f64) -> Vec<(f64, f64)> {
    let count = ((duration / 75.0).ceil() as usize).clamp(1, SAMPLE_COUNT);
    let sample = SAMPLE_SECONDS.min(duration);
    if count == 1 {
        return vec![((duration - sample) / 2.0, sample)];
    }
    (0..count)
        .map(|i| ((duration - sample) * i as f64 / (count - 1) as f64, sample))
        .collect()
}

#[derive(Debug)]
struct Fit {
    offset: f64,
    scale: f64,
    matches: usize,
    confidence: f64,
}

fn fit_timing(source: &[Cue], speech: &[Cue]) -> Option<Fit> {
    let mut pairs = Vec::<(f64, f64, f64)>::new();
    for cue in source {
        let words = normalize(&cue.text);
        let mut best: Option<(f64, &Cue)> = None;
        for spoken in speech {
            let spoken_words = normalize(&spoken.text);
            let overlap = words
                .iter()
                .filter(|word| spoken_words.contains(word))
                .count();
            let score = (2 * overlap) as f64 / (words.len() + spoken_words.len()).max(1) as f64;
            if best.is_none_or(|(current, _)| score > current) {
                best = Some((score, spoken));
            }
        }
        if let Some((score, spoken)) = best.filter(|(score, _)| *score >= 0.52) {
            pairs.push((cue.start, spoken.start, score));
        }
    }
    if pairs.len() < 4 {
        return None;
    }
    // Search coarse affine hypotheses rather than only taking the median
    // offset: a small clock drift makes offsets spread linearly over a film.
    // Bound the hypothesis set so long subtitle files stay cheap.
    let candidates = if pairs.len() <= 80 {
        pairs.iter().collect::<Vec<_>>()
    } else {
        (0..80)
            .map(|i| &pairs[i * (pairs.len() - 1) / 79])
            .collect::<Vec<_>>()
    };
    let mut best: Option<(usize, f64, f64, f64)> = None;
    for left in 0..candidates.len() {
        for right in left + 1..candidates.len() {
            let (x1, y1, _) = candidates[left];
            let (x2, y2, _) = candidates[right];
            if (x2 - x1).abs() < 1.0 {
                continue;
            }
            let candidate_scale = (y2 - y1) / (x2 - x1);
            if !(0.95..=1.05).contains(&candidate_scale) {
                continue;
            }
            let candidate_offset = y1 - candidate_scale * x1;
            let inliers = pairs
                .iter()
                .filter(|(x, y, _)| (y - (candidate_scale * x + candidate_offset)).abs() <= 1.75)
                .collect::<Vec<_>>();
            let score = inliers.iter().map(|(_, _, score)| score).sum::<f64>();
            if best.is_none_or(|(count, quality, _, _)| {
                inliers.len() > count || (inliers.len() == count && score > quality)
            }) {
                best = Some((inliers.len(), score, candidate_scale, candidate_offset));
            }
        }
    }
    let (best_count, _, best_scale, best_offset) = best?;
    let inliers = pairs
        .iter()
        .filter(|(x, y, _)| (y - (best_scale * x + best_offset)).abs() <= 1.75)
        .collect::<Vec<_>>();
    if inliers.len() != best_count {
        return None;
    }
    if inliers.len() < 4 || inliers.len() * 2 < pairs.len() {
        return None;
    }
    let mean_x = inliers.iter().map(|(x, _, _)| x).sum::<f64>() / inliers.len() as f64;
    let mean_y = inliers.iter().map(|(_, y, _)| y).sum::<f64>() / inliers.len() as f64;
    let covariance = inliers
        .iter()
        .map(|(x, y, _)| (x - mean_x) * (y - mean_y))
        .sum::<f64>();
    let variance = inliers
        .iter()
        .map(|(x, _, _)| (x - mean_x).powi(2))
        .sum::<f64>();
    let scale = if variance > 1.0 {
        covariance / variance
    } else {
        1.0
    };
    if !(0.95..=1.05).contains(&scale) {
        return None;
    }
    let offset = mean_y - scale * mean_x;
    let residuals = inliers
        .iter()
        .map(|(x, y, _)| (y - (scale * x + offset)).abs())
        .collect::<Vec<_>>();
    let mut sorted = residuals.clone();
    sorted.sort_by(f64::total_cmp);
    let median_error = sorted[sorted.len() / 2];
    if median_error > 0.8 {
        return None;
    }
    let confidence = (inliers.iter().map(|(_, _, score)| score).sum::<f64>()
        / inliers.len() as f64)
        * (1.0 - (median_error / 1.6).min(0.8));
    Some(Fit {
        offset,
        scale,
        matches: inliers.len(),
        confidence,
    })
}

fn write_srt(cues: &[Cue], scale: f64, offset: f64) -> String {
    cues.iter()
        .enumerate()
        .map(|(index, cue)| {
            let start = (scale * cue.start + offset).max(0.0);
            let end = (scale * cue.end + offset).max(start + 0.05);
            format!(
                "{}\n{} --> {}\n{}\n\n",
                index + 1,
                format_timestamp(start),
                format_timestamp(end),
                cue.text
            )
        })
        .collect()
}

fn format_timestamp(seconds: f64) -> String {
    let millis = (seconds * 1000.0).round() as u64;
    let h = millis / 3_600_000;
    let m = (millis / 60_000) % 60;
    let s = (millis / 1_000) % 60;
    let ms = millis % 1_000;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cues(lines: &[(&str, f64)]) -> Vec<Cue> {
        lines
            .iter()
            .map(|(text, time)| Cue {
                start: *time,
                end: *time + 2.0,
                text: (*text).into(),
            })
            .collect()
    }

    #[test]
    fn robust_speech_matches_recover_offset_and_modest_clock_drift() {
        let source = cues(&[
            ("we should leave now", 10.0),
            ("the train is ready", 90.0),
            ("where did it go", 180.0),
            ("look behind you", 270.0),
            ("we have to move", 360.0),
        ]);
        let speech = cues(&[
            ("we should leave now", 12.2),
            ("the train is ready", 93.8),
            ("where did it go", 185.6),
            ("look behind you", 277.4),
            ("we have to move", 369.2),
        ]);
        let fit = fit_timing(&source, &speech).unwrap();
        assert!((fit.scale - 1.02).abs() < 0.01);
        assert!(fit.matches >= 4);
        assert!(fit.confidence > 0.8);
    }

    #[test]
    fn unrelated_or_sparse_dialogue_is_rejected() {
        let source = cues(&[
            ("alpha beta gamma", 10.0),
            ("delta epsilon zeta", 20.0),
            ("theta iota kappa", 30.0),
        ]);
        let speech = cues(&[
            ("different words entirely", 12.0),
            ("other language here", 22.0),
        ]);
        assert!(fit_timing(&source, &speech).is_none());
    }

    #[test]
    fn subtitle_time_transform_leaves_source_cues_and_clamps_zero() {
        let original = cues(&[("hello there", 1.0)]);
        let rendered = write_srt(&original, 1.0, -2.0);
        assert!(rendered.contains("00:00:00,000 --> 00:00:01,000"));
        assert_eq!(original[0].start, 1.0);
    }

    #[test]
    fn sample_offsets_cover_the_timeline_and_clamp_count() {
        let spans = sample_spans(600.0);
        assert_eq!(spans.len(), SAMPLE_COUNT);
        assert_eq!(spans[0].0, 0.0);
        assert!((spans.last().unwrap().0 + SAMPLE_SECONDS - 600.0).abs() < 0.01);
        assert_eq!(sample_spans(20.0).len(), 1);
    }
}
