//! saturn - a living Saturn for your terminal.
//!
//! Render path: orthographic ray-cast per half-block pixel (2x2 supersampled).
//! Sphere = banded latitude palette + advected value-noise clouds + drifting
//! storm ovals + limb darkening. Rings = piecewise radial profile
//! (C/B/Cassini/A/Encke/F) with Keplerian shear shimmer. Shadows are reciprocal:
//! ring plane shadow on the winter hemisphere, planet shadow notch across the
//! rings. Stars twinkle with color temperature; meteors streak occasionally.
//!
//! Output contract: autowrap off, cursor addressed per row (ESC[r;1H), every row
//! terminated by an explicit black-bg ESC[K (erase uses the current bg SGR —
//! erasing with a star pixel's color or the terminal default paints stripes),
//! zero newlines in a frame, one write() per frame.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::panic;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

type Col = [f64; 3];
const TAU: f64 = std::f64::consts::TAU;
const PI: f64 = std::f64::consts::PI;
/// Equatorial turn in 26 s: cloud drift is obvious within seconds, calm enough
/// to live behind a work terminal for hours.
const ROT: f64 = TAU / 26.0;

// math helpers
#[derive(Clone, Copy)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}
impl V3 {
    fn dot(self, o: V3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    fn add(self, o: V3) -> V3 {
        V3 { x: self.x + o.x, y: self.y + o.y, z: self.z + o.z }
    }
    fn scale(self, s: f64) -> V3 {
        V3 { x: self.x * s, y: self.y * s, z: self.z * s }
    }
    fn cross(self, o: V3) -> V3 {
        V3 {
            x: self.y * o.z - self.z * o.y,
            y: self.z * o.x - self.x * o.z,
            z: self.x * o.y - self.y * o.x,
        }
    }
    fn norm(self) -> V3 {
        let l = self.dot(self).sqrt().max(1e-12);
        self.scale(1.0 / l)
    }
}
fn mix3(a: Col, b: Col, t: f64) -> Col {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}
fn mul3(a: Col, s: f64) -> Col {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn u8c(c: [u8; 3]) -> Col {
    [c[0] as f64, c[1] as f64, c[2] as f64]
}
fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}
fn smooth(a: f64, b: f64, v: f64) -> f64 {
    let t = clamp01((v - a) / (b - a));
    t * t * (3.0 - 2.0 * t)
}

// value noise
fn hash3(ix: i64, iy: i64, iz: i64) -> f64 {
    let mut h = (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (iy as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (iz as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h as f64) / (u64::MAX as f64)
}
fn vnoise3(x: f64, y: f64, z: f64) -> f64 {
    let xi = x.floor() as i64;
    let yi = y.floor() as i64;
    let zi = z.floor() as i64;
    let fx = x - x.floor();
    let fy = y - y.floor();
    let fz = z - z.floor();
    let u = fx * fx * (3.0 - 2.0 * fx);
    let v = fy * fy * (3.0 - 2.0 * fy);
    let w = fz * fz * (3.0 - 2.0 * fz);
    let c = |dx: i64, dy: i64, dz: i64| hash3(xi + dx, yi + dy, zi + dz);
    let x00 = c(0, 0, 0) + (c(1, 0, 0) - c(0, 0, 0)) * u;
    let x10 = c(0, 1, 0) + (c(1, 1, 0) - c(0, 1, 0)) * u;
    let x01 = c(0, 0, 1) + (c(1, 0, 1) - c(0, 0, 1)) * u;
    let x11 = c(0, 1, 1) + (c(1, 1, 1) - c(0, 1, 1)) * u;
    let y0 = x00 + (x10 - x00) * v;
    let y1 = x01 + (x11 - x01) * v;
    y0 + (y1 - y0) * w
}
fn fbm3(x: f64, y: f64, z: f64, oct: u32) -> f64 {
    let (mut amp, mut freq, mut sum, mut norm) = (1.0, 1.0, 0.0, 0.0);
    for _ in 0..oct {
        sum += amp * (vnoise3(x * freq, y * freq, z * freq) * 2.0 - 1.0);
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}

/// Deterministic splitmix64 - starfield and meteors must be identical every run.
struct Rng(u64);
impl Rng {
    fn next_f64(&mut self) -> f64 {
        let mut z = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.0 = z;
        z ^= z >> 30;
        z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z ^= z >> 27;
        z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z as f64) / (u64::MAX as f64)
    }
}

// palette
/// Latitude banding, pale straw -> cream equator -> grey-tan polar hoods.
const BAND_STOPS: [(f64, [u8; 3]); 12] = [
    (-90.0, [132, 120, 100]),
    (-75.0, [143, 130, 108]),
    (-55.0, [168, 150, 120]),
    (-35.0, [197, 173, 133]),
    (-18.0, [215, 191, 147]),
    (-5.0, [228, 208, 164]),
    (8.0, [232, 214, 170]),
    (20.0, [223, 201, 153]),
    (35.0, [204, 181, 139]),
    (55.0, [178, 160, 126]),
    (75.0, [152, 140, 116]),
    (90.0, [138, 127, 106]),
];
fn band_color(lat: f64) -> Col {
    let l = lat.clamp(-90.0, 90.0);
    let mut i = 0usize;
    while i + 2 < BAND_STOPS.len() && BAND_STOPS[i + 1].0 < l {
        i += 1;
    }
    let (l0, c0) = BAND_STOPS[i];
    let (l1, c1) = BAND_STOPS[i + 1];
    let t = clamp01((l - l0) / (l1 - l0));
    let c = mix3(u8c(c0), u8c(c1), t);
    // Fine ringlet banding superimposed on the broad zones.
    mul3(c, 1.0 + 0.030 * (l * 0.55).sin() + 0.018 * (l * 1.35 + 0.8).sin())
}

/// Radial ring profile in planet radii: (r, alpha, tint).
/// D faint -> C translucent straw -> B bright ice -> Cassini dark ->
/// A medium with Encke notch -> F thin outer braid.
const RING_STOPS: [(f64, f64, [u8; 3]); 20] = [
    (1.200, 0.00, [40, 36, 30]),
    (1.240, 0.10, [120, 106, 84]),
    (1.300, 0.22, [150, 132, 100]),
    (1.500, 0.36, [168, 150, 116]),
    (1.525, 0.82, [206, 196, 176]),
    (1.700, 0.92, [224, 216, 198]),
    (1.900, 0.96, [234, 228, 212]),
    (1.950, 0.80, [226, 220, 204]),
    (2.020, 0.16, [86, 80, 70]),
    (2.100, 0.14, [80, 74, 66]),
    (2.125, 0.58, [198, 193, 180]),
    (2.200, 0.70, [208, 203, 189]),
    (2.232, 0.10, [70, 66, 60]),
    (2.246, 0.66, [204, 199, 186]),
    (2.300, 0.56, [196, 191, 178]),
    (2.345, 0.28, [150, 146, 136]),
    (2.360, 0.00, [60, 58, 52]),
    (2.545, 0.00, [60, 58, 52]),
    (2.556, 0.48, [216, 212, 198]),
    (2.568, 0.00, [60, 58, 52]),
];
fn ring_at(r: f64) -> Option<(f64, Col)> {
    if r < RING_STOPS[0].0 || r > RING_STOPS[RING_STOPS.len() - 1].0 {
        return None;
    }
    let mut i = 0usize;
    while i + 2 < RING_STOPS.len() && RING_STOPS[i + 1].0 < r {
        i += 1;
    }
    let (r0, a0, c0) = RING_STOPS[i];
    let (r1, a1, c1) = RING_STOPS[i + 1];
    let t = clamp01((r - r0) / (r1 - r0));
    Some((a0 + (a1 - a0) * t, mix3(u8c(c0), u8c(c1), t)))
}

// camera
struct View {
    right: V3,
    up: V3,
    f: V3,
    sun: V3,
    sx: f64,
    sy: f64,
    cx: f64,
    cy: f64,
    el: f64,
}
/// Ring plane = world xy, spin axis = +z. Motion budget (what the eye reads as
/// "alive" within seconds, without becoming distracting):
///   spin 26 s/turn (differential) · tilt 20±6° / 160 s · roll -9±2.5° / 140 s
///   azimuth ±7° / 240 s (shadow notch travels the rings)
/// `aspect` is the terminal cell height/width ratio: half-block pixels are only
/// square when aspect == 2, so the vertical scale compensates for real fonts.
fn make_view(t: f64, w: usize, hp: usize, aspect: f64) -> View {
    let el = (20.0 + 6.0 * (t * TAU / 160.0).sin()).to_radians();
    let roll = (-9.0 + 2.5 * (t * TAU / 140.0 + 1.0).sin()).to_radians();
    let az = (7.0 * (t * TAU / 240.0).sin()).to_radians();
    let (c, s) = (el.cos(), el.sin());
    let (ca, sa) = (az.cos(), az.sin());
    let f = V3 { x: c * ca, y: c * sa, z: s }.scale(-1.0);
    let r0 = V3 { x: -sa, y: ca, z: 0.0 };
    let u0 = r0.cross(f);
    let (cr, sr) = (roll.cos(), roll.sin());
    let right = r0.scale(cr).add(u0.scale(sr));
    let up = u0.scale(cr).add(r0.scale(-sr));
    // Sun near the view axis: full-disc lighting like the reference, with just
    // enough tilt that the ring shadow hugs the limb instead of eating the belly.
    let sun = f.scale(-0.92).add(up.scale(0.22)).add(right.scale(-0.18)).norm();
    // 5.5 x 3.0 world units of framing box: outer F ring is at r=2.57.
    let sx = 0.94 * (w as f64 / 5.5).min(hp as f64 / 3.0);
    let sy = sx * 2.0 / aspect;
    View { right, up, f, sun, sx, sy, cx: w as f64 * 0.5, cy: hp as f64 * 0.5, el }
}

/// shading
/// (latitude deg, longitude rad at t=0): three pale storm ovals, subtle enough
/// to read as weather rather than stickers, drifting with the spin.
const STORMS: [(f64, f64); 3] = [(-22.0, 0.0), (15.0, 2.1), (-38.0, 4.2)];

fn planet_color(p: V3, vw: &View, t: f64) -> Col {
    let lat = p.z.clamp(-1.0, 1.0).asin().to_degrees();
    let lon = p.y.atan2(p.x);
    // Differential rotation: equator leads, poles lag — clouds and storms shear.
    let omega = ROT * (1.05 - 0.25 * (lat / 90.0).abs());
    let a = lon - omega * t;
    // Noise sampled on the rotated angle keeps the seam at lon=±pi invisible.
    let streak = fbm3(a.cos() * 2.2, a.sin() * 2.2, lat * 0.055 + 31.7, 2);
    let mut base = mul3(band_color(lat), 1.0 + 0.15 * streak);
    let mut g = 0.0;
    for (slat, slon) in STORMS {
        let dl = (lat - slat) / 7.0;
        let mut da = (a - slon).rem_euclid(TAU);
        if da > PI {
            da -= TAU;
        }
        let da = da / 0.38;
        g += (-(dl * dl + da * da)).exp();
    }
    let g = g.clamp(0.0, 1.0);
    base = mix3(base, [246.0, 240.0, 224.0], g * 0.45);
    base = mul3(base, 1.0 + 0.08 * g);
    let mu = clamp01(-p.dot(vw.f));
    let limb = 0.16 + 0.84 * mu.powf(0.45);
    let ndl = clamp01(p.dot(vw.sun));
    let light = 0.38 + 0.62 * ndl.powf(0.8);
    // Ring-plane shadow: march from surface toward sun, test crossing radius.
    // Kept at 45%: full strength reads as a dirty band, not a shadow.
    let mut shf = 1.0;
    if vw.sun.z.abs() > 1e-6 {
        let ss = -p.z / vw.sun.z;
        if ss > 0.0 {
            let qx = p.x + vw.sun.x * ss;
            let qy = p.y + vw.sun.y * ss;
            if let Some((al, _)) = ring_at((qx * qx + qy * qy).sqrt()) {
                shf = 1.0 - 0.45 * al;
            }
        }
    }
    mul3(base, limb * light * shf)
}

fn ring_color(q: V3, r: f64, tint: Col, vw: &View, t: f64) -> Col {
    let th = q.y.atan2(q.x);
    // Kepler: omega ~ r^-1.5. Inner ringlets outrun outer ones => visible shear.
    let om = 0.9 * r.powf(-1.5);
    let a = th - om * t * 0.35;
    let cr = 2.0 + r * 2.5;
    let shim = fbm3(a.cos() * cr, a.sin() * cr, r * 3.1 + 7.7, 2);
    // Ringlets are axisymmetric in reality: radial noise only, no azimuth.
    let rid = 1.0 + 0.055 * (vnoise3(r * 41.0, 7.31, 0.0) * 2.0 - 1.0);
    // Planet shadow: distance from world origin to the sun-ray through q.
    let qs = q.dot(vw.sun);
    let mut shadow = 1.0;
    if qs < 0.0 {
        let dist = (q.dot(q) - qs * qs).max(0.0).sqrt();
        shadow = 0.16 + 0.84 * smooth(0.92, 1.06, dist);
    }
    mul3(tint, rid * (1.0 + 0.10 * shim) * shadow)
}

/// Orthographic ray per sub-pixel: sphere hit, ring-plane hit, depth-compose.
/// Returns (color, solid) where solid occludes starlight and meteors.
fn shade(o: V3, vw: &View, t: f64) -> (Col, bool) {
    let d = vw.f;
    let o2 = o.dot(o);
    let mut sphere: Option<(f64, Col)> = None;
    if o2 < 1.0 {
        let ts = -(1.0 - o2).sqrt();
        let p = o.add(d.scale(ts));
        sphere = Some((ts, planet_color(p, vw, t)));
    }
    if d.z.abs() > 1e-9 {
        let tr = -o.z / d.z;
        let q = o.add(d.scale(tr));
        let r = (q.x * q.x + q.y * q.y).sqrt();
        if let Some((alpha, tint)) = ring_at(r) {
            if alpha > 0.004 {
                let rc = ring_color(q, r, tint, vw, t);
                return match sphere {
                    Some((ts, sc)) if ts < tr => (sc, true), // ring behind globe
                    Some((_, sc)) => (mix3(sc, rc, alpha), true), // ring in front, translucent
                    None => (mul3(rc, alpha), alpha > 0.55), // ring over space
                };
            }
        }
    }
    match sphere {
        Some((_, sc)) => (sc, true),
        None => ([0.0, 0.0, 0.0], false),
    }
}

// stars & meteors
struct Star {
    u: f64,
    v: f64,
    b: f64,
    ph: f64,
    sp: f64,
    tint: Col,
}
fn make_stars(n: usize) -> Vec<Star> {
    let mut rng = Rng(0x5EED_1234);
    (0..n)
        .map(|_| {
            let r = rng.next_f64();
            let pick = rng.next_f64();
            let tint = if pick < 0.3 {
                [0.78, 0.86, 1.0] // blue-white
            } else if pick < 0.8 {
                [1.0, 1.0, 1.0]
            } else {
                [1.0, 0.88, 0.72] // amber
            };
            Star {
                u: rng.next_f64(),
                v: rng.next_f64(),
                b: 0.2 + 0.8 * r * r, // few bright, many faint
                ph: TAU * rng.next_f64(),
                sp: 0.8 + 1.6 * rng.next_f64(),
                tint,
            }
        })
        .collect()
}
fn add_star(pix: &mut [[u8; 3]], occ: &[u8], w: usize, hp: usize, x: i64, y: i64, c: f64, tint: Col) {
    if x < 0 || y < 0 || x >= w as i64 || y >= hp as i64 {
        return;
    }
    let i = (y as usize) * w + x as usize;
    if occ[i] != 0 {
        return; // occulted by globe or opaque ring
    }
    let add = (c * 235.0).clamp(0.0, 255.0);
    let p = &mut pix[i];
    p[0] = (p[0] as f64 + add * tint[0]).min(255.0) as u8;
    p[1] = (p[1] as f64 + add * tint[1]).min(255.0) as u8;
    p[2] = (p[2] as f64 + add * tint[2]).min(255.0) as u8;
}
fn splat_stars(pix: &mut [[u8; 3]], occ: &[u8], w: usize, hp: usize, t: f64, stars: &[Star]) {
    for st in stars {
        let x = (st.u * w as f64) as i64;
        let y = (st.v * hp as f64) as i64;
        let c = st.b * (0.55 + 0.45 * (t * st.sp + st.ph).sin());
        add_star(pix, occ, w, hp, x, y, c, st.tint);
        if st.b > 0.8 {
            add_star(pix, occ, w, hp, x + 1, y, c * 0.35, st.tint);
            add_star(pix, occ, w, hp, x - 1, y, c * 0.35, st.tint);
            add_star(pix, occ, w, hp, x, y + 1, c * 0.35, st.tint);
            add_star(pix, occ, w, hp, x, y - 1, c * 0.35, st.tint);
        }
    }
}

struct Meteor {
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    sp: f64,
    t0: f64,
}
const METEOR_TINT: Col = [0.80, 0.88, 1.0];
/// Head + fading tail, occulted like starlight. Lives 0.9 s.
fn splat_meteor(pix: &mut [[u8; 3]], occ: &[u8], w: usize, hp: usize, t: f64, m: &Meteor) {
    let age = t - m.t0;
    let hx = m.x + m.dx * m.sp * age;
    let hy = m.y + m.dy * m.sp * age;
    let fade = 1.0 - age / 0.9;
    for s in 0..14 {
        let b = (1.0 - s as f64 / 14.0) * fade;
        let x = (hx - m.dx * s as f64 * 1.1) as i64;
        let y = (hy - m.dy * s as f64 * 1.1) as i64;
        add_star(pix, occ, w, hp, x, y, b, METEOR_TINT);
    }
}

fn render_scene(pix: &mut [[u8; 3]], occ: &mut [u8], w: usize, hp: usize, t: f64, vw: &View, stars: &[Star]) {
    // 2x2 supersample: sphere limb and ring edges are the aliasing sources.
    const OFF: [(f64, f64); 4] = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)];
    for y in 0..hp {
        for x in 0..w {
            let mut acc = [0.0f64; 3];
            let mut hits = 0u8;
            for (ox, oy) in OFF {
                let u = (x as f64 + ox - vw.cx) / vw.sx;
                let v = (vw.cy - (y as f64 + oy)) / vw.sy;
                let o = vw.right.scale(u).add(vw.up.scale(v));
                let (c, solid) = shade(o, vw, t);
                acc[0] += c[0];
                acc[1] += c[1];
                acc[2] += c[2];
                if solid {
                    hits += 1;
                }
            }
            let i = y * w + x;
            pix[i] = [
                (acc[0] * 0.25).clamp(0.0, 255.0) as u8,
                (acc[1] * 0.25).clamp(0.0, 255.0) as u8,
                (acc[2] * 0.25).clamp(0.0, 255.0) as u8,
            ];
            occ[i] = u8::from(hits >= 3);
        }
    }
    splat_stars(pix, occ, w, hp, t, stars);
}

// output
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    True,
    X256,
    X16,
}
#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Half,
    Ascii,
}
fn depth_name(d: Depth) -> &'static str {
    match d {
        Depth::True => "truecolor",
        Depth::X256 => "256-color",
        Depth::X16 => "16-color",
    }
}
const RAMP: &[u8; 10] = b" .:-=+*%#@";
const STD16: [[u8; 3]; 16] = [
    [0, 0, 0], [128, 0, 0], [0, 128, 0], [128, 128, 0], [0, 0, 128], [128, 0, 128],
    [0, 128, 128], [192, 192, 192], [128, 128, 128], [255, 0, 0], [0, 255, 0],
    [255, 255, 0], [0, 0, 255], [255, 0, 255], [0, 255, 255], [255, 255, 255],
];
fn q6(v: u8) -> usize {
    const Q: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut bi = 0;
    let mut bd = i32::MAX;
    for (i, q) in Q.iter().enumerate() {
        let d = (v as i32 - *q as i32).abs();
        if d < bd {
            bd = d;
            bi = i;
        }
    }
    bi
}
fn code256(c: [u8; 3]) -> u8 {
    16 + 36 * q6(c[0]) as u8 + 6 * q6(c[1]) as u8 + q6(c[2]) as u8
}
fn code16(c: [u8; 3]) -> usize {
    let mut bi = 0;
    let mut bd = i32::MAX;
    for (i, s) in STD16.iter().enumerate() {
        let d = (c[0] as i32 - s[0] as i32).pow(2)
            + (c[1] as i32 - s[1] as i32).pow(2)
            + (c[2] as i32 - s[2] as i32).pow(2);
        if d < bd {
            bd = d;
            bi = i;
        }
    }
    bi
}
fn push_fg(out: &mut String, c: [u8; 3], d: Depth) {
    match d {
        Depth::True => { let _ = write!(out, "\x1b[38;2;{};{};{}m", c[0], c[1], c[2]); }
        Depth::X256 => { let _ = write!(out, "\x1b[38;5;{}m", code256(c)); }
        Depth::X16 => {
            let i = code16(c);
            let code = if i < 8 { 30 + i } else { 90 + i - 8 };
            let _ = write!(out, "\x1b[{code}m");
        }
    }
}
fn push_bg(out: &mut String, c: [u8; 3], d: Depth) {
    match d {
        Depth::True => { let _ = write!(out, "\x1b[48;2;{};{};{}m", c[0], c[1], c[2]); }
        Depth::X256 => { let _ = write!(out, "\x1b[48;5;{}m", code256(c)); }
        Depth::X16 => {
            let i = code16(c);
            let code = if i < 8 { 40 + i } else { 100 + i - 8 };
            let _ = write!(out, "\x1b[{code}m");
        }
    }
}
fn lum_idx(c: [u8; 3]) -> usize {
    let l = (0.2126 * c[0] as f64 + 0.7152 * c[1] as f64 + 0.0722 * c[2] as f64) / 255.0;
    (l * 9.0 + 0.5).clamp(0.0, 9.0) as usize
}

/// Whole frame into one String. Rows are addressed with CSI H; each row ends by
/// setting an explicit black bg and then CSI K - erase-to-end-of-line paints with
/// the current bg SGR, so erasing with a star pixel's color (or the terminal's
/// default after a reset) used to leave gray/bright stripes. No newlines
/// anywhere: a full-width row followed by LF mis-advances on several terminals.
fn encode(pix: &[[u8; 3]], w: usize, hp: usize, glyph: Glyph, depth: Depth, top_row: usize) -> String {
    let rows = hp / 2;
    let mut out = String::with_capacity(w * rows * 6 + rows * 24 + 64);
    let mut pf: Option<[u8; 3]> = None;
    let mut pb: Option<[u8; 3]> = None;
    for r in 0..rows {
        let _ = write!(out, "\x1b[{};1H", top_row + r);
        for x in 0..w {
            let (fg, bg, g) = match glyph {
                Glyph::Half => {
                    let f = pix[(2 * r) * w + x];
                    (f, pix[(2 * r + 1) * w + x], '▀')
                }
                // ASCII mode downsamples the same 2-row pixel pair: half the
                // glyph noise of rendering at cell resolution directly.
                Glyph::Ascii => {
                    let a = pix[(2 * r) * w + x];
                    let b = pix[(2 * r + 1) * w + x];
                    let c = [
                        ((a[0] as u16 + b[0] as u16) / 2) as u8,
                        ((a[1] as u16 + b[1] as u16) / 2) as u8,
                        ((a[2] as u16 + b[2] as u16) / 2) as u8,
                    ];
                    (c, [0, 0, 0], RAMP[lum_idx(c)] as char)
                }
            };
            if Some(fg) != pf { push_fg(&mut out, fg, depth); pf = Some(fg); }
            if Some(bg) != pb { push_bg(&mut out, bg, depth); pb = Some(bg); }
            out.push(g);
        }
        push_bg(&mut out, [0, 0, 0], depth);
        pb = Some([0, 0, 0]);
        out.push_str("\x1b[K");
    }
    out.push_str("\x1b[0m");
    out
}
/// Monochrome glyph map - the squint-test artifact for the frame gate.
/// Plain rows on purpose: it is meant to be read in an editor, not catted.
fn encode_lum(pix: &[[u8; 3]], w: usize, hp: usize) -> String {
    let mut s = String::with_capacity(w * hp + hp);
    for y in 0..hp {
        for x in 0..w {
            s.push(RAMP[lum_idx(pix[y * w + x])] as char);
        }
        s.push('\n');
    }
    s
}
fn pad_to(s: &mut String, l: &str, n: usize) {
    s.push_str(l);
    let c = l.chars().count();
    for _ in c..n {
        s.push(' ');
    }
}
/// Status line is padded with real spaces on an explicit black bg: no reliance
/// on erase color, no debris when toggling or resizing.
fn status_row(w: usize, t: f64, fps: u32, depth: Depth, el_deg: f64) -> String {
    let mm = (t as u64) / 60;
    let ss = (t as u64) % 60;
    let mut s = String::new();
    push_fg(&mut s, [124, 122, 130], depth); // ~4.9:1 on black
    push_bg(&mut s, [0, 0, 0], depth);
    let txt = format!(
        " saturn · tilt {el_deg:.1}° · t {mm:02}:{ss:02} · {fps} fps · {} · [a]scii [h]ide [q]uit ",
        depth_name(depth)
    );
    if txt.chars().count() >= w {
        s.extend(txt.chars().take(w));
    } else {
        pad_to(&mut s, &txt, w);
    }
    s.push_str("\x1b[0m");
    s
}
// tty lifecycle
fn restore_tty() {
    let _ = io::stdout().write_all(b"\x1b[?7h"); // re-enable autowrap for the shell
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}
fn detect_depth() -> Depth {
    if let Ok(v) = env::var("SATURN_COLOR") {
        match v.as_str() {
            "truecolor" => return Depth::True,
            "256" => return Depth::X256,
            "16" => return Depth::X16,
            _ => {}
        }
    }
    if let Ok(ct) = env::var("COLORTERM") {
        let c = ct.to_lowercase();
        if c.contains("truecolor") || c.contains("24bit") {
            return Depth::True;
        }
    }
    if let Ok(term) = env::var("TERM") {
        if term.contains("truecolor") {
            return Depth::True;
        }
        if term.contains("256") {
            return Depth::X256;
        }
    }
    Depth::X16
}
/// Real cell height/width from the terminal's pixel size when it reports one;
/// 2.0 otherwise (the ratio half-block rendering assumes).
fn detect_aspect() -> f64 {
    match terminal::window_size() {
        Ok(ws) => aspect_from(ws.columns, ws.rows, 2.0),
        Err(_) => 2.0,
    }
}
/// Recompute the cell-aspect after a resize: pixel size comes from
/// `terminal::window_size()`, grid size from the `Resize(cols, rows)` event.
fn aspect_from(cols: u16, rows: u16, fallback: f64) -> f64 {
    if let Ok(ws) = terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && cols > 0 && rows > 0 {
            let cw = ws.width as f64 / cols as f64;
            if cw > 1.0 {
                return ((ws.height as f64 / rows as f64) / cw).clamp(1.4, 2.8);
            }
        }
    }
    fallback
}

fn run(fps: u32, mut ascii: bool, aspect_arg: Option<f64>) -> io::Result<()> {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_tty();
        default_hook(info);
    }));
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    // Autowrap off: a glyph in the last column must never arm a pending wrap.
    stdout.write_all(b"\x1b[?7l\x1b[2J")?;

    let depth = detect_depth();
    let mut aspect = aspect_arg.unwrap_or_else(detect_aspect);
    let stars = make_stars(180);
    let mut rng = Rng(0xDEAD_BEEF);
    let mut meteor: Option<Meteor> = None;
    let mut next_meteor = 5.0 + rng.next_f64() * 8.0;
    let mut pix: Vec<[u8; 3]> = Vec::new();
    let mut occ: Vec<u8> = Vec::new();
    let mut show_status = true;
    let mut t = 0.0f64;
    let frame_dur = Duration::from_secs_f64(1.0 / fps as f64);

    loop {
        let fs = Instant::now();
        let (w, h) = terminal::size()?;
        let (w, h) = (w as usize, h as usize);
        if w < 40 || h < 12 {
            let mut s = String::from("\x1b[2J");
            let _ = write!(s, "\x1b[{};1H", h / 2 + 1);
            push_fg(&mut s, [124, 122, 130], depth);
            push_bg(&mut s, [0, 0, 0], depth);
            pad_to(&mut s, " saturn needs at least 40x12 ", w);
            s.push_str("\x1b[0m");
            stdout.write_all(s.as_bytes())?;
            stdout.flush()?;
        } else {
            let status = show_status && h > 13;
            let hp = 2 * (h - if status { 1 } else { 0 });
            pix.resize(w * hp, [0, 0, 0]);
            occ.resize(w * hp, 0);
            let vw = make_view(t, w, hp, aspect);
            render_scene(&mut pix, &mut occ, w, hp, t, &vw, &stars);
            // Meteor lifecycle: spawn on schedule, live 0.9 s, then gone.
            match meteor {
                Some(ref m) if t - m.t0 < 0.9 => splat_meteor(&mut pix, &occ, w, hp, t, m),
                Some(_) => meteor = None,
                None => {
                    if t > next_meteor {
                        let dir = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
                        let dx = 0.6 + rng.next_f64() * 0.5;
                        let dy = 0.35 + rng.next_f64() * 0.4;
                        let nl = (dx * dx + dy * dy).sqrt();
                        meteor = Some(Meteor {
                            x: rng.next_f64() * w as f64 * 0.8 + w as f64 * 0.1,
                            y: rng.next_f64() * hp as f64 * 0.3,
                            dx: dx / nl * dir,
                            dy: dy / nl,
                            sp: (0.35 + rng.next_f64() * 0.3) * w as f64,
                            t0: t,
                        });
                        next_meteor = t + 14.0 + rng.next_f64() * 22.0;
                        if let Some(m) = &meteor {
                            splat_meteor(&mut pix, &occ, w, hp, t, m);
                        }
                    }
                }
            }
            let mut frame = encode(&pix, w, hp, if ascii { Glyph::Ascii } else { Glyph::Half }, depth, 1);
            if status {
                let _ = write!(frame, "\x1b[{};1H", h);
                frame.push_str(&status_row(w, t, fps, depth, vw.el.to_degrees()));
            }
            stdout.write_all(frame.as_bytes())?;
            stdout.flush()?;
        }
        let wait = frame_dur.saturating_sub(fs.elapsed());
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(k) => {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break;
                    }
                    match k.code {
                        KeyCode::Char('h') => show_status = !show_status,
                        KeyCode::Char('a') => ascii = !ascii,
                        _ => {}
                    }
                }
                Event::Resize(cols, rows) => {
                    aspect = aspect_arg.unwrap_or_else(|| aspect_from(cols, rows, aspect))
                }
                _ => {}
            }
        }
        t += fs.elapsed().as_secs_f64().min(0.25);
    }
    restore_tty();
    Ok(())
}

/// Headless capture: 2 sizes x 3 times, ANSI + monochrome glyph map.
/// This is the runnable path for the frame gate when there is no tty.
fn run_snapshot(prefix: &str) -> io::Result<()> {
    let depth = detect_depth();
    let stars = make_stars(180);
    let sizes = [(80usize, 24usize), (140usize, 45usize)];
    let times = [0.0f64, 45.0, 240.0];
    let mut written = Vec::new();
    for (w, h) in sizes {
        for t in times {
            let hp = 2 * (h - 1); // one cell row reserved for the status line
            let mut pix = vec![[0u8; 3]; w * hp];
            let mut occ = vec![0u8; w * hp];
            let vw = make_view(t, w, hp, 2.0);
            render_scene(&mut pix, &mut occ, w, hp, t, &vw, &stars);
            let base = format!("{prefix}-{w}x{h}-t{t:03.0}");
            let ans = format!("{base}.ans");
            fs::write(&ans, encode(&pix, w, hp, Glyph::Half, depth, 1))?;
            let lum = format!("{base}.lum.txt");
            fs::write(&lum, encode_lum(&pix, w, hp))?;
            written.push(ans);
            written.push(lum);
        }
    }
    for f in &written {
        println!("wrote {f}");
    }
    println!("view: cat {prefix}-140x45-t045.ans   squint: open {prefix}-140x45-t045.lum.txt in an editor");
    Ok(())
}

const HELP: &str = "\
saturn - a living Saturn in your terminal

usage: saturn [--fps N] [--ascii] [--aspect R] [--snapshot PREFIX] [--help]
  --fps N        frame rate 1..60 (default 24)
  --ascii        luminance-glyph mode instead of half-block pixels (live toggle: a)
  --aspect R     cell height/width ratio (default: auto from terminal pixel size,
                 fallback 2.0). Fix for fonts that make the planet an egg.
  --snapshot P   headless: write .ans + .lum.txt frames at 80x24 and 140x45,
                 t = 0/45/240 s, to P-* (P may include a directory). No tty needed.
  keys: q / esc / ctrl-c quit · h hud · a ascii

color depth: $SATURN_COLOR (truecolor|256|16) > $COLORTERM > $TERM.
";

fn main() -> ExitCode {
    let mut fps = 24u32;
    let mut ascii = false;
    let mut aspect: Option<f64> = None;
    let mut snap: Option<String> = None;
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fps" => fps = it.next().and_then(|v| v.parse().ok()).unwrap_or(24).clamp(1, 60),
            "--ascii" => ascii = true,
            "--aspect" => aspect = it.next().and_then(|v| v.parse().ok()).map(|v: f64| v.clamp(1.4, 2.8)),
            "--snapshot" => snap = it.next(),
            "--help" | "-h" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => eprintln!("saturn: unknown arg {other} (see --help)"),
        }
    }
    if let Some(prefix) = snap {
        return match run_snapshot(&prefix) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("saturn: snapshot failed: {e} (does the directory exist?)");
                ExitCode::FAILURE
            }
        };
    }
    match run(fps, ascii, aspect) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            restore_tty();
            eprintln!("saturn: {e}");
            ExitCode::FAILURE
        }
    }
}
