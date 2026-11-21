//! Threshold-based VNG reconstruction for 1:1 native detail viewports.

use crate::debayer::reflect;
use fulgur_core::{Color, DemosaicMethod, Error, RawFrame, Result, RgbImage, check_cancelled};
use rayon::prelude::*;
use std::sync::atomic::AtomicBool;

include!(concat!(env!("OUT_DIR"), "/vng_kernels.rs"));

#[path = "vng_simd.rs"]
mod simd;

pub fn vng_demosaic(frame: &RawFrame, cfa: &[f32], cancel: &AtomicBool) -> Result<RgbImage> {
    vng_demosaic_with(frame, cfa, cancel, &simd::Kernel::detect(), Vec::new())
}

fn vng_demosaic_with(
    frame: &RawFrame,
    cfa: &[f32],
    cancel: &AtomicBool,
    kernel: &simd::Kernel,
    mut pixels: Vec<[f32; 3]>,
) -> Result<RgbImage> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    let pattern = crate::debayer::bayer_pattern(frame)?;
    if width < 6 || height < 6 || width.checked_mul(height) != Some(cfa.len()) {
        return Err(Error::Invalid("Invalid CFA reconstruction dimensions."));
    }
    let bounded: Vec<bool> = if kernel.vector {
        cfa.par_chunks(width).map(simd::bounded).collect()
    } else {
        vec![false; height]
    };
    pixels.resize(cfa.len(), [0.0; 3]);
    pixels
        .par_chunks_mut(width)
        .enumerate()
        .try_for_each(|(y, row)| -> Result<()> {
            check_cancelled(cancel)?;
            let span = [0, width];
            let sources: [usize; 5] =
                std::array::from_fn(|i| reflect(y as isize + i as isize - 2, height));
            let rows: [&[f32]; 5] = sources.map(|yy| &cfa[yy * width..(yy + 1) * width]);
            let vector = sources.iter().all(|&yy| bounded[yy]).then_some(kernel);
            let center = pattern.color(0, y);
            let right = pattern.color(1, y);
            match (center, right) {
                (Color::Green, Color::Red) => reconstruct_row::<0, 1>(&rows, row, vector, span),
                (Color::Red, Color::Green) => reconstruct_row::<1, 0>(&rows, row, vector, span),
                (Color::Green, Color::Blue) => reconstruct_row::<2, 3>(&rows, row, vector, span),
                (Color::Blue, Color::Green) => reconstruct_row::<3, 2>(&rows, row, vector, span),
                _ => unreachable!("A Bayer row alternates green and red/blue."),
            }
            Ok(())
        })?;
    Ok(RgbImage {
        width,
        height,
        demosaic_method: DemosaicMethod::Vng,
        pixels,
    })
}

#[inline]
fn interior_window(rows: &[&[f32]; 5], x: usize) -> [f32; 25] {
    let mut window = [0.; 25];
    for (j, row) in rows.iter().enumerate() {
        window[j * 5..j * 5 + 5].copy_from_slice(&row[x - 2..x + 3]);
    }
    window
}

fn reflected_window(rows: &[&[f32]; 5], x: usize, width: usize) -> [f32; 25] {
    std::array::from_fn(|i| rows[i / 5][reflect(x as isize + (i % 5) as isize - 2, width)])
}

fn reconstruct_row<const LEFT: usize, const RIGHT: usize>(
    rows: &[&[f32]; 5],
    row: &mut [[f32; 3]],
    vector: Option<&simd::Kernel>,
    [from, to]: [usize; 2],
) {
    let width = row.len();
    if from < 2 {
        row[0] = reconstruct::<LEFT>(&reflected_window(rows, 0, width));
        row[1] = reconstruct::<RIGHT>(&reflected_window(rows, 1, width));
    }
    let first = (from / 2 * 2).max(2);
    let last = (to.div_ceil(2) * 2).min(width - 2);
    if first < last {
        let bounds = first - 2..last + 2;
        let interior = rows.map(|r| &r[bounds.clone()]);
        reconstruct_interior::<LEFT, RIGHT>(&interior, &mut row[bounds], vector);
    }
    if to > width - 2 {
        if width % 2 == 0 {
            row[width - 2] = reconstruct::<LEFT>(&reflected_window(rows, width - 2, width));
            row[width - 1] = reconstruct::<RIGHT>(&reflected_window(rows, width - 1, width));
        } else {
            row[width - 2] = reconstruct::<RIGHT>(&reflected_window(rows, width - 2, width));
            row[width - 1] = reconstruct::<LEFT>(&reflected_window(rows, width - 1, width));
        }
    }
}

fn reconstruct_interior<const LEFT: usize, const RIGHT: usize>(
    rows: &[&[f32]; 5],
    row: &mut [[f32; 3]],
    vector: Option<&simd::Kernel>,
) {
    let width = row.len();
    let done = vector.map_or(0, |kernel| kernel.pairs::<LEFT, RIGHT>(rows, row));
    let mut pairs = row[2 + 2 * done..width - 2].chunks_exact_mut(2);
    for (i, pair) in pairs.by_ref().enumerate() {
        let x = 2 + (done + i) * 2;
        pair[0] = reconstruct::<LEFT>(&interior_window(rows, x));
        pair[1] = reconstruct::<RIGHT>(&interior_window(rows, x + 1));
    }
    if let Some(last) = pairs.into_remainder().first_mut() {
        *last = reconstruct::<LEFT>(&interior_window(rows, width - 3));
    }
}
