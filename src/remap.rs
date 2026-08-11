use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

pub struct RemapFiles {
    pub x_path: PathBuf,
    pub y_path: PathBuf,
    pub out_width: u32,
    pub out_height: u32,
    pub supersample: u32,
}

impl Drop for RemapFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.x_path);
        let _ = fs::remove_file(&self.y_path);
    }
}

impl RemapFiles {
    pub fn x_path(&self) -> &Path {
        &self.x_path
    }

    pub fn y_path(&self) -> &Path {
        &self.y_path
    }
}

pub fn generate_remap(
    input_width: u32,
    input_height: u32,
    squeeze: bool,
    supersample: u32,
) -> Result<RemapFiles> {
    let ratio = if squeeze { 4.0 / 3.0 } else { 16.0 / 9.0 };
    let out_width = (input_height as f64 * ratio) as u32 / 2 * 2;
    let out_height = input_height;

    let ss = supersample.max(1);
    let map_width = out_width * ss;
    let map_height = out_height * ss;
    let scaled_input = input_width * ss;

    let row = if squeeze {
        squeeze_row(scaled_input, map_width)
    } else {
        stretch_row(scaled_input, map_width)
    };

    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let x_path = dir.join(format!("superview_x_{pid}.pgm"));
    let y_path = dir.join(format!("superview_y_{pid}.pgm"));

    write_maps(&x_path, &y_path, &row, map_height)?;

    Ok(RemapFiles {
        x_path,
        y_path,
        out_width,
        out_height,
        supersample: ss,
    })
}

fn stretch_source(x: f64, out_width: f64, input_width: f64) -> f64 {
    let width_diff = out_width - input_width;
    let sx = x - width_diff / 2.0;
    let tx = (x / out_width - 0.5) * 2.0;

    let mut offset = tx.powi(2) * (width_diff / 2.0);
    if tx < 0.0 {
        offset *= -1.0;
    }

    sx - offset
}

fn stretch_row(input_width: u32, out_width: u32) -> Vec<u16> {
    let max_x = (input_width - 1) as f64;
    (0..out_width)
        .map(|x| {
            stretch_source(x as f64, out_width as f64, input_width as f64)
                .round()
                .clamp(0.0, max_x) as u16
        })
        .collect()
}

fn squeeze_row(input_width: u32, out_width: u32) -> Vec<u16> {
    let in_w = input_width as f64;
    let out_w = out_width as f64;
    let max_x = in_w - 1.0;
    (0..out_width)
        .map(|s| {
            let target = s as f64;
            let mut lo = 0.0f64;
            let mut hi = in_w;
            for _ in 0..48 {
                let mid = (lo + hi) / 2.0;
                if stretch_source(mid, in_w, out_w) < target {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            ((lo + hi) / 2.0).round().clamp(0.0, max_x) as u16
        })
        .collect()
}

fn write_maps(x_path: &Path, y_path: &Path, row: &[u16], height: u32) -> Result<()> {
    let mut wx = BufWriter::new(File::create(x_path)?);
    let mut wy = BufWriter::new(File::create(y_path)?);

    for w in [&mut wx, &mut wy] {
        writeln!(w, "P5")?;
        writeln!(w, "{} {}", row.len(), height)?;
        writeln!(w, "65535")?;
    }

    let x_row: Vec<u8> = row.iter().flat_map(|v| v.to_be_bytes()).collect();
    let mut y_row = vec![0u8; row.len() * 2];

    for y in 0..height {
        wx.write_all(&x_row)?;
        let y_bytes = (y as u16).to_be_bytes();
        for chunk in y_row.chunks_exact_mut(2) {
            chunk.copy_from_slice(&y_bytes);
        }
        wy.write_all(&y_row)?;
    }

    wx.flush()?;
    wy.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_row_endpoints_and_center() {
        let row = stretch_row(640, 852);
        assert_eq!(row.len(), 852);
        assert_eq!(row[0], 0);
        assert_eq!(row[851], 639);
        assert_eq!(row[426], 320);
    }

    #[test]
    fn stretch_row_is_monotonic() {
        let row = stretch_row(640, 852);
        assert!(row.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn squeeze_row_endpoints_and_center() {
        let row = squeeze_row(852, 640);
        assert_eq!(row.len(), 640);
        assert_eq!(row[0], 0);
        assert!((row[639] as i64 - 851).abs() <= 2);
        assert_eq!(row[320], 426);
    }

    #[test]
    fn squeeze_row_is_monotonic() {
        let row = squeeze_row(852, 640);
        assert!(row.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn squeeze_is_inverse_of_stretch() {
        let fwd = stretch_row(640, 852);
        let inv = squeeze_row(852, 640);
        for (s, &x) in inv.iter().enumerate() {
            let back = fwd[x as usize] as i64;
            assert!(
                (back - s as i64).abs() <= 1,
                "round trip at {s}: {x} -> {back}"
            );
        }
    }

    #[test]
    fn stretch_center_is_near_identity() {
        let row = stretch_row(640, 852);
        let center_out = 426.0;
        let center_in = 320.0;
        for d in [-10i64, -5, 5, 10] {
            let x = (center_out as i64 + d) as usize;
            let expected = center_in as i64 + d;
            let got = row[x] as i64;
            assert!(
                (got - expected).abs() <= 1,
                "near-center pixel {x} maps to {got}, expected ~{expected}"
            );
        }
    }
}
