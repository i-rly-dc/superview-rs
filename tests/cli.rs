use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("superview_test_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_test_video(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x480:rate=30:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-y",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "failed to generate test video");
}

fn make_rotated_test_video(path: &Path) {
    let plain = path.with_file_name("plain_portrait.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=480x640:rate=30:duration=1",
            "-c:v",
            "libx264",
            "-y",
        ])
        .arg(&plain)
        .status()
        .unwrap();
    assert!(status.success(), "failed to generate portrait test video");

    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-display_rotation",
            "90",
            "-i",
        ])
        .arg(&plain)
        .args(["-c", "copy", "-y"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "failed to add rotation metadata");
}

fn make_pcm_audio_mkv(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x480:rate=30:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-c:a",
            "pcm_s16le",
            "-y",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "failed to generate pcm-audio test video");
}

fn probe_audio_codec(path: &Path) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn probe_dims(path: &Path) -> (u32, u32) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split(',');
    (
        parts.next().unwrap().parse().unwrap(),
        parts.next().unwrap().parse().unwrap(),
    )
}

fn run_superview(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_superview-rs"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn stretch_and_squeeze_round_trip() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("roundtrip");
    let input = dir.join("in.mp4");
    let stretched = dir.join("stretched.mp4");
    let squeezed = dir.join("squeezed.mp4");
    make_test_video(&input);

    let out = run_superview(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        stretched.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stretch failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(probe_dims(&stretched), (852, 480));

    let out = run_superview(&[
        "-i",
        stretched.to_str().unwrap(),
        "-o",
        squeezed.to_str().unwrap(),
        "-s",
    ]);
    assert!(
        out.status.success(),
        "squeeze failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(probe_dims(&squeezed), (640, 480));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn derives_output_name_and_refuses_overwrite() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("naming");
    let input = dir.join("clip.mp4");
    make_test_video(&input);

    let out = run_superview(&["-i", input.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "run failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let derived = dir.join("clip_superview.mp4");
    assert!(derived.exists(), "derived output file missing");

    let out = run_superview(&["-i", input.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "second run should refuse to overwrite"
    );

    let out = run_superview(&["-i", input.to_str().unwrap(), "--overwrite"]);
    assert!(
        out.status.success(),
        "run with --overwrite failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn high_quality_mode_produces_same_dimensions() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("hq");
    let input = dir.join("in.mp4");
    let output = dir.join("out.mp4");
    make_test_video(&input);

    let out = run_superview(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--high-quality",
    ]);
    assert!(
        out.status.success(),
        "high-quality run failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(probe_dims(&output), (852, 480));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rotated_input_uses_display_dimensions() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("rotated");
    let input = dir.join("rotated.mp4");
    let output = dir.join("out.mp4");
    make_rotated_test_video(&input);

    let out = run_superview(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "rotated run failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(probe_dims(&output), (852, 480));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn transcodes_mp4_incompatible_audio_to_aac() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("audio");
    let input = dir.join("in.mkv");
    let output = dir.join("out.mp4");
    make_pcm_audio_mkv(&input);

    let out = run_superview(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "run with pcm audio failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(probe_audio_codec(&output), "aac");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("re-encoding to AAC"),
        "expected a transcode warning on stderr"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn refuses_output_path_aliasing_the_input() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("alias");
    let input = dir.join("in.mp4");
    make_test_video(&input);
    let input_size = fs::metadata(&input).unwrap().len();

    let out = Command::new(env!("CARGO_BIN_EXE_superview-rs"))
        .current_dir(&dir)
        .args([
            "-i",
            input.to_str().unwrap(),
            "-o",
            "./in.mp4",
            "--overwrite",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "aliasing output should be refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("overwrite the input"),
        "unexpected error:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::metadata(&input).unwrap().len(),
        input_size,
        "input file was modified"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn batch_mode_processes_multiple_inputs() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not found; skipping integration test");
        return;
    }
    let dir = test_dir("batch");
    let a = dir.join("a.mp4");
    let b = dir.join("b.mp4");
    make_test_video(&a);
    make_test_video(&b);

    let out = run_superview(&["-i", a.to_str().unwrap(), b.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "batch run failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("a_superview.mp4").exists());
    assert!(dir.join("b_superview.mp4").exists());

    let _ = fs::remove_dir_all(&dir);
}
