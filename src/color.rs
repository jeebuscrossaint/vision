//! Color math: turns slider values into a 3x3 CTM and a 1D DEGAMMA LUT.
//!
//! The hardware pipeline is: framebuffer → DEGAMMA_LUT → CTM → GAMMA_LUT → panel.
//! wlroots owns GAMMA_LUT (night-light tools use it), so Vision uses the other
//! two stages. Both operate on the gamma-encoded signal, the same way the
//! Windows driver control panels apply their "saturation"/"vibrance" knobs.

use serde::{Deserialize, Serialize};

/// Rec.709 luma weights, used so saturation changes keep perceived brightness.
const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adjust {
    /// 1.0 = unchanged, 0.0 = grayscale, 2.0 = double.
    pub saturation: f64,
    /// Degrees, -180..180.
    pub hue: f64,
    /// Kelvin, 6500 = neutral.
    pub temperature: f64,
    /// Additive offset, -0.5..0.5.
    pub brightness: f64,
    /// 1.0 = unchanged.
    pub contrast: f64,
    /// 1.0 = unchanged; >1 brightens midtones.
    pub gamma: f64,
    /// Per-channel gains, 1.0 = unchanged.
    pub red: f64,
    pub green: f64,
    pub blue: f64,
}

impl Default for Adjust {
    fn default() -> Self {
        Adjust {
            saturation: 1.0,
            hue: 0.0,
            temperature: 6500.0,
            brightness: 0.0,
            contrast: 1.0,
            gamma: 1.0,
            red: 1.0,
            green: 1.0,
            blue: 1.0,
        }
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-4
}

type M3 = [[f64; 3]; 3];

fn mul(a: &M3, b: &M3) -> M3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    r
}

/// RGB multipliers for a black-body-ish white point, normalised so 6500K is
/// neutral and the brightest channel is 1.0 (so whites never clip).
pub fn temperature_rgb(kelvin: f64) -> [f64; 3] {
    fn raw(k: f64) -> [f64; 3] {
        let t = k / 100.0;
        let r = if t <= 66.0 { 255.0 } else { 329.698727446 * (t - 60.0).powf(-0.1332047592) };
        let g = if t <= 66.0 {
            99.4708025861 * t.ln() - 161.1195681661
        } else {
            288.1221695283 * (t - 60.0).powf(-0.0755148492)
        };
        let b = if t >= 66.0 {
            255.0
        } else if t <= 19.0 {
            0.0
        } else {
            138.5177312231 * (t - 10.0).ln() - 305.0447927307
        };
        [r.clamp(0.0, 255.0), g.clamp(0.0, 255.0), b.clamp(0.0, 255.0)]
    }
    let n = raw(6500.0);
    let c = raw(kelvin);
    let v = [c[0] / n[0], c[1] / n[1], c[2] / n[2]];
    let m = v.iter().cloned().fold(f64::MIN, f64::max);
    [v[0] / m, v[1] / m, v[2] / m]
}

impl Adjust {
    pub fn is_identity(&self) -> bool {
        self.matrix_is_identity() && self.tone_is_identity()
    }

    pub fn matrix_is_identity(&self) -> bool {
        near(self.saturation, 1.0)
            && near(self.hue, 0.0)
            && near(self.temperature, 6500.0)
            && near(self.red, 1.0)
            && near(self.green, 1.0)
            && near(self.blue, 1.0)
    }

    pub fn tone_is_identity(&self) -> bool {
        near(self.brightness, 0.0) && near(self.contrast, 1.0) && near(self.gamma, 1.0)
    }

    /// Row-major 3x3 matrix, out = M · in.
    pub fn matrix(&self) -> M3 {
        let s = self.saturation;
        let mut sat = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                sat[i][j] = (1.0 - s) * LUMA[j] + if i == j { s } else { 0.0 };
            }
        }

        // Rotation about the gray (1,1,1) axis.
        let (sn, c) = self.hue.to_radians().sin_cos();
        let a = (1.0 - c) / 3.0;
        let b = sn / 3f64.sqrt();
        let hue = [[c + a, a - b, a + b], [a + b, c + a, a - b], [a - b, a + b, c + a]];

        let t = temperature_rgb(self.temperature);
        let g = [t[0] * self.red, t[1] * self.green, t[2] * self.blue];
        let gains = [[g[0], 0.0, 0.0], [0.0, g[1], 0.0], [0.0, 0.0, g[2]]];

        mul(&gains, &mul(&hue, &sat))
    }

    /// Tone curve applied per channel in the DEGAMMA stage.
    pub fn tone(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0).powf(1.0 / self.gamma.max(0.05));
        ((x - 0.5) * self.contrast + 0.5 + self.brightness).clamp(0.0, 1.0)
    }

    /// `struct drm_color_ctm`: nine S31.32 sign-magnitude fixed-point values.
    pub fn ctm_blob(&self) -> Vec<u8> {
        let m = self.matrix();
        let mut out = Vec::with_capacity(72);
        for row in m {
            for v in row {
                let mag = ((v.abs() * 4294967296.0).round() as u64) & 0x7fff_ffff_ffff_ffff;
                let raw = if v < 0.0 { mag | (1 << 63) } else { mag };
                out.extend_from_slice(&raw.to_le_bytes());
            }
        }
        out
    }

    /// `struct drm_color_lut[size]`: u16 red, green, blue, reserved.
    pub fn degamma_blob(&self, size: u32) -> Vec<u8> {
        let n = size.max(2);
        let mut out = Vec::with_capacity(n as usize * 8);
        for i in 0..n {
            let y = self.tone(i as f64 / (n - 1) as f64);
            let v = (y * 65535.0).round() as u16;
            for _ in 0..3 {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matrix() {
        let m = Adjust::default().matrix();
        for i in 0..3 {
            for j in 0..3 {
                assert!((m[i][j] - if i == j { 1.0 } else { 0.0 }).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn saturation_keeps_gray() {
        let a = Adjust { saturation: 1.8, hue: 40.0, ..Default::default() };
        let m = a.matrix();
        for row in m {
            assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn ctm_encoding() {
        let a = Adjust { saturation: 0.0, ..Default::default() };
        let b = a.ctm_blob();
        assert_eq!(b.len(), 72);
        let first = u64::from_le_bytes(b[0..8].try_into().unwrap());
        assert_eq!(first, (0.2126f64 * 4294967296.0).round() as u64);
        let neg = Adjust { saturation: 2.0, ..Default::default() }.ctm_blob();
        let m01 = u64::from_le_bytes(neg[8..16].try_into().unwrap());
        assert!(m01 >> 63 == 1);
    }

    #[test]
    fn neutral_temperature() {
        let t = temperature_rgb(6500.0);
        assert!(t.iter().all(|v| (v - 1.0).abs() < 1e-9));
        let w = temperature_rgb(3500.0);
        assert!(w[0] == 1.0 && w[2] < w[1] && w[1] < 1.0);
    }
}
