// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::altitude::Altitude;
use crate::protocol::TerrainUpdate;
use crate::transform::DimensionTransform;
use crate::util::lerp;
use crate::world;
use fast_hilbert as hilbert;
use glam::Vec2;
use lazy_static::lazy_static;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem::{size_of, transmute};
use std::ops::{Add, Mul, RangeInclusive, Sub};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Scale of terrain aka meters per pixel.
pub const SCALE: f32 = 25.0;
// Size of whole terrain.
// Must be a power of 2.
pub const SIZE: usize = (1 << 10) * crate::world::SIZE;
// Offset to convert between signed coordinates to unsigned.
const OFFSET: isize = (SIZE / 2) as isize;
// Position of arctic biome in terrain y coordinate.
pub const ARCTIC: usize = ((world::ARCTIC / SCALE) as isize + OFFSET) as usize;

// Size of a chunk.
// Must be a power of 2.
const CHUNK_SIZE: usize = 1 << 6;
// Offset to convert between signed chunk coordinates to unsigned.
const CHUNK_OFFSET: isize = (SIZE / CHUNK_SIZE / 2) as isize;
// Size of terrain in chunks.
const SIZE_CHUNKS: usize = SIZE / CHUNK_SIZE;

pub const SAND_LEVEL: Altitude = Altitude(0);
pub const GRASS_LEVEL: Altitude = Altitude(1 << 4);

/// Terrain data to altitude (non-linear, to allow both shallow and deep areas).
const ALTITUDE_LUT: [i8; 17] = [
    i8::MIN,
    -115,
    -100,
    -50,
    -20,
    -5,
    -2,
    -1,
    0,
    1,
    2,
    5,
    20,
    50,
    100,
    115,
    i8::MAX,
];

/// Offset the terrain by 6 units, so that the strata representing sea level is slightly
/// above 0. A typical terrain shader would add a similar amount (on average, via noise) to
/// make islands more interesting (but still smooth).
const DATA_OFFSET: u8 = 6;

/// Converts terrain data into [`Altitude`].
const fn lookup_altitude(data: u8) -> Altitude {
    let data = data.saturating_add(DATA_OFFSET);

    // Linearly interpolate between adjacent altitudes.
    let low = ALTITUDE_LUT[(data >> 4) as usize] as i16;
    let high = ALTITUDE_LUT[((data >> 4) + 1) as usize] as i16;
    let frac = (data & 0b1111) as i16;
    Altitude((low + (high - low) * frac / 0b10000) as i8)
}

/// Converts [`Altitude`] into terrain data.
///
/// TODO: Doesn't interpolate at all. Only returns multiples of 16, minus DATA_OFFSET.
fn reverse_lookup_altitude(altitude: Altitude) -> u8 {
    (ALTITUDE_LUT
        .binary_search(&altitude.0)
        .map_err(|n| n.saturating_sub(1))
        .into_ok_or_err() as u8)
        .saturating_mul(16) //.saturating_sub(DATA_OFFSET)
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Coord(pub usize, pub usize);

/// Any terrain pixel can be represented as a `Coord`.
impl Coord {
    pub fn from_position(v: Vec2) -> Option<Self> {
        let v = v.mul(1.0 / SCALE);
        Self::from_scaled_position(v)
    }

    /// Converts a position to the nearest valid `Coord`.
    fn saturating_from_position(mut pos: Vec2) -> Self {
        pos *= 1.0 / (SCALE as f32);
        pos += OFFSET as f32;
        let x = (pos.x as i64).clamp(0, (SIZE - 1) as i64) as usize;
        let y = (pos.y as i64).clamp(0, (SIZE - 1) as i64) as usize;
        Self(x, y)
    }

    fn from_scaled_position(v: Vec2) -> Option<Self> {
        let coord = unsafe {
            Self(
                (v.x.to_int_unchecked::<isize>() + OFFSET) as usize,
                (v.y.to_int_unchecked::<isize>() + OFFSET) as usize,
            )
        };

        if coord.0 >= SIZE || coord.1 >= SIZE {
            None
        } else {
            Some(coord)
        }
    }

    pub fn corner(&self) -> Vec2 {
        // TODO investigate if this is actually the corner.
        let pos = Vec2::new(
            (self.0 as isize - OFFSET) as f32,
            (self.1 as isize - OFFSET) as f32,
        )
        .mul(SCALE);
        debug_assert_eq!(self, &Self::saturating_from_position(pos));
        pos
    }
}

impl Add<RelativeCoord> for Coord {
    type Output = Coord;
    fn add(self, rhs: RelativeCoord) -> Self::Output {
        Self(self.0 + rhs.0 as usize, self.1 + rhs.1 as usize)
    }
}

pub fn signed_coord_corner(x: isize, y: isize) -> Vec2 {
    Vec2::new((x - OFFSET) as f32, (y - OFFSET) as f32).mul(SCALE)
}

impl<U> From<(U, U)> for Coord
where
    U: Into<u64>,
{
    fn from(x: (U, U)) -> Self {
        Self(x.0.into() as usize, x.1.into() as usize)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChunkId(pub u16, pub u16);

/// Any terrain chunk can be represented as a `ChunkId`.
impl ChunkId {
    fn as_index(&self) -> usize {
        self.0 as usize + self.1 as usize * SIZE_CHUNKS
    }

    fn from_index(index: usize) -> Self {
        Self((index % SIZE_CHUNKS) as u16, (index / SIZE_CHUNKS) as u16)
    }

    pub fn as_coord(&self) -> Coord {
        Coord(self.0 as usize * CHUNK_SIZE, self.1 as usize * CHUNK_SIZE)
    }

    #[inline]
    fn from_coord(coord: Coord) -> Self {
        Self((coord.0 / CHUNK_SIZE) as u16, (coord.1 / CHUNK_SIZE) as u16)
    }

    fn as_position(&self) -> Vec2 {
        Vec2::new(
            (self.0 as isize - CHUNK_OFFSET) as f32,
            (self.1 as isize - CHUNK_OFFSET) as f32,
        )
        .add(0.5)
        .mul(SCALE * CHUNK_SIZE as f32)
    }

    fn in_radius(&self, position: Vec2, radius: f32) -> bool {
        const HALF: f32 = (SCALE * CHUNK_SIZE as f32) / 2.0;
        let abs_diff = self.as_position().sub(position).abs();
        if abs_diff.x > HALF + radius || abs_diff.y > HALF + radius {
            false
        } else if abs_diff.x <= HALF || abs_diff.y <= HALF {
            true
        } else {
            abs_diff.sub(HALF).max(Vec2::ZERO).length_squared() < radius.powi(2)
        }
    }

    fn saturating_from(mut pos: Vec2) -> Self {
        pos *= 1.0 / (SCALE * CHUNK_SIZE as f32);
        pos += SIZE_CHUNKS as f32 / 2.0;
        let x = (pos.x as i32).clamp(0, (SIZE_CHUNKS - 1) as i32) as u16;
        let y = (pos.y as i32).clamp(0, (SIZE_CHUNKS - 1) as i32) as u16;
        Self(x, y)
    }
}

impl TryFrom<Vec2> for ChunkId {
    type Error = &'static str;

    fn try_from(mut pos: Vec2) -> Result<Self, Self::Error> {
        pos *= 1.0 / (SCALE * CHUNK_SIZE as f32);
        pos += SIZE as f32 / 2.0;
        let (x, y) = (pos.x as i32, pos.y as i32);
        const RANGE: RangeInclusive<i32> = 0..=((SIZE_CHUNKS - 1) as i32);
        if RANGE.contains(&x) && RANGE.contains(&y) {
            Ok(Self(x as u16, y as u16))
        } else {
            Err("out of terrain")
        }
    }
}

type Generator = fn(usize, usize) -> u8;

/// Always returns zero. For placeholder purposes, or when no generator is required.
fn zero_generator(_: usize, _: usize) -> u8 {
    0
}

/// Terrain stores a bitmap representing the altitude at each pixel in a 2D grid.
pub struct Terrain {
    chunks: [[Option<Box<Chunk>>; SIZE_CHUNKS]; SIZE_CHUNKS],
    /// Which chunks were modified since the last reset.
    /// Resets are triggered by terrain regeneration in post update on the server
    /// and the background layer on the client.
    pub updated: ChunkSet,
    /// Guards chunk generation.
    mutex: Mutex<()>,
    generator: Generator,
}

pub struct TerrainMutation {
    /// Four surrounding pixels will be affected.
    position: Vec2,
    /// Amount to add/subtract.
    amount: f32,
    /// Pixels lying in this range are modified.
    condition: RangeInclusive<Altitude>,
    /// Modified pixels will be clamped to this range.
    clamp: RangeInclusive<Altitude>,
}

impl TerrainMutation {
    /// Changes the height of nearby terrain.
    pub fn simple(position: Vec2, amount: f32) -> Self {
        Self {
            position,
            amount,
            condition: Altitude::MIN..=Altitude::MAX,
            clamp: Altitude::MIN..=Altitude::MAX,
        }
    }

    /// Changes the height of nearby terrain provided it is within a range.
    pub fn conditional(position: Vec2, amount: f32, condition: RangeInclusive<Altitude>) -> Self {
        Self {
            position,
            amount,
            condition,
            clamp: Altitude::MIN..=Altitude::MAX,
        }
    }

    /// Changes the height of nearby terrain such that it remain within a range.
    pub fn clamped(position: Vec2, amount: f32, clamp: RangeInclusive<Altitude>) -> Self {
        Self {
            position,
            amount,
            condition: Altitude::MIN..=Altitude::MAX,
            clamp,
        }
    }

    /// Changes the height of nearby terrain provided it is within a range, such that it remains within
    /// another range.
    pub fn conditional_clamped(
        position: Vec2,
        amount: f32,
        condition: RangeInclusive<Altitude>,
        clamp: RangeInclusive<Altitude>,
    ) -> Self {
        Self {
            position,
            amount,
            condition,
            clamp,
        }
    }
}

impl Terrain {
    /// Allocates a Terrain with a zero generator.
    pub fn new() -> Self {
        Self::with_generator(zero_generator)
    }

    /// Allocates a Terrain with a custom generator, but does not actually generate any chunks.
    pub fn with_generator(generator: Generator) -> Self {
        const NONE_CHUNK: Option<Box<Chunk>> = None;
        const NONE_CHUNK_ROW: [Option<Box<Chunk>>; SIZE_CHUNKS] = [NONE_CHUNK; SIZE_CHUNKS];

        Self {
            chunks: [NONE_CHUNK_ROW; SIZE_CHUNKS],
            updated: ChunkSet::new(),
            mutex: Mutex::new(()),
            generator,
        }
    }

    /// Returns the maximum world radius to not exceed the terrain size, which is effectively a
    /// constant.
    pub fn max_world_radius() -> f32 {
        (SIZE / 2) as f32 * SCALE
    }

    /// Returns a mutable reference to a chunk.
    pub fn mut_chunk(&mut self, chunk_id: ChunkId) -> &mut Chunk {
        let chunk = &mut self.chunks[chunk_id.1 as usize][chunk_id.0 as usize];
        if chunk.is_none() {
            *chunk = Some(Chunk::new(chunk_id, self.generator));
        }
        chunk.as_mut().unwrap()
    }

    /// Gets a reference to a chunk, generating it if necessary.
    #[inline]
    pub fn get_chunk(&self, chunk_id: ChunkId) -> &Chunk {
        unsafe {
            let ptr: &AtomicPtr<Chunk> =
                transmute(&self.chunks[chunk_id.1 as usize][chunk_id.0 as usize]);
            if let Some(chunk) = ptr.load(Ordering::Relaxed).as_ref() {
                return chunk;
            }
            self.get_chunk_slow(ptr, chunk_id)
        }
    }

    #[inline(never)]
    unsafe fn get_chunk_slow(&self, ptr: &AtomicPtr<Chunk>, chunk_id: ChunkId) -> &Chunk {
        let lock = self.mutex.lock().unwrap();
        if let Some(chunk) = ptr.load(Ordering::Acquire).as_ref() {
            return chunk;
        }

        // TODO generate in parallel.
        let chunk = Box::into_raw(Chunk::new(chunk_id, self.generator));
        ptr.store(chunk, Ordering::Release);
        drop(lock);
        chunk.as_ref().unwrap()
    }

    /// Applies a terrain update, overwriting relevant terrain pixels.
    pub fn apply_update(&mut self, update: &TerrainUpdate) {
        for (chunk_id, serialized) in update.iter() {
            self.mut_chunk(*chunk_id).apply_serialized_chunk(serialized);
            self.updated.add(*chunk_id)
        }
    }

    /// Gets the raw terrain data at a Coord.
    pub fn at(&self, coord: Coord) -> u8 {
        self.get_chunk(ChunkId::from_coord(coord)).at(coord)
    }

    /// returns an iterator that iterates exactly width * height terrain pixels.
    /// If a given terrain pixel lies outside the terrain it will evaluate to default.
    pub fn iter_rect_or(
        &self,
        center: Coord,
        width: usize,
        height: usize,
        default: u8,
    ) -> impl Iterator<Item = u8> + '_ {
        let mut cached_chunk_id = None;
        let mut cached_chunk = None;

        (0..height)
            .flat_map(move |j| (0..width).map(move |i| (i, j)))
            .map(move |(i, j)| {
                let x = center.0 as isize + (i as isize - (width / 2) as isize);
                let y = center.1 as isize + (j as isize - (height / 2) as isize);

                if x >= 0 && x < SIZE as isize && y >= 0 && y < SIZE as isize {
                    let coord = Coord(x as usize, y as usize);
                    let chunk_id = ChunkId::from_coord(coord);

                    // Cache chunk for faster lookup.
                    if Some(chunk_id) != cached_chunk_id {
                        cached_chunk_id = Some(chunk_id);
                        cached_chunk = Some(self.get_chunk(chunk_id));
                    }

                    cached_chunk.unwrap().at(coord)
                } else {
                    default
                }
            })
    }

    /// Sets the raw terrain data at a Coord. Returns if actually changed underlying data.
    fn set(&mut self, coord: Coord, value: u8) -> bool {
        let chunk_id = ChunkId::from_coord(coord);
        let chunk = self.mut_chunk(chunk_id);

        // Don't record sets that change nothing.
        if chunk.at(coord) & 0b11110000 != value & 0b11110000 {
            chunk.set_capture(coord, value);
            self.updated.add(chunk_id);
            true
        } else {
            false
        }
    }

    /// Gets the smoothed Altitude at a position.
    pub fn sample(&self, pos: Vec2) -> Option<Altitude> {
        let pos = pos.mul(1.0 / SCALE);

        let c_pos = pos.ceil();
        let c = Coord::from_scaled_position(c_pos)?;
        let Coord(cx, cy) = c;

        let f_pos = pos.floor();
        let f = Coord::from_scaled_position(f_pos)?;
        let Coord(fx, fy) = f;

        // Sample 2x2 grid
        // 00 10
        // 01 11
        let c00: u8;
        let c10: u8;
        let c01: u8;
        let c11: u8;

        let chunk_id = ChunkId::from_coord(f);
        if chunk_id == ChunkId::from_coord(c) {
            let chunk = self.get_chunk(chunk_id);
            c00 = chunk.at(Coord(fx, fy));
            c10 = chunk.at(Coord(cx, fy));
            c01 = chunk.at(Coord(fx, cy));
            c11 = chunk.at(Coord(cx, cy));
        } else {
            c00 = self.at(Coord(fx, fy));
            c10 = self.at(Coord(cx, fy));
            c01 = self.at(Coord(fx, cy));
            c11 = self.at(Coord(cx, cy));
        }

        let delta = pos.sub(f_pos);
        Some(lookup_altitude(lerp(
            lerp(c00 as f32, c10 as f32, delta.x),
            lerp(c01 as f32, c11 as f32, delta.x),
            delta.y,
        ) as u8))
    }

    /// collides_with returns one point (and the altitude there) of collision if an entity collides
    /// with the terrain any time in the next delta_seconds.
    pub fn collides_with(
        &self,
        mut dim_transform: DimensionTransform,
        threshold: Altitude,
        delta_seconds: f32,
    ) -> Option<(Vec2, Altitude)> {
        let normal = dim_transform.transform.direction.to_vec();
        let tangent = Vec2::new(-normal.y, normal.x);

        let sweep = delta_seconds * dim_transform.transform.velocity.to_mps();
        dim_transform.dimensions.x += sweep;
        dim_transform.transform.position += normal * (sweep * 0.5);

        // Not worth doing multiple terrain samples for small, slow moving entities.
        if dim_transform.dimensions.x <= SCALE * 0.2 && dim_transform.dimensions.y <= SCALE * 0.2 {
            if let Some(alt) = self.sample(dim_transform.transform.position) {
                return (alt >= threshold).then_some((dim_transform.transform.position, alt));
            }
        }

        // Allow for a small margin of error.
        const GRACE_MARGIN: f32 = 0.9;
        dim_transform.dimensions *= GRACE_MARGIN;

        let dx = (SCALE * 2.0 / 3.0).min(dim_transform.dimensions.x * 0.499);
        let dy = (SCALE * 2.0 / 3.0).min(dim_transform.dimensions.y * 0.499);

        let half_length = (dim_transform.dimensions.x / dx) as i32 / 2;
        let half_width = (dim_transform.dimensions.y / dy) as i32 / 2;

        // Find highest altitude point that we are colliding with.
        let mut max = Altitude::MIN;
        let mut max_position = Vec2::ZERO;

        for l in -(half_length as i32)..=half_length as i32 {
            for w in -(half_width as i32)..=half_width as i32 {
                let l = l as f32 * dx;
                debug_assert!(
                    l > dim_transform.dimensions.x * -0.5,
                    "{} <= {}",
                    l,
                    dim_transform.dimensions.x * -0.5
                );
                debug_assert!(
                    l < dim_transform.dimensions.x * 0.5,
                    "{} >= {}",
                    l,
                    dim_transform.dimensions.x * 0.5
                );
                let w = w as f32 * dy;
                debug_assert!(w > dim_transform.dimensions.y * -0.5);
                debug_assert!(w < dim_transform.dimensions.y * 0.5);
                let pos = dim_transform.transform.position + normal * l + tangent * w;

                if let Some(alt) = self.sample(pos) {
                    if alt >= threshold && alt >= max {
                        max = alt;
                        max_position = pos;
                    }
                }
            }
        }

        (max > Altitude::MIN).then_some((max_position, max))
    }

    /// Returns if there is any land in a square, centered at center. Useful for determining whether something can spawn.
    pub fn land_in_square(&self, center: Vec2, side_length: f32) -> bool {
        let lower_left = Coord::saturating_from_position(center - side_length * 0.5);
        let upper_right = Coord::saturating_from_position(center + side_length * 0.5);

        for x in lower_left.0..upper_right.0 {
            for y in lower_left.1..upper_right.1 {
                if self.at(Coord(x, y)) + 6 > 255 / 2 {
                    return true;
                }
            }
        }
        false
    }

    /// Modifies a small radius around a pos by adding or subtracting an amount of land. Returns
    /// if actually modified terrain, or None if unsuccessful.
    pub fn modify(&mut self, mut mutation: TerrainMutation) -> Option<bool> {
        let pos = mutation.position.mul(1.0 / SCALE);

        let c_pos = pos.ceil();
        let Coord(cx, cy) = Coord::from_scaled_position(c_pos)?;

        let f_pos = pos.floor();
        let Coord(fx, fy) = Coord::from_scaled_position(f_pos)?;

        let fract = pos.sub(f_pos);

        // The following code effectively doubles the amount, so correct for this.
        mutation.amount *= 0.5;

        // Return if actually changed underlying data.
        fn mutate(
            terrain: &mut Terrain,
            x: usize,
            y: usize,
            factor: f32,
            mutation: &TerrainMutation,
        ) -> bool {
            let coord = Coord(x, y);
            let old = terrain.at(coord) + 0b0011;
            let old_altitude = lookup_altitude(old);
            if mutation.condition.contains(&old_altitude) {
                let to_add = (mutation.amount * factor) as i8;
                let new = lookup_altitude(old.saturating_add_signed(to_add));
                let clamped = new.clamp(*mutation.clamp.start(), *mutation.clamp.end());
                //println!("old: {:?}, new: {:?}, clamped: {:?}, reverse: {}", old_altitude, new, clamped, reverse_lookup_altitude(clamped));
                terrain.set(coord, reverse_lookup_altitude(clamped))
            } else {
                false
            }
        }

        let mut modified = false;

        modified |= mutate(self, fx, fy, 2.0 - fract.x - fract.y, &mutation);
        modified |= mutate(self, cx, fy, 1.0 + fract.x - fract.y, &mutation);
        modified |= mutate(self, fx, cy, 1.0 - fract.x + fract.y, &mutation);
        modified |= mutate(self, cx, cy, fract.x + fract.y, &mutation);

        Some(modified)
    }

    /// pre_update is called once before all clients recieve updates after physics each tick.
    pub fn pre_update(&mut self) {
        for chunk in self.chunks.iter_mut().flatten().flatten() {
            // Converts and dedupes updated coords into mods.
            chunk.calculate_mods();
        }
    }

    /// post_update is called once after all clients recieve updates each tick.
    pub fn post_update(&mut self) {
        // Reset updated
        self.updated = ChunkSet::new();

        let now = Instant::now();
        for (cy, chunks) in self.chunks.iter_mut().enumerate() {
            for (cx, chunk) in chunks.iter_mut().enumerate() {
                if let Some(chunk) = chunk {
                    let chunk: &mut Chunk = chunk;

                    // Reset chunk updates (needs to be before regenerate).
                    chunk.update = ChunkUpdate::None;

                    // Regenerate applicable chunks.
                    if let Some(next_regen) = chunk.next_regen {
                        if now >= next_regen {
                            let chunk_id = ChunkId(cx as u16, cy as u16);
                            chunk.regenerate(chunk_id, self.generator); // TODO parallelize

                            chunk.update = ChunkUpdate::Complete;
                            self.updated.add(chunk_id);
                        }
                    }
                }
            }
        }
    }

    /// Clears the update from all chunks that were updated.
    pub fn clear_updated(&mut self) {
        let updated = std::mem::take(&mut self.updated);
        for chunk_id in updated.into_iter() {
            self.mut_chunk(chunk_id).update = ChunkUpdate::None;
        }
    }
}

/// RelativeCoord is a coord within a chunk.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RelativeCoord(pub u8, pub u8);

impl From<Coord> for RelativeCoord {
    fn from(coord: Coord) -> Self {
        Self((coord.0 % CHUNK_SIZE) as u8, (coord.1 % CHUNK_SIZE) as u8)
    }
}

impl RelativeCoord {
    /// into_coord converts into a Coord given a chunk id.
    fn _into_coord(self, chunk_id: ChunkId) -> Coord {
        Coord(
            self.0 as usize + chunk_id.0 as usize * CHUNK_SIZE,
            self.1 as usize + chunk_id.1 as usize * CHUNK_SIZE,
        )
    }

    /// into_absolute_coord is like into_coord, but it assumes chunk is at 0, 0.
    fn into_absolute_coord(self) -> Coord {
        Coord(self.0 as usize, self.1 as usize)
    }
}

impl Add for RelativeCoord {
    type Output = RelativeCoord;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0, self.1 + rhs.1)
    }
}

struct Mod {
    data: u16,
}

impl Mod {
    fn new(coord: RelativeCoord, value: u8) -> Self {
        // Depends on chunk size.
        assert_eq!(CHUNK_SIZE, 64);

        // 6 bits
        let x = coord.0 as u16;
        // 6 bits
        let y = coord.1 as u16;
        // 4 bits
        let real_value = (value >> 4) as u16;

        let m = Self {
            data: x << 10 | y << 4 | real_value,
        };
        debug_assert_eq!(m.to_coord_and_value(), (coord, value));
        m
    }

    fn to_coord_and_value(&self) -> (RelativeCoord, u8) {
        // Depends on chunk size.
        assert_eq!(CHUNK_SIZE, 64);

        let x = (self.data >> 10) as u8;
        let y = ((self.data >> 4) % 64) as u8;
        let amount = ((self.data % 16) as u8) << 4;

        (RelativeCoord(x, y), amount)
    }

    fn to_bytes(&self) -> [u8; 2] {
        self.data.to_le_bytes()
    }

    fn from_bytes(bytes: [u8; 2]) -> Self {
        Self {
            data: u16::from_le_bytes(bytes),
        }
    }
}

impl Default for Terrain {
    fn default() -> Self {
        Self::new()
    }
}

/// ChunkUpdate stores the updates that happened to a chunk during a tick.
enum ChunkUpdate {
    None,                       // No changes.
    Coords(Vec<RelativeCoord>), // For collecting coordinates of modifications.
    Mods(Arc<[u8]>), // Mods encoded as bytes wrapped with Arc for sharing across threads.
    Complete,        // Send whole chunk.
}

impl Default for ChunkUpdate {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SerializedChunk {
    is_update: bool,
    bytes: Arc<[u8]>, // TODO: use serde_bytes.
}

/// A single chunk in a Terrain.
pub struct Chunk {
    data: [[u8; CHUNK_SIZE / 2]; CHUNK_SIZE],
    next_regen: Option<Instant>,
    update: ChunkUpdate,
}

impl Chunk {
    /// Allocates a zero chunk.
    pub fn zero() -> Self {
        Self {
            data: [[0; CHUNK_SIZE / 2]; CHUNK_SIZE],
            next_regen: None,
            update: ChunkUpdate::None,
        }
    }

    /// Generates a new chunk by invoking generator for each pixel.
    pub fn new(chunk_id: ChunkId, generator: Generator) -> Box<Self> {
        // Ensure array is initialized on the heap, not the stack.
        // See https://github.com/rust-lang/rust/issues/28008#issuecomment-135032399
        let mut chunk = box Self::zero();

        let coord = chunk_id.as_coord();
        let x_offset = coord.0;
        let y_offset = coord.1;

        for y in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                chunk.set(Coord(x, y), generator(x + x_offset, y + y_offset));
            }
        }
        chunk
    }

    /// regenerate brings each pixel of the chunk one unit closer to original height.
    pub fn regenerate(&mut self, chunk_id: ChunkId, generator: Generator) {
        let coord = chunk_id.as_coord();
        let x_offset = coord.0;
        let y_offset = coord.1;

        // Whether the regeneration is incomplete (some pixels are still not equal to original values).
        let mut incomplete = false;

        for y in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let coord = Coord(x, y);
                let height = self.at(coord);
                let original_height = generator(x + x_offset, y + y_offset) & 0b11110000;

                let new_height = match original_height.cmp(&height) {
                    std::cmp::Ordering::Less => height - 0b10000,
                    std::cmp::Ordering::Greater => height + 0b10000,
                    std::cmp::Ordering::Equal => continue,
                };

                self.set(coord, new_height);

                if new_height != original_height {
                    incomplete = true;
                }
            }
        }

        self.next_regen = None;

        if incomplete {
            self.mark_for_regenerate();
        }
    }

    /// mark_for_regenerate marks this chunk for regenerating after a standard time delay.
    /// Does nothing if the chunk is already marked as such.
    fn mark_for_regenerate(&mut self) {
        if self.next_regen.is_none() {
            self.next_regen = Some(
                Instant::now()
                    + Duration::from_secs_f32(thread_rng().gen_range(0.75..1.25) * 60.0 * 20.0),
            );
        }
    }

    /// Gets the raw value of one pixel, specified by coord modulo CHUNK_SIZE.
    #[inline]
    pub fn at(&self, coord: Coord) -> u8 {
        let Coord(x, mut y) = coord;
        let sx = (x / 2) % (CHUNK_SIZE / 2);
        y %= CHUNK_SIZE;

        (self.data[y][sx] << ((x & 1) * 4)) & 0b11110000
    }

    /// set_capture captures modifications.
    fn set_capture(&mut self, coord: Coord, value: u8) {
        self.set(coord, value);
        self.mark_for_regenerate();

        self.update = ChunkUpdate::Coords(match &mut self.update {
            ChunkUpdate::None => {
                vec![coord.into()]
            }
            ChunkUpdate::Coords(coords) => {
                coords.push(coord.into());
                return;
            }
            ChunkUpdate::Mods(_) => panic!("mods should have been cleared"),
            ChunkUpdate::Complete => return, // Already sending whole chunk...
        });
    }

    /// calculate_mods converts Coords to Mods.
    fn calculate_mods(&mut self) {
        use std::mem;

        self.update = match mem::take(&mut self.update) {
            ChunkUpdate::None => ChunkUpdate::None,
            ChunkUpdate::Complete => ChunkUpdate::Complete,
            ChunkUpdate::Coords(mut coords) => {
                // Remove duplicates.
                coords.sort_unstable();
                coords.dedup();

                // Not worth doing updates if above this many bytes (average compressed chunk is 2k).
                const MAX_BYTES: usize = 1600;

                if coords.len() * mem::size_of::<Mod>() < MAX_BYTES {
                    ChunkUpdate::Mods(
                        coords
                            .into_iter()
                            .map(|coord| {
                                std::array::IntoIter::new(
                                    Mod::new(coord, self.at(coord.into_absolute_coord()))
                                        .to_bytes(),
                                )
                            })
                            .flatten()
                            .collect(),
                    )
                } else {
                    ChunkUpdate::Complete
                }
            }
            ChunkUpdate::Mods(_) => panic!("mods should have been cleared"),
        }
    }

    /// Sets the raw value of one pixel, specified by coord modulo CHUNK_SIZE.
    fn set(&mut self, coord: Coord, value: u8) {
        let Coord(x, mut y) = coord;
        let sx = (x / 2) % (CHUNK_SIZE / 2);
        y %= CHUNK_SIZE;

        let shift = (x & 1) * 4;
        self.data[y][sx] = (self.data[y][sx] & (0b1111 << shift)) | ((value & 0b11110000) >> shift);
    }

    /// to_bytes encodes a chunk as bytes.
    /// It uses run-length encoding of the chunk mapped to a hilbert curve.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut compressor = Compressor::new(1024);
        for coord in HILBERT_TO_COORD.iter() {
            compressor.write_byte(self.at(coord.into_absolute_coord()));
        }
        compressor.into_vec()
    }

    /// from_bytes decodes bytes encoded with to_bytes into a chunk.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut chunk = Self::zero();
        let hilbert = &HILBERT_TO_COORD;
        for (i, b) in Decompressor::new(bytes).enumerate() {
            chunk.set(hilbert[i].into_absolute_coord(), b);
        }
        chunk
    }

    pub fn to_serialized_chunk(
        &self,
        should_update: bool,
        terrain: &Terrain,
        chunk_id: ChunkId,
    ) -> SerializedChunk {
        if should_update {
            match &self.update {
                ChunkUpdate::None => {
                    debug_assert!(false, "no updates {}", terrain.updated.contains(chunk_id))
                }
                ChunkUpdate::Coords(_) => debug_assert!(false, "coords should have been removed"),
                ChunkUpdate::Mods(mods) => {
                    return SerializedChunk {
                        is_update: true,
                        bytes: Arc::clone(mods),
                    }
                }
                ChunkUpdate::Complete => (),
            }
        }

        // Send whole chunk.
        SerializedChunk {
            is_update: false,
            bytes: self.to_bytes().into(), // TODO could save encoded chunk is lru cache but would require atomics.
        }
    }

    pub fn apply_serialized_chunk(&mut self, serialized: &SerializedChunk) {
        let bytes: &[u8] = &*serialized.bytes;

        self.update = if serialized.is_update {
            // Apply mods and collect coords.
            ChunkUpdate::Coords(
                bytes
                    .array_chunks::<2>()
                    .map(|b| {
                        let m = Mod::from_bytes(*b);
                        let (coord, value) = m.to_coord_and_value();
                        self.set(coord.into_absolute_coord(), value);
                        coord
                    })
                    .collect(),
            )
        } else {
            // Overwrite chunk.
            *self = Self::from_bytes(bytes);
            ChunkUpdate::Complete
        }
    }

    /// Returns an iterator of rects that cover the updated portion of the chunk.
    pub fn updated_rects(&self) -> impl Iterator<Item = (RelativeCoord, RelativeCoord)> {
        let mut iter1 = None;
        let mut iter2 = None;

        match &self.update {
            ChunkUpdate::Coords(coords) => {
                let mut mask = [[false; CHUNK_SIZE + 1]; CHUNK_SIZE + 1];
                for &coord in coords.iter() {
                    for c in [
                        RelativeCoord(0, 0),
                        RelativeCoord(1, 0),
                        RelativeCoord(0, 1),
                        RelativeCoord(1, 1),
                    ]
                    .iter()
                    .map(|&o| coord + o)
                    {
                        // Coords are sorted by x and then y so index in that order.
                        mask[c.0 as usize][c.1 as usize] = true;
                    }
                }

                // Preserve a copy of mask before it's modified in debug mode for assertions.
                #[cfg(debug_assertions)]
                let mask1 = mask;

                // Use greedy meshing algorithm.
                let mut rects = vec![];
                for x in 0..=CHUNK_SIZE {
                    let mut y = 0;

                    while y <= CHUNK_SIZE {
                        if mask[x][y] {
                            let mut maybe_y2 = None;
                            for x2 in x..=(CHUNK_SIZE + 1) {
                                let i = (x2 <= CHUNK_SIZE)
                                    .then(|| {
                                        mask[x2][y..=maybe_y2.unwrap_or(CHUNK_SIZE)]
                                            .iter()
                                            .enumerate()
                                            .take_while(|(_, &v)| v)
                                            .map(|(i, _)| i)
                                            .last()
                                    })
                                    .flatten();

                                if let Some(y2) = maybe_y2 {
                                    if i.map(|i| i + y) != Some(y2) {
                                        rects.push((
                                            RelativeCoord(x as u8, y as u8),
                                            RelativeCoord((x2 - 1) as u8, y2 as u8),
                                        ));
                                        y = y2;
                                        break;
                                    }
                                } else {
                                    if let Some(i) = i {
                                        maybe_y2 = Some(i + y);
                                    } else {
                                        break;
                                    }
                                }

                                mask[x2][y..=(y + i.unwrap())].fill(false);
                            }
                        }

                        y += 1;
                    }
                }

                #[cfg(debug_assertions)]
                {
                    let mut mask2 = [[false; CHUNK_SIZE + 1]; CHUNK_SIZE + 1];
                    for (s, e) in rects.iter().copied() {
                        for x in s.0..=e.0 {
                            for y in s.1..=e.1 {
                                mask2[x as usize][y as usize] = true;
                            }
                        }
                    }

                    fn format_mask(mask: &[[bool; CHUNK_SIZE + 1]; CHUNK_SIZE + 1]) -> String {
                        let mut s = String::new();
                        for x in 0..=CHUNK_SIZE {
                            for y in 0..=CHUNK_SIZE {
                                s.push((b'0' + mask[x][y] as u8) as char);
                            }
                            s.push('\n');
                        }
                        s
                    }

                    if mask1 != mask2 {
                        let mut s = String::from("mask1\n");
                        s += &format_mask(&mask1);
                        s += "mask2\n";
                        s += &format_mask(&mask2);
                        let mut diff = mask1;
                        for x in 0..=CHUNK_SIZE {
                            for y in 0..=CHUNK_SIZE {
                                diff[x][y] = diff[x][y] != mask2[x][y];
                            }
                        }
                        s += "diff\n";
                        s += &format_mask(&diff);
                        panic!("{}", s);
                    }
                }

                iter1 = Some(rects.into_iter());
            }
            ChunkUpdate::Complete => {
                const MAX: u8 = CHUNK_SIZE as u8;
                iter2 = Some((RelativeCoord(0, 0), RelativeCoord(MAX, MAX)))
            }
            _ => panic!("invalid update"),
        }

        iter1.into_iter().flatten().chain(iter2.into_iter())
    }
}

lazy_static! {
    static ref HILBERT_TO_COORD: [RelativeCoord; CHUNK_SIZE * CHUNK_SIZE] = {
        let mut lut = [RelativeCoord(0u8, 0u8); CHUNK_SIZE * CHUNK_SIZE];
        for (i, v) in lut.iter_mut().enumerate() {
            let c = hilbert::h2xy(i as u16);
            *v = RelativeCoord(c.0, c.1);
        }
        lut
    };
}

/// An efficient set of ChunkIds.
#[derive(Clone, Eq, PartialEq)]
pub struct ChunkSet {
    data: [usize; Self::DATA_SIZE],
}

impl ChunkSet {
    const ROW_SIZE: usize = size_of::<usize>();
    const ROW_SIZE_LOG2: u32 = (Self::ROW_SIZE - 1).count_ones();
    const DATA_SIZE: usize = SIZE_CHUNKS.pow(2) / Self::ROW_SIZE;

    pub fn new() -> Self {
        Self {
            data: [0; Self::DATA_SIZE],
        }
    }

    /// Returns the set of all chunks that are within a radius around the center.
    pub fn new_radius(center: Vec2, radius: f32) -> Self {
        let min_chunk_id = ChunkId::saturating_from(center - radius);
        let max_chunk_id = ChunkId::saturating_from(center + radius);

        let mut result = Self::new();

        for y in min_chunk_id.1..=max_chunk_id.1 {
            for x in min_chunk_id.0..=max_chunk_id.0 {
                let chunk_id = ChunkId(x, y);
                if chunk_id.in_radius(center, radius) {
                    result.add(chunk_id);
                }
            }
        }

        result
    }

    /// Returns the set of all chunks that are within a rect around the center.
    pub fn new_rect(center: Vec2, dimensions: Vec2) -> Self {
        let half = dimensions * 0.5;
        let min_chunk_id = ChunkId::saturating_from(center - half);
        let max_chunk_id = ChunkId::saturating_from(center + half);

        let mut result = Self::new();

        for y in min_chunk_id.1..=max_chunk_id.1 {
            for x in min_chunk_id.0..=max_chunk_id.0 {
                // TODO could improve algorithm to write 1 usize.
                result.add(ChunkId(x, y));
            }
        }

        result
    }

    /// Returns true if the set contains a given ChunkId.
    pub fn contains(&self, chunk_id: ChunkId) -> bool {
        self.contains_index(chunk_id.as_index())
    }

    fn contains_index(&self, index: usize) -> bool {
        let row = self.data[index >> Self::ROW_SIZE_LOG2];
        row & 1 << (index % Self::ROW_SIZE) != 0
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::new()
    }

    /// Inserts a given ChunkId into this set.
    pub fn add(&mut self, chunk_id: ChunkId) {
        self.add_index(chunk_id.as_index());
    }

    fn add_index(&mut self, index: usize) {
        let row = &mut self.data[index >> Self::ROW_SIZE_LOG2];
        *row |= 1 << (index % Self::ROW_SIZE);
    }

    /// Iterates all ChunkIds in the set.
    pub fn into_iter(self) -> impl Iterator<Item = ChunkId> {
        (0..Self::DATA_SIZE * Self::ROW_SIZE)
            .filter(move |i| self.contains_index(*i))
            .map(ChunkId::from_index)
    }

    fn unary_op<F: Fn(usize) -> usize>(&self, op: F) -> Self {
        let mut result = Self::new();
        for (i, a) in self.data.iter().enumerate() {
            result.data[i] = op(*a);
        }
        result
    }

    fn binary_op<F: Fn(usize, usize) -> usize>(&self, other: &Self, op: F) -> Self {
        let mut result = Self::new();
        for (i, (a, b)) in self.data.iter().zip(other.data.iter()).enumerate() {
            result.data[i] = op(*a, *b);
        }
        result
    }

    /// Returns the intersection of two sets.
    pub fn and(&self, other: &Self) -> Self {
        self.binary_op(other, |a, b| a & b)
    }

    /// Returns the union of two sets.
    pub fn or(&self, other: &Self) -> Self {
        self.binary_op(other, |a, b| a | b)
    }

    /// Returns the inverse of this set.
    pub fn not(&self) -> Self {
        self.unary_op(|a| !a)
    }
}

impl Default for ChunkSet {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for ChunkSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}",
            self.clone().into_iter().collect::<Vec<ChunkId>>()
        )
    }
}

struct Compressor {
    buf: Vec<u8>,
}

impl Compressor {
    fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    fn write_byte(&mut self, b: u8) {
        const MAX_COUNT: u8 = 16;

        let next = b >> 4;

        let n = self.buf.len();
        if n > 0 {
            let last = &mut self.buf[n - 1];

            let tuple = *last;
            let current = tuple >> 4;
            let count_minus_one = tuple % MAX_COUNT;

            // Add to run length if same nibble and count has more space.
            if next == current && count_minus_one < MAX_COUNT - 1 {
                *last += 1;
                return;
            }
        }
        self.buf.push(next << 4);
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

struct Decompressor<'a> {
    buf: &'a [u8],
    off: usize,
    repeat: u8,
}

impl<'a> Decompressor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            off: 0,
            repeat: 0,
        }
    }
}

impl Iterator for Decompressor<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        const MAX_COUNT: u8 = 16;

        if self.off < self.buf.len() {
            let tuple = self.buf[self.off];
            if tuple % MAX_COUNT != self.repeat {
                self.repeat += 1;
            } else {
                self.off += 1;
                self.repeat = 0;
            }

            Some(tuple & 0b11110000)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{thread_rng, Rng};

    fn random_generator(_: usize, _: usize) -> u8 {
        thread_rng().gen::<u8>() & 0b11110000
    }

    #[test]
    fn altitude() {
        //assert!(lookup_altitude(u8::MAX / 2).is_submerged());
        //assert_eq!(lookup_altitude(u8::MAX / 2 + 1), Altitude::ZERO);

        for i in 0..=u8::MAX {
            println!(
                "i={}, i={:b}, i>>4={}, alt={}, bs={:?}, rev={}",
                i,
                i,
                i >> 4,
                lookup_altitude(i).0,
                ALTITUDE_LUT
                    .binary_search(&lookup_altitude(i).0)
                    .map(|n| n + 1),
                reverse_lookup_altitude(lookup_altitude(i)) >> 4
            );
        }

        for i in 0..u8::MAX {
            assert_eq!(
                i.saturating_add(DATA_OFFSET) >> 4,
                (reverse_lookup_altitude(lookup_altitude(i)) + DATA_OFFSET) >> 4
            );
        }

        // This is a very lenient test (until reverse_altitude_lookup is interpolated).
        for i in i8::MIN..=i8::MAX {
            let a = Altitude(i);
            let r = reverse_lookup_altitude(a);
            let l = lookup_altitude(r);
            let bound = if (-10i8..10i8).contains(&i) { 20 } else { 50 };
            assert!(
                l.difference(a) < Altitude(bound),
                "{:?} -> {} -> {:?}",
                a.0,
                r,
                l.0
            );
        }
    }

    #[test]
    fn mutate() {
        let mut terrain = Terrain::with_generator(zero_generator);

        let pos = Vec2::ZERO;

        assert!(terrain.sample(pos).unwrap() < Altitude(-120));

        terrain.modify(TerrainMutation::simple(pos, 50.0)).unwrap();

        assert!(
            terrain.sample(pos).unwrap() > Altitude(-120),
            "{:?}",
            terrain.sample(pos).unwrap()
        );

        terrain.modify(TerrainMutation::simple(pos, -50.0)).unwrap();

        assert!(
            terrain.sample(pos).unwrap() < Altitude(-60),
            "{:?}",
            terrain.sample(pos).unwrap()
        );

        terrain
            .modify(TerrainMutation::conditional(
                pos,
                50.0,
                Altitude::ZERO..=Altitude::MAX,
            ))
            .unwrap();

        assert!(terrain.sample(pos).unwrap() < Altitude(-60));

        terrain
            .modify(TerrainMutation::conditional(
                pos,
                50.0,
                Altitude::MIN..=Altitude::ZERO,
            ))
            .unwrap();

        assert!(terrain.sample(pos).unwrap() > Altitude(-60));

        terrain
            .modify(TerrainMutation::clamped(
                pos,
                -50.0,
                Altitude(-1)..=Altitude(1),
            ))
            .unwrap();

        assert_eq!(terrain.sample(pos).unwrap(), Altitude(-1));
    }

    #[test]
    fn compress() {
        let mut terrain = Terrain::with_generator(random_generator);
        let chunk = terrain.mut_chunk(ChunkId(0, 0));
        let bytes = chunk.to_bytes();
        println!(
            "random chunk: {} compressed vs {} memory",
            bytes.len(),
            size_of::<Chunk>()
        );
        let chunk2 = Chunk::from_bytes(&bytes);
        assert_eq!(chunk.data, chunk2.data);
    }

    #[test]
    fn updated_rects() {
        let mut chunk = Chunk::new(ChunkId(0, 0), zero_generator);
        let mut rng = thread_rng();

        for _ in 0..1000 {
            let mut coords: Vec<RelativeCoord> = (0..rng.gen_range(0..1000))
                .map(|_| {
                    RelativeCoord(
                        rng.gen_range(0..CHUNK_SIZE as u8),
                        rng.gen_range(0..CHUNK_SIZE as u8),
                    )
                })
                .collect();

            coords.sort_unstable();
            coords.dedup();
            chunk.update = ChunkUpdate::Coords(coords);

            let _ = chunk.updated_rects().collect::<Vec<_>>();
        }

        let coords = (0..CHUNK_SIZE as u8)
            .flat_map(|x| (0..CHUNK_SIZE as u8).map(move |y| RelativeCoord(x, y)))
            .collect();
        chunk.update = ChunkUpdate::Coords(coords);

        assert_eq!(
            chunk.updated_rects().collect::<Vec<_>>(),
            vec![(
                RelativeCoord(0, 0),
                RelativeCoord(CHUNK_SIZE as u8, CHUNK_SIZE as u8)
            )]
        );
    }
}
