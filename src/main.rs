mod ffmpeg;
mod remap;

use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap::Parser;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

#[derive(Parser)]
#[command(
    name = "superview-rs",
    version,
    about = "Dynamic video stretching (4:3 to 16:9)"
)]
struct Cli {
    #[arg(short, long, num_args = 1.., required = true)]
    input: Vec<PathBuf>,

    #[arg(short, long)]
    output: Option<PathBuf>,

    #[arg(short, long)]
    encoder: Option<String>,

    #[arg(short, long)]
    bitrate: Option<u64>,

    #[arg(short, long)]
    crf: Option<u8>,

    #[arg(short, long)]
    preset: Option<String>,

    #[arg(short, long, default_value_t = false)]
    squeeze: bool,

    #[arg(long, default_value_t = false, conflicts_with = "squeeze")]
    auto_crop: bool,

    #[arg(long, default_value_t = false, requires = "auto_crop")]
    no_stretch: bool,

    #[arg(long, default_value_t = false)]
    high_quality: bool,

    #[arg(short = 'y', long, default_value_t = false)]
    overwrite: bool,

    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    hw_decode: bool,

    #[arg(long)]
    vaapi_device: Option<PathBuf>,
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    println!("===> Superview-rs - dynamic video stretching <===\n");

    let _ = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            process::exit(130);
        }
        eprintln!("\nInterrupted, cleaning up... (press Ctrl-C again to force quit)");
    });

    let paths = ffmpeg::resolve_paths(cli.ffmpeg_path.as_deref());

    let ff_info = ffmpeg::check_ffmpeg(&paths)?;
    println!("ffmpeg version: {}", ff_info.version);
    println!(
        "H.264/H.265 encoders: {}\n",
        ff_info.h26x_encoders().join(", ")
    );

    if cli.input.len() > 1 && cli.output.is_some() {
        anyhow::bail!(
            "--output cannot be used with multiple inputs; output names are derived per file"
        );
    }

    let total = cli.input.len();
    let mut failed = Vec::new();

    for (i, input) in cli.input.iter().enumerate() {
        if interrupted() {
            anyhow::bail!("Interrupted");
        }
        if total > 1 {
            println!("[{}/{}] {}", i + 1, total, input.display());
        }
        if let Err(e) = process_file(&cli, &paths, &ff_info, input) {
            if interrupted() {
                anyhow::bail!("Interrupted");
            }
            if total == 1 {
                return Err(e);
            }
            eprintln!("Error processing {}: {e:#}\n", input.display());
            failed.push(input.display().to_string());
        }
    }

    if !failed.is_empty() {
        anyhow::bail!(
            "{}/{} files failed: {}",
            failed.len(),
            total,
            failed.join(", ")
        );
    }

    Ok(())
}

fn process_file(
    cli: &Cli,
    paths: &ffmpeg::FfmpegPaths,
    ff_info: &ffmpeg::FfmpegInfo,
    input: &Path,
) -> Result<()> {
    let video = ffmpeg::probe_video(paths, input)?;
    let stream = &video.streams[0];
    let data_streams = ffmpeg::copyable_data_streams(paths, input)?;

    let encoder = ffmpeg::find_encoder(cli.encoder.as_deref(), ff_info, &video)?;

    let vaapi_device = if encoder.contains("vaapi") {
        let device = ffmpeg::find_vaapi_device(
            paths,
            &encoder,
            stream.is_10bit(),
            cli.vaapi_device.as_deref(),
        )?;
        println!("Using VAAPI device: {}", device.display());
        Some(device)
    } else {
        None
    };

    let hwaccel_device = if cli.hw_decode {
        match ffmpeg::find_hw_decode_device(paths, input, cli.vaapi_device.as_deref()) {
            Some(device) => {
                println!("Using VAAPI hardware decoding on {}", device.display());
                Some(device)
            }
            None => {
                eprintln!(
                    "Warning: no VAAPI device can hardware-decode this file, using software decoding"
                );
                None
            }
        }
    } else {
        None
    };

    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| default_output(input, cli));
    if resolved(&output) == resolved(input) {
        anyhow::bail!("Output file would overwrite the input file");
    }
    if output.exists() && !cli.overwrite {
        anyhow::bail!(
            "Output file {} already exists (use --overwrite to replace it)",
            output.display()
        );
    }

    let audio_streams = ffmpeg::audio_streams(paths, input, &output)?;
    for a in &audio_streams {
        if !a.copy {
            eprintln!(
                "Warning: audio codec '{}' (stream #{}) is not supported by the mp4 muxer, re-encoding to AAC",
                a.codec_name, a.index
            );
        }
    }

    let crop = if cli.auto_crop {
        println!("Detecting black bars...");
        let crop = ffmpeg::detect_crop(paths, input, stream.duration_secs())?;
        if crop.w == stream.width && crop.h == stream.height {
            println!("No black bars detected, proceeding without cropping.\n");
            None
        } else {
            let crop_aspect = crop.aspect_ratio();
            println!(
                "Detected content region: {}x{} at offset ({}, {}) — aspect ratio {:.2}",
                crop.w, crop.h, crop.x, crop.y, crop_aspect
            );
            if (crop_aspect - 4.0 / 3.0).abs() > 0.1 {
                eprintln!(
                    "Warning: cropped region aspect ratio {:.2} is not close to 4:3 ({:.2}) - output may look distorted",
                    crop_aspect,
                    4.0 / 3.0
                );
            }
            println!();
            Some(crop)
        }
    } else {
        None
    };

    let quality = if let Some(br) = cli.bitrate {
        ffmpeg::Quality::Bitrate(br)
    } else {
        let crf = cli.crf.unwrap_or_else(|| ffmpeg::default_crf(&encoder));
        ffmpeg::Quality::Crf(crf)
    };

    if cli.no_stretch {
        let crop = crop
            .as_ref()
            .context("--no-stretch used but no black bars were detected")?;

        println!(
            "Cropping {} (codec: {}, duration: {}s) from {}x{} to {}x{} | crop-only",
            input.display(),
            stream.codec_name,
            stream.duration_secs() as u64,
            stream.width,
            stream.height,
            crop.w,
            crop.h,
        );
        print_quality(&encoder, &quality);

        ffmpeg::encode(
            paths,
            &ffmpeg::EncodeOptions {
                input,
                output: &output,
                encoder: &encoder,
                quality: &quality,
                preset: cli.preset.as_deref(),
                stream,
                crop: Some(crop),
                remap: None,
                audio_streams: &audio_streams,
                data_streams: &data_streams,
                vaapi_device: vaapi_device.as_deref(),
                hwaccel_device: hwaccel_device.as_deref(),
            },
            print_progress,
        )?;
    } else {
        let (remap_width, remap_height) = if let Some(ref crop) = crop {
            (crop.w, crop.h)
        } else {
            (stream.width, stream.height)
        };

        if crop.is_none() {
            let aspect = remap_width as f64 / remap_height as f64;
            let expected = if cli.squeeze { 16.0 / 9.0 } else { 4.0 / 3.0 };
            if (aspect - expected).abs() > 0.05 {
                eprintln!(
                    "Warning: input aspect ratio {:.2} is not close to {:.2} ({}) - output may look distorted",
                    aspect,
                    expected,
                    if cli.squeeze { "16:9" } else { "4:3" },
                );
            }
        }

        let supersample = if cli.high_quality { 2 } else { 1 };
        let remap = remap::generate_remap(remap_width, remap_height, cli.squeeze, supersample)?;

        if let Some(ref crop) = crop {
            println!(
                "Scaling {} (codec: {}, duration: {}s) crop {}x{} -> {}x{} | auto-crop",
                input.display(),
                stream.codec_name,
                stream.duration_secs() as u64,
                crop.w,
                crop.h,
                remap.out_width,
                remap.out_height,
            );
        } else {
            println!(
                "Scaling {} (codec: {}, duration: {}s) from {}x{} to {}x{} | squeeze: {}",
                input.display(),
                stream.codec_name,
                stream.duration_secs() as u64,
                stream.width,
                stream.height,
                remap.out_width,
                remap.out_height,
                cli.squeeze,
            );
        }
        print_quality(&encoder, &quality);

        ffmpeg::encode(
            paths,
            &ffmpeg::EncodeOptions {
                input,
                output: &output,
                encoder: &encoder,
                quality: &quality,
                preset: cli.preset.as_deref(),
                stream,
                crop: crop.as_ref(),
                remap: Some(ffmpeg::RemapSpec {
                    x_path: remap.x_path(),
                    y_path: remap.y_path(),
                    out_width: remap.out_width,
                    out_height: remap.out_height,
                    supersample: remap.supersample,
                }),
                audio_streams: &audio_streams,
                data_streams: &data_streams,
                vaapi_device: vaapi_device.as_deref(),
                hwaccel_device: hwaccel_device.as_deref(),
            },
            print_progress,
        )?;
    }

    eprintln!();
    println!("Done! Output file: {}\n", output.display());

    Ok(())
}

fn resolved(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (parent.canonicalize(), path.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

fn default_output(input: &Path, cli: &Cli) -> PathBuf {
    let suffix = if cli.no_stretch {
        "_cropped"
    } else if cli.squeeze {
        "_squeezed"
    } else {
        "_superview"
    };
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    input.with_file_name(format!("{stem}{suffix}.mp4"))
}

fn print_quality(encoder: &str, quality: &ffmpeg::Quality) {
    match quality {
        ffmpeg::Quality::Bitrate(br) => println!(
            "Re-encoding with {} encoder at {} Mbit/s bitrate\n",
            encoder,
            br / 1_000_000
        ),
        ffmpeg::Quality::Crf(crf) => {
            println!("Re-encoding with {} encoder at CRF {}\n", encoder, crf)
        }
    }
}

fn print_progress(p: &ffmpeg::Progress) {
    match p.percent {
        Some(percent) => match (p.speed, p.eta_secs) {
            (Some(speed), Some(eta)) => {
                let eta = eta.round() as u64;
                eprint!(
                    "\rEncoding progress: {:5.1}% (speed {:.2}x, ETA {}:{:02})   ",
                    percent,
                    speed,
                    eta / 60,
                    eta % 60
                );
            }
            _ => eprint!("\rEncoding progress: {:5.1}%", percent),
        },
        None => {
            let done = p.done_secs.round() as u64;
            match p.speed {
                Some(speed) => eprint!(
                    "\rEncoded {}:{:02} (speed {:.2}x)   ",
                    done / 60,
                    done % 60,
                    speed
                ),
                None => eprint!("\rEncoded {}:{:02}", done / 60, done % 60),
            }
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        process::exit(1);
    }
}
