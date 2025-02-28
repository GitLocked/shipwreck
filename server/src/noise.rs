// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use common::terrain;
use common::terrain::*;
use noise::{NoiseFn, SuperSimplex};
use std::mem::MaybeUninit;

static mut NOISE: MaybeUninit<SuperSimplex> = MaybeUninit::uninit();

/// Mutable so that many seeds can be tested (see tests).
pub static mut SEED: f64 = 42700.0;

pub fn init() {
    unsafe { NOISE = MaybeUninit::new(SuperSimplex::new()) }
}

fn get_noise() -> &'static SuperSimplex {
    unsafe { NOISE.assume_init_ref() }
}

/// noise generator returns noise (one of 256 possible Altitude's) for a given terrain coordinate.
pub fn noise_generator(x: usize, y: usize) -> u8 {
    // Distance from border of arctic (positive = arctic, negative = ocean).
    let arctic_distance = y as isize - terrain::ARCTIC as isize;

    // Don't generate land near ocean/arctic border due to "subduction".
    let scale = ((arctic_distance as f64).abs() * (1.0 / 20.0)).min(1.0);

    const S: f64 = SCALE as f64 * 0.0012;
    // Safety: Seed is only ever modified for testing purposes, when there are no other threads
    // accessing the terrain.
    let noise_x = x as f64 * S + unsafe { SEED };
    let noise_y = y as f64 * S;

    // Height in range of 0.0..1.0, 0.0 being the lowest point in the ocean and 1.0 being highest mountain.
    let mut height = fractal_noise(get_noise(), noise_x, noise_y, 4) * scale;

    if arctic_distance > 0 {
        let ice_sheet = (arctic_distance as f64 * (1.0 / 40.0)).min(1.0);

        let v = fractal_noise(get_noise(), noise_x * 0.35 + 1000.0, noise_y * 0.35, 4) * scale;
        let m = (v + 0.04).max(height + 0.25) - (1.0 - ice_sheet);

        // Ice sheets.
        match m {
            m if m > 0.5 => height = height.max(10.0 / 16.0),
            m if m > 0.3 => height = height.max(9.0 / 16.0),
            _ => (),
        }
    }

    // Convert height to u8 (later converted to u4 by terrain).
    (height * 255.0) as u8
}

/// fractal noise returns multi-level noise for a given fractional coordinate.
#[inline]
fn fractal_noise(noise: &SuperSimplex, x: f64, y: f64, octaves: u32) -> f64 {
    (0..octaves)
        .map(|i| {
            let freq = (1 << i) as f64;
            noise.get([x * freq, y * freq]) * (1.0 / freq)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::init;
    use crate::noise::{noise_generator, SEED};
    use common::altitude::Altitude;
    use common::terrain::*;
    use common::util;
    use glam::Vec2;
    use image::{Rgb, RgbImage};

    type Color = [u8; 3];
    const COLORS: [Color; 4] = [
        [0, 50, 115],    // Deep water
        [0, 75, 130],    // Shallow water
        [194, 178, 128], // Sand
        [90, 180, 30],   // Grass
    ];

    fn lerp(a: Color, b: Color, x: f32) -> Color {
        [
            util::lerp(a[0] as f32, b[0] as f32, x) as u8,
            util::lerp(a[1] as f32, b[1] as f32, x) as u8,
            util::lerp(a[2] as f32, b[2] as f32, x) as u8,
        ]
    }

    #[test]
    fn render() {
        init();

        const SIZE: u32 = 3000;
        const ZOOM: f32 = 1.0;

        for s in 100..1000 {
            unsafe {
                // SAFETY: This is test code and has exclusive access.
                SEED = s as f64 * 10.0;
            }

            let mut image = RgbImage::new(SIZE, SIZE);
            let terrain = Terrain::with_generator(noise_generator);

            for j in 0..SIZE {
                for i in 0..SIZE {
                    let pos = Vec2::new(
                        (i as i32 - SIZE as i32 / 2) as f32 * ZOOM,
                        (j as i32 - SIZE as i32 / 2) as f32 * ZOOM,
                    );

                    if pos.length() > 1500.0 {
                        continue;
                    }

                    // let height = terrain.at(i as usize, j as usize);
                    let height =
                        (terrain.sample(pos).unwrap_or(Altitude::MIN).0 as i16 + 128) as f32;
                    let color = if height < 128.0 {
                        lerp(COLORS[0], COLORS[1], height / 127.0)
                    } else if height < 144.0 {
                        lerp(
                            COLORS[1],
                            COLORS[2],
                            ((height - 128.0) * (1.0 / 8.0)).min(1.0),
                        )
                    } else {
                        lerp(COLORS[2], COLORS[3], ((height - 144.0) * 0.05).min(1.0))
                    };
                    *image.get_pixel_mut(i, j) = Rgb::from(color);
                }
            }

            image
                .save(&format!("terrain_test/{}.png", unsafe { SEED }))
                .unwrap();
        }
    }
}
