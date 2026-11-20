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

/// One adjustable value, with everything the GUI and CLI need to show,
/// parse and clamp it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Saturation,
    Hue,
    Temperature,
    Brightness,
    Contrast,
    Gamma,
    Red,
    Green,
    Blue,
}

impl Field {
    pub const ALL: [Field; 9] = [
        Field::Saturation,
        Field::Hue,
        Field::Temperature,
        Field::Brightness,
        Field::Contrast,
        Field::Gamma,
        Field::Red,
        Field::Green,
        Field::Blue,
    ];

    /// Command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Field::Saturation => "saturation",
            Field::Hue => "hue",
            Field::Temperature => "temperature",
            Field::Brightness => "brightness",
            Field::Contrast => "contrast",
            Field::Gamma => "gamma",
            Field::Red => "red",
            Field::Green => "green",
            Field::Blue => "blue",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::Saturation => "Saturation",
            Field::Hue => "Hue",
            Field::Temperature => "Temperature",
            Field::Brightness => "Brightness",
            Field::Contrast => "Contrast",
            Field::Gamma => "Gamma",
            Field::Red => "Red",
            Field::Green => "Green",
            Field::Blue => "Blue",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Field::Saturation => "Color intensity, 0-300% (100% = stock)",
            Field::Hue => "Hue rotation, -180..180 degrees",
            Field::Temperature => "White point, 2700-10000 K (6500 = stock)",
            Field::Brightness => "Brightness offset, -50..+50",
            Field::Contrast => "Contrast, 50-150%",
            Field::Gamma => "Gamma, 0.40-2.50 (>1 brightens midtones)",
            Field::Red => "Red gain, 50-150%",
            Field::Green => "Green gain, 50-150%",
            Field::Blue => "Blue gain, 50-150%",
        }
    }

    pub fn from_name(s: &str) -> Option<Field> {
        let s = s.to_ascii_lowercase();
        let alias = match s.as_str() {
            "sat" | "vibrance" => "saturation",
            "temp" | "kelvin" => "temperature",
            "bright" => "brightness",
            "r" => "red",
            "g" => "green",
            "b" => "blue",
            other => other,
        };
        Field::ALL.into_iter().find(|f| f.name() == alias)
    }

    pub fn min(self) -> f64 {
        match self {
            Field::Saturation => 0.0,
            Field::Hue => -180.0,
            Field::Temperature => 2700.0,
            Field::Brightness => -0.5,
            Field::Contrast | Field::Red | Field::Green | Field::Blue => 0.5,
            Field::Gamma => 0.4,
        }
    }

    pub fn max(self) -> f64 {
        match self {
            Field::Saturation => 3.0,
            Field::Hue => 180.0,
            Field::Temperature => 10000.0,
            Field::Brightness => 0.5,
            Field::Contrast | Field::Red | Field::Green | Field::Blue => 1.5,
            Field::Gamma => 2.5,
        }
    }

    pub fn step(self) -> f64 {
        match self {
            Field::Hue => 1.0,
            Field::Temperature => 50.0,
            _ => 0.01,
        }
    }

    /// Multiplier between the stored value and what people see/type.
    pub fn scale(self) -> f64 {
        match self {
            Field::Hue | Field::Temperature | Field::Gamma => 1.0,
            _ => 100.0,
        }
    }

    pub fn get(self, a: &Adjust) -> f64 {
        match self {
            Field::Saturation => a.saturation,
            Field::Hue => a.hue,
            Field::Temperature => a.temperature,
            Field::Brightness => a.brightness,
            Field::Contrast => a.contrast,
            Field::Gamma => a.gamma,
            Field::Red => a.red,
            Field::Green => a.green,
            Field::Blue => a.blue,
        }
    }

    pub fn set(self, a: &mut Adjust, v: f64) {
        let v = v.clamp(self.min(), self.max());
        match self {
            Field::Saturation => a.saturation = v,
            Field::Hue => a.hue = v,
            Field::Temperature => a.temperature = v,
            Field::Brightness => a.brightness = v,
            Field::Contrast => a.contrast = v,
            Field::Gamma => a.gamma = v,
            Field::Red => a.red = v,
            Field::Green => a.green = v,
            Field::Blue => a.blue = v,
        }
    }

    pub fn default_value(self) -> f64 {
        self.get(&Adjust::default())
    }

    pub fn snap(self, v: f64) -> f64 {
        ((v / self.step()).round() * self.step()).clamp(self.min(), self.max())
    }

    pub fn format(self, v: f64) -> String {
        match self {
            Field::Hue => format!("{v:+.0}°"),
            Field::Temperature => format!("{v:.0} K"),
            Field::Gamma => format!("{v:.2}"),
            Field::Brightness => format!("{:+.0}", v * 100.0),
            _ => format!("{:.0}%", v * 100.0),
        }
    }

    /// Parse a number in display units ("140", "140%", "-20°", "5000K"),
    /// without clamping.
    pub fn parse_raw(self, text: &str) -> Option<f64> {
        let t: String = text.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
        Some(t.parse::<f64>().ok()? / self.scale())
    }

    /// Parse and clamp to the valid range.
    pub fn parse(self, text: &str) -> Option<f64> {
        self.parse_raw(text).map(|v| v.clamp(self.min(), self.max()))
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
    fn field_parsing() {
        assert_eq!(Field::Saturation.parse("140%"), Some(1.4));
        assert_eq!(Field::Saturation.parse("999"), Some(3.0));
        assert_eq!(Field::Temperature.parse("5000K"), Some(5000.0));
        assert_eq!(Field::Hue.parse("-20°"), Some(-20.0));
        assert_eq!(Field::Brightness.parse_raw("+10"), Some(0.1));
        assert_eq!(Field::from_name("temp"), Some(Field::Temperature));
        for f in Field::ALL {
            assert_eq!(f.parse(&f.format(f.default_value())), Some(f.default_value()));
        }
    }

    #[test]
    fn neutral_temperature() {
        let t = temperature_rgb(6500.0);
        assert!(t.iter().all(|v| (v - 1.0).abs() < 1e-9));
        let w = temperature_rgb(3500.0);
        assert!(w[0] == 1.0 && w[2] < w[1] && w[1] < 1.0);
    }
}
