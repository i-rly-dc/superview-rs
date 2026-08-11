use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub struct FfmpegPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

pub fn resolve_paths(explicit: Option<&Path>) -> FfmpegPaths {
    if let Some(path) = explicit {
        let dir = path.parent().unwrap_or(Path::new("."));
        return FfmpegPaths {
            ffmpeg: path.to_path_buf(),
            ffprobe: dir.join(probe_name()),
        };
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(ffmpeg_name());
        if candidate.exists() {
            return FfmpegPaths {
                ffmpeg: candidate,
                ffprobe: dir.join(probe_name()),
            };
        }
    }

    FfmpegPaths {
        ffmpeg: PathBuf::from(ffmpeg_name()),
        ffprobe: PathBuf::from(probe_name()),
    }
}

fn ffmpeg_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn probe_name() -> &'static str {
    if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    }
}

#[derive(Debug, Deserialize)]
pub struct VideoSpecs {
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Option<Format>,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    #[serde(default)]
    pub duration: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Stream {
    pub codec_name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub pix_fmt: Option<String>,
    #[serde(default)]
    pub color_space: Option<String>,
    #[serde(default)]
    pub color_primaries: Option<String>,
    #[serde(default)]
    pub color_transfer: Option<String>,
    #[serde(default)]
    pub color_range: Option<String>,
    #[serde(default)]
    pub side_data_list: Vec<SideData>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SideData {
    #[serde(default)]
    pub rotation: Option<f64>,
}

impl Stream {
    pub fn duration_secs(&self) -> f64 {
        self.duration
            .as_deref()
            .and_then(|d| d.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    pub fn rotation_degrees(&self) -> u32 {
        self.side_data_list
            .iter()
            .find_map(|s| s.rotation)
            .map(|r| ((r.round() as i32 % 360) + 360) as u32 % 360)
            .unwrap_or(0)
    }

    pub fn is_10bit(&self) -> bool {
        self.pix_fmt.as_deref().is_some_and(|p| p.contains("10"))
    }
}

pub enum Quality {
    Crf(u8),
    Bitrate(u64),
}

pub fn default_crf(encoder: &str) -> u8 {
    if encoder.contains("265") || encoder.contains("hevc") {
        18
    } else if encoder.contains("264") {
        23
    } else {
        28
    }
}

pub struct FfmpegInfo {
    pub version: String,
    pub encoders: Vec<String>,
}

impl FfmpegInfo {
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.iter().any(|e| e == name)
    }

    pub fn h26x_encoders(&self) -> Vec<&str> {
        self.encoders
            .iter()
            .map(String::as_str)
            .filter(|e| e.contains("264") || e.contains("265") || e.contains("hevc"))
            .collect()
    }
}

pub fn check_ffmpeg(paths: &FfmpegPaths) -> Result<FfmpegInfo> {
    let output = Command::new(&paths.ffmpeg)
        .arg("-version")
        .output()
        .with_context(|| {
            format!(
                "Cannot find ffmpeg at '{}'. Install it, place it next to superview-rs, or use --ffmpeg-path.",
                paths.ffmpeg.display()
            )
        })?;

    let version_line = String::from_utf8_lossy(&output.stdout);
    let version = version_line
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_string();

    Command::new(&paths.ffprobe)
        .arg("-version")
        .output()
        .with_context(|| {
            format!(
                "Cannot find ffprobe at '{}'. Install it, place it next to superview-rs, or use --ffmpeg-path.",
                paths.ffprobe.display()
            )
        })?;

    let output = Command::new(&paths.ffmpeg)
        .args(["-encoders", "-hide_banner"])
        .output()
        .context("Failed to query ffmpeg encoders")?;

    let encoder_list = String::from_utf8_lossy(&output.stdout);
    let encoders: Vec<String> = encoder_list
        .lines()
        .filter(|line| line.starts_with(" V"))
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect();

    Ok(FfmpegInfo { version, encoders })
}

pub fn probe_video(paths: &FfmpegPaths, file: &Path) -> Result<VideoSpecs> {
    let file_str = file
        .to_str()
        .context("Input file path contains invalid UTF-8")?;
    let output = Command::new(&paths.ffprobe)
        .args([
            "-i",
            file_str,
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height,duration,pix_fmt,color_space,color_primaries,color_transfer,color_range",
            "-show_entries",
            "stream_side_data=rotation",
            "-show_entries",
            "format=duration",
            "-print_format",
            "json",
        ])
        .output()
        .context("Failed to run ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut specs: VideoSpecs =
        serde_json::from_slice(&output.stdout).context("Failed to parse ffprobe output")?;

    if specs.streams.is_empty() {
        bail!("No video streams found in input file");
    }

    if let Some(fmt) = &specs.format
        && specs.streams[0].duration.is_none()
    {
        specs.streams[0].duration = fmt.duration.clone();
    }

    let stream = &mut specs.streams[0];
    let rotation = stream.rotation_degrees();
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut stream.width, &mut stream.height);
        println!(
            "Input has {rotation}° rotation metadata; using display dimensions {}x{}",
            stream.width, stream.height
        );
    }

    Ok(specs)
}

#[derive(Debug, Deserialize)]
struct ProbedStreamList {
    #[serde(default)]
    streams: Vec<ProbedStream>,
}

#[derive(Debug, Deserialize)]
struct ProbedStream {
    index: u32,
    #[serde(default)]
    codec_name: Option<String>,
}

const COPYABLE_DATA_CODECS: &[&str] = &["gpmd"];

const MP4_AUDIO_CODECS: &[&str] = &["aac", "mp3", "mp2", "ac3", "eac3", "alac"];

#[derive(Debug, PartialEq)]
pub struct AudioStream {
    pub index: u32,
    pub codec_name: String,
    pub copy: bool,
}

fn probe_streams(paths: &FfmpegPaths, file: &Path, selector: &str) -> Result<Vec<ProbedStream>> {
    let file_str = file
        .to_str()
        .context("Input file path contains invalid UTF-8")?;
    let output = Command::new(&paths.ffprobe)
        .args([
            "-i",
            file_str,
            "-v",
            "error",
            "-select_streams",
            selector,
            "-show_entries",
            "stream=index,codec_name",
            "-print_format",
            "json",
        ])
        .output()
        .context("Failed to run ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let parsed: ProbedStreamList =
        serde_json::from_slice(&output.stdout).context("Failed to parse ffprobe output")?;

    Ok(parsed.streams)
}

fn filter_copyable(streams: Vec<ProbedStream>) -> Vec<u32> {
    streams
        .into_iter()
        .filter(|s| {
            s.codec_name
                .as_deref()
                .is_some_and(|c| COPYABLE_DATA_CODECS.contains(&c))
        })
        .map(|s| s.index)
        .collect()
}

pub fn copyable_data_streams(paths: &FfmpegPaths, file: &Path) -> Result<Vec<u32>> {
    Ok(filter_copyable(probe_streams(paths, file, "d")?))
}

fn plan_audio(streams: Vec<ProbedStream>, mp4_output: bool) -> Vec<AudioStream> {
    streams
        .into_iter()
        .map(|s| {
            let codec_name = s.codec_name.unwrap_or_else(|| "unknown".to_string());
            let copy = !mp4_output || MP4_AUDIO_CODECS.contains(&codec_name.as_str());
            AudioStream {
                index: s.index,
                codec_name,
                copy,
            }
        })
        .collect()
}

pub fn audio_streams(paths: &FfmpegPaths, file: &Path, output: &Path) -> Result<Vec<AudioStream>> {
    let mp4_output = output
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "m4v" | "mov"));
    Ok(plan_audio(probe_streams(paths, file, "a")?, mp4_output))
}

pub fn find_encoder(
    requested: Option<&str>,
    info: &FfmpegInfo,
    video: &VideoSpecs,
) -> Result<String> {
    if let Some(req) = requested {
        if info.has_encoder(req) {
            return Ok(req.to_string());
        }
        bail!(
            "Requested encoder '{}' is not available. Available H.264/H.265 encoders: {}",
            req,
            info.h26x_encoders().join(", ")
        );
    }

    let codec = &video.streams[0].codec_name;
    let candidate = match codec.as_str() {
        "hevc" | "h265" => "libx265",
        "h264" | "avc" => "libx264",
        "vp9" => "libvpx-vp9",
        "vp8" => "libvpx",
        "av1" => "libsvtav1",
        other => other,
    };
    if info.has_encoder(candidate) {
        return Ok(candidate.to_string());
    }

    for fallback in ["libx264", "libx265"] {
        if info.has_encoder(fallback) {
            eprintln!(
                "Warning: encoder '{}' for input codec '{}' is not available, falling back to {}",
                candidate, codec, fallback
            );
            return Ok(fallback.to_string());
        }
    }

    bail!(
        "No suitable encoder found for input codec '{}'. Available H.264/H.265 encoders: {}",
        codec,
        info.h26x_encoders().join(", ")
    )
}

#[derive(Debug, Clone)]
pub struct CropRect {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

impl CropRect {
    pub fn aspect_ratio(&self) -> f64 {
        self.w as f64 / self.h as f64
    }

    pub fn as_filter(&self) -> String {
        format!("crop={}:{}:{}:{}", self.w, self.h, self.x, self.y)
    }
}

pub fn detect_crop(paths: &FfmpegPaths, file: &Path, duration_secs: f64) -> Result<CropRect> {
    let start = if duration_secs > 60.0 { 30.0 } else { 0.0 };
    let analyze_duration = if duration_secs > 20.0 || duration_secs <= 0.0 {
        10.0
    } else {
        duration_secs
    };

    let output = Command::new(&paths.ffmpeg)
        .args(["-hide_banner", "-ss", &start.to_string(), "-i"])
        .arg(file)
        .args([
            "-t",
            &analyze_duration.to_string(),
            "-vf",
            "cropdetect=24:2:0",
            "-f",
            "null",
            "-",
        ])
        .output()
        .context("Failed to run ffmpeg cropdetect")?;

    if !output.status.success() {
        bail!(
            "ffmpeg cropdetect failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut counts: HashMap<String, usize> = HashMap::new();

    for line in stderr.lines() {
        if let Some(idx) = line.rfind("crop=") {
            let crop_str = &line[idx..];
            let crop_str = crop_str.split_whitespace().next().unwrap_or(crop_str);
            *counts.entry(crop_str.to_string()).or_insert(0) += 1;
        }
    }

    let most_common = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(crop, _)| crop)
        .context("cropdetect produced no output - video may have no frames")?;

    parse_crop(&most_common)
}

fn parse_crop(s: &str) -> Result<CropRect> {
    let values: Vec<&str> = s.strip_prefix("crop=").unwrap_or(s).split(':').collect();

    if values.len() != 4 {
        bail!("Unexpected cropdetect format: {}", s);
    }

    let w: u32 = values[0].parse().context("Invalid crop width")?;
    let h: u32 = values[1].parse().context("Invalid crop height")?;
    let x: u32 = values[2].parse().context("Invalid crop x")?;
    let y: u32 = values[3].parse().context("Invalid crop y")?;

    Ok(CropRect { w, h, x, y })
}

fn vaapi_upload_format(is_10bit: bool) -> &'static str {
    if is_10bit { "p010" } else { "nv12" }
}

fn vaapi_encode_works(paths: &FfmpegPaths, encoder: &str, device: &Path, format: &str) -> bool {
    Command::new(&paths.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-vaapi_device"])
        .arg(device)
        .args([
            "-f",
            "lavfi",
            "-i",
            "color=black:size=320x240:rate=30",
            "-frames:v",
            "1",
            "-vf",
            &format!("format={format},hwupload"),
            "-c:v",
            encoder,
            "-f",
            "null",
            "-",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn find_vaapi_device(
    paths: &FfmpegPaths,
    encoder: &str,
    is_10bit: bool,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(dev) = explicit {
        return Ok(dev.to_path_buf());
    }

    let format = vaapi_upload_format(is_10bit);
    let nodes = render_nodes();

    if nodes.is_empty() {
        bail!("No VAAPI render devices found in /dev/dri (is this a Linux system with a GPU?)");
    }

    for node in &nodes {
        if vaapi_encode_works(paths, encoder, node, format) {
            return Ok(node.clone());
        }
    }

    bail!(
        "No VAAPI device supports encoding {} format with {} (checked {}). Use --vaapi-device to specify one.",
        format,
        encoder,
        nodes
            .iter()
            .map(|n| n.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn render_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
}

fn vaapi_decode_works(paths: &FfmpegPaths, input: &Path, device: &Path) -> bool {
    Command::new(&paths.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-hwaccel",
            "vaapi",
            "-hwaccel_output_format",
            "vaapi",
            "-hwaccel_device",
        ])
        .arg(device)
        .arg("-i")
        .arg(input)
        .args(["-frames:v", "1", "-f", "null", "-"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn find_hw_decode_device(
    paths: &FfmpegPaths,
    input: &Path,
    explicit: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dev) = explicit {
        return vaapi_decode_works(paths, input, dev).then(|| dev.to_path_buf());
    }
    render_nodes()
        .into_iter()
        .find(|node| vaapi_decode_works(paths, input, node))
}

pub struct RemapSpec<'a> {
    pub x_path: &'a Path,
    pub y_path: &'a Path,
    pub out_width: u32,
    pub out_height: u32,
    pub supersample: u32,
}

pub struct EncodeOptions<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub encoder: &'a str,
    pub quality: &'a Quality,
    pub preset: Option<&'a str>,
    pub stream: &'a Stream,
    pub crop: Option<&'a CropRect>,
    pub remap: Option<RemapSpec<'a>>,
    pub audio_streams: &'a [AudioStream],
    pub data_streams: &'a [u32],
    pub vaapi_device: Option<&'a Path>,
    pub hwaccel_device: Option<&'a Path>,
}

pub struct Progress {
    pub percent: Option<f64>,
    pub done_secs: f64,
    pub speed: Option<f64>,
    pub eta_secs: Option<f64>,
}

fn quality_args(encoder: &str, quality: &Quality) -> Result<Vec<String>> {
    Ok(match quality {
        Quality::Bitrate(br) => vec!["-b:v".into(), br.to_string()],
        Quality::Crf(crf) => {
            let crf = crf.to_string();
            if encoder.contains("nvenc") {
                vec![
                    "-rc".into(),
                    "vbr".into(),
                    "-cq".into(),
                    crf,
                    "-b:v".into(),
                    "0".into(),
                ]
            } else if encoder.contains("qsv") {
                vec!["-global_quality".into(), crf]
            } else if encoder.contains("vaapi") {
                vec!["-qp".into(), crf]
            } else if encoder.contains("amf") {
                vec![
                    "-rc".into(),
                    "cqp".into(),
                    "-qp_i".into(),
                    crf.clone(),
                    "-qp_p".into(),
                    crf,
                ]
            } else if encoder.contains("videotoolbox") {
                bail!("CRF mode is not supported with {encoder}; use --bitrate instead")
            } else if encoder.starts_with("libvpx") {
                vec!["-crf".into(), crf, "-b:v".into(), "0".into()]
            } else {
                vec!["-crf".into(), crf]
            }
        }
    })
}

fn build_filter(opts: &EncodeOptions) -> String {
    let is_10bit = opts.stream.is_10bit();
    let fmt_full = if is_10bit { "yuv444p10le" } else { "yuv444p" };
    let fmt_sub = if opts.encoder.contains("vaapi") {
        format!("{},hwupload", vaapi_upload_format(is_10bit))
    } else if is_10bit {
        "yuv420p10le".to_string()
    } else {
        "yuv420p".to_string()
    };

    let crop_part = opts
        .crop
        .map(|c| format!("{},", c.as_filter()))
        .unwrap_or_default();

    match &opts.remap {
        Some(remap) => {
            let (pre, post) = if remap.supersample > 1 {
                let ss = remap.supersample;
                (
                    format!("{crop_part}scale=iw*{ss}:ih*{ss}:flags=bicubic"),
                    format!(
                        ",scale={}:{}:flags=bicubic",
                        remap.out_width, remap.out_height
                    ),
                )
            } else {
                (format!("{crop_part}null"), String::new())
            };
            format!(
                "[0:v]{pre}[pre];[pre][1:v][2:v]remap,format={fmt_full}{post},format={fmt_sub}[v]"
            )
        }
        None => format!("[0:v]{crop_part}format={fmt_sub}[v]"),
    }
}

pub fn encode(
    paths: &FfmpegPaths,
    opts: &EncodeOptions,
    mut progress_cb: impl FnMut(&Progress),
) -> Result<()> {
    let mut cmd = Command::new(&paths.ffmpeg);
    cmd.args([
        "-hide_banner",
        "-progress",
        "pipe:1",
        "-loglevel",
        "error",
        "-ignore_unknown",
        "-y",
    ]);
    if let Some(device) = opts.vaapi_device {
        cmd.arg("-vaapi_device").arg(device);
    }
    if let Some(device) = opts.hwaccel_device {
        cmd.args(["-hwaccel", "vaapi", "-hwaccel_device"]);
        cmd.arg(device);
    }
    cmd.arg("-i");
    cmd.arg(opts.input);
    if let Some(remap) = &opts.remap {
        cmd.arg("-i").arg(remap.x_path).arg("-i").arg(remap.y_path);
    }

    let filter = build_filter(opts);
    cmd.args(["-filter_complex", &filter]);
    cmd.args(["-map", "[v]"]);
    for a in opts.audio_streams {
        cmd.args(["-map", &format!("0:{}", a.index)]);
    }
    for idx in opts.data_streams {
        cmd.args(["-map", &format!("0:{idx}")]);
    }
    cmd.args(["-map_metadata", "0"]);
    cmd.args(["-c:v", opts.encoder]);
    cmd.args(quality_args(opts.encoder, opts.quality)?);
    if let Some(preset) = opts.preset {
        if opts.encoder.contains("vaapi") {
            eprintln!("Warning: VAAPI encoders do not support presets, ignoring --preset {preset}");
        } else {
            cmd.args(["-preset", preset]);
        }
    }
    for (flag, value) in [
        ("-colorspace", &opts.stream.color_space),
        ("-color_primaries", &opts.stream.color_primaries),
        ("-color_trc", &opts.stream.color_transfer),
        ("-color_range", &opts.stream.color_range),
    ] {
        if let Some(v) = value
            && v != "unknown"
            && v != "unspecified"
        {
            cmd.args([flag, v]);
        }
    }
    for (n, a) in opts.audio_streams.iter().enumerate() {
        cmd.arg(format!("-c:a:{n}"));
        if a.copy {
            cmd.arg("copy");
        } else {
            cmd.arg("aac");
            cmd.arg(format!("-b:a:{n}"));
            cmd.arg("192k");
        }
    }
    cmd.args(["-c:d", "copy"]);
    if opts.encoder == "libx265" {
        cmd.args(["-x265-params", "log-level=error"]);
    }
    cmd.arg(opts.output);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("Failed to start ffmpeg")?;

    let stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut buf);
        buf
    });

    let duration_secs = opts.stream.duration_secs();
    let mut speed: Option<f64> = None;

    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(speed_str) = line.strip_prefix("speed=") {
            speed = speed_str.trim().trim_end_matches('x').parse::<f64>().ok();
        } else if let Some(time_str) = line.strip_prefix("out_time_ms=")
            && let Ok(time_us) = time_str.parse::<f64>()
        {
            let done_secs = time_us / 1_000_000.0;
            let (percent, eta_secs) = if duration_secs > 0.0 {
                (
                    Some((done_secs / duration_secs * 100.0).min(100.0)),
                    speed
                        .filter(|s| *s > 0.0)
                        .map(|s| (duration_secs - done_secs).max(0.0) / s),
                )
            } else {
                (None, None)
            };
            progress_cb(&Progress {
                percent,
                done_secs,
                speed,
                eta_secs,
            });
        }
    }

    let status = child.wait()?;
    let stderr_text = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(opts.output);
        bail!("ffmpeg exited with status {}:\n{}", status, stderr_text);
    }

    let stderr_text = stderr_text.trim();
    if !stderr_text.is_empty() {
        eprintln!("\nffmpeg messages:\n{stderr_text}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_crop_valid() {
        let c = parse_crop("crop=640:480:0:80").unwrap();
        assert_eq!((c.w, c.h, c.x, c.y), (640, 480, 0, 80));
    }

    #[test]
    fn parse_crop_without_prefix() {
        let c = parse_crop("1920:1080:0:0").unwrap();
        assert_eq!((c.w, c.h, c.x, c.y), (1920, 1080, 0, 0));
    }

    #[test]
    fn parse_crop_rejects_garbage() {
        assert!(parse_crop("crop=640:480").is_err());
        assert!(parse_crop("crop=a:b:c:d").is_err());
    }

    #[test]
    fn quality_args_per_encoder() {
        let crf = Quality::Crf(23);
        assert_eq!(quality_args("libx264", &crf).unwrap(), vec!["-crf", "23"]);
        assert_eq!(
            quality_args("h264_nvenc", &crf).unwrap(),
            vec!["-rc", "vbr", "-cq", "23", "-b:v", "0"]
        );
        assert_eq!(
            quality_args("hevc_qsv", &crf).unwrap(),
            vec!["-global_quality", "23"]
        );
        assert_eq!(quality_args("h264_vaapi", &crf).unwrap(), vec!["-qp", "23"]);
        assert_eq!(
            quality_args("libvpx-vp9", &crf).unwrap(),
            vec!["-crf", "23", "-b:v", "0"]
        );
        assert!(quality_args("hevc_videotoolbox", &crf).is_err());
        assert_eq!(
            quality_args("libx265", &Quality::Bitrate(100_000_000)).unwrap(),
            vec!["-b:v", "100000000"]
        );
    }

    #[test]
    fn default_crf_by_encoder() {
        assert_eq!(default_crf("libx265"), 18);
        assert_eq!(default_crf("hevc_nvenc"), 18);
        assert_eq!(default_crf("libx264"), 23);
        assert_eq!(default_crf("libsvtav1"), 28);
    }

    fn specs_with_codec(codec: &str) -> VideoSpecs {
        VideoSpecs {
            streams: vec![Stream {
                codec_name: codec.to_string(),
                width: 640,
                height: 480,
                ..Default::default()
            }],
            format: None,
        }
    }

    fn info_with_encoders(encoders: &[&str]) -> FfmpegInfo {
        FfmpegInfo {
            version: "test".to_string(),
            encoders: encoders.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn find_encoder_uses_requested_when_available() {
        let info = info_with_encoders(&["libx264", "libx265"]);
        let specs = specs_with_codec("h264");
        assert_eq!(
            find_encoder(Some("libx265"), &info, &specs).unwrap(),
            "libx265"
        );
    }

    #[test]
    fn find_encoder_rejects_unavailable_request() {
        let info = info_with_encoders(&["libx264"]);
        let specs = specs_with_codec("h264");
        assert!(find_encoder(Some("h264_nvenc"), &info, &specs).is_err());
    }

    #[test]
    fn find_encoder_maps_codec_and_falls_back() {
        let info = info_with_encoders(&["libx264", "libx265"]);
        assert_eq!(
            find_encoder(None, &info, &specs_with_codec("hevc")).unwrap(),
            "libx265"
        );
        assert_eq!(
            find_encoder(None, &info, &specs_with_codec("av1")).unwrap(),
            "libx264"
        );
    }

    #[test]
    fn find_encoder_errors_with_no_options() {
        let info = info_with_encoders(&[]);
        assert!(find_encoder(None, &info, &specs_with_codec("h264")).is_err());
    }

    #[test]
    fn build_filter_uploads_for_vaapi() {
        let stream = Stream::default();
        let quality = Quality::Crf(20);
        let mut opts = EncodeOptions {
            input: Path::new("in.mp4"),
            output: Path::new("out.mp4"),
            encoder: "hevc_vaapi",
            quality: &quality,
            preset: None,
            stream: &stream,
            crop: None,
            remap: None,
            audio_streams: &[],
            data_streams: &[],
            vaapi_device: None,
            hwaccel_device: None,
        };
        assert!(build_filter(&opts).ends_with("format=nv12,hwupload[v]"));
        opts.encoder = "libx265";
        assert!(build_filter(&opts).ends_with("format=yuv420p[v]"));

        let stream_10bit = Stream {
            pix_fmt: Some("yuv420p10le".to_string()),
            ..Default::default()
        };
        opts.stream = &stream_10bit;
        opts.encoder = "hevc_vaapi";
        assert!(build_filter(&opts).ends_with("format=p010,hwupload[v]"));
        opts.encoder = "libx265";
        assert!(build_filter(&opts).ends_with("format=yuv420p10le[v]"));
    }

    #[test]
    fn vaapi_upload_format_matches_bit_depth() {
        assert_eq!(vaapi_upload_format(false), "nv12");
        assert_eq!(vaapi_upload_format(true), "p010");
    }

    #[test]
    fn filter_copyable_keeps_only_known_data_codecs() {
        let streams = vec![
            ProbedStream {
                index: 2,
                codec_name: None,
            },
            ProbedStream {
                index: 3,
                codec_name: Some("gpmd".to_string()),
            },
            ProbedStream {
                index: 4,
                codec_name: Some("unknown".to_string()),
            },
        ];
        assert_eq!(filter_copyable(streams), vec![3]);
    }

    #[test]
    fn plan_audio_transcodes_incompatible_codecs_for_mp4() {
        let streams = vec![
            ProbedStream {
                index: 1,
                codec_name: Some("aac".to_string()),
            },
            ProbedStream {
                index: 2,
                codec_name: Some("vorbis".to_string()),
            },
            ProbedStream {
                index: 3,
                codec_name: Some("pcm_s16le".to_string()),
            },
            ProbedStream {
                index: 4,
                codec_name: None,
            },
        ];
        let plan = plan_audio(streams, true);
        assert_eq!(
            plan.iter().map(|a| (a.index, a.copy)).collect::<Vec<_>>(),
            vec![(1, true), (2, false), (3, false), (4, false)]
        );
    }

    #[test]
    fn plan_audio_copies_everything_for_non_mp4() {
        let streams = vec![ProbedStream {
            index: 1,
            codec_name: Some("vorbis".to_string()),
        }];
        let plan = plan_audio(streams, false);
        assert!(plan[0].copy);
    }

    #[test]
    fn rotation_degrees_normalizes() {
        let rotated = |r: f64| Stream {
            side_data_list: vec![SideData { rotation: Some(r) }],
            ..Default::default()
        };
        assert_eq!(Stream::default().rotation_degrees(), 0);
        assert_eq!(rotated(90.0).rotation_degrees(), 90);
        assert_eq!(rotated(-90.0).rotation_degrees(), 270);
        assert_eq!(rotated(180.0).rotation_degrees(), 180);
        assert_eq!(rotated(-180.0).rotation_degrees(), 180);
        assert_eq!(rotated(450.0).rotation_degrees(), 90);
    }

    #[test]
    fn stream_parses_rotation_side_data() {
        let json = r#"{
            "codec_name": "h264",
            "width": 480,
            "height": 640,
            "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]
        }"#;
        let s: Stream = serde_json::from_str(json).unwrap();
        assert_eq!(s.rotation_degrees(), 270);
    }

    #[test]
    fn stream_10bit_detection() {
        let mut s = Stream {
            pix_fmt: Some("yuv420p10le".to_string()),
            ..Default::default()
        };
        assert!(s.is_10bit());
        s.pix_fmt = Some("yuv420p".to_string());
        assert!(!s.is_10bit());
        s.pix_fmt = None;
        assert!(!s.is_10bit());
    }
}
