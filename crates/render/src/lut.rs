//! `.cube` LUTs (Adobe/Resolve format): 1D or 3D, with DOMAIN_MIN/MAX, trilinear interpolation.

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lut {
    /// Entries per axis.
    pub size: usize,
    pub three_d: bool,
    /// Red varies fastest (3D: index = r + g·n + b·n²).
    pub data: Vec<[f32; 3]>,
    pub dmin: [f32; 3],
    pub dmax: [f32; 3],
}

impl Lut {
    pub fn load(path: &Path) -> Result<Lut, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read LUT {}: {e}", path.display()))?;
        Lut::parse(&text).map_err(|e| format!("LUT {} is not a valid .cube file: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Lut, String> {
        let mut size3 = None;
        let mut size1 = None;
        let mut dmin = [0.0f32; 3];
        let mut dmax = [1.0f32; 3];
        let mut data: Vec<[f32; 3]> = Vec::new();
        for (no, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let head = it.next().unwrap_or("");
            let triple = |it: &mut std::str::SplitWhitespace| -> Result<[f32; 3], String> {
                let mut v = [0f32; 3];
                for x in v.iter_mut() {
                    *x = it.next().and_then(|s| s.parse().ok()).ok_or_else(|| format!("line {}: expected three numbers", no + 1))?;
                }
                Ok(v)
            };
            match head.to_ascii_uppercase().as_str() {
                "TITLE" => {}
                "LUT_3D_SIZE" => size3 = it.next().and_then(|s| s.parse::<usize>().ok()),
                "LUT_1D_SIZE" => size1 = it.next().and_then(|s| s.parse::<usize>().ok()),
                "DOMAIN_MIN" => dmin = triple(&mut it)?,
                "DOMAIN_MAX" => dmax = triple(&mut it)?,
                "LUT_3D_INPUT_RANGE" | "LUT_1D_INPUT_RANGE" => {
                    let a: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    let b: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
                    dmin = [a; 3];
                    dmax = [b; 3];
                }
                _ => {
                    if head.parse::<f32>().is_ok() {
                        let mut it2 = line.split_whitespace();
                        data.push(triple(&mut it2)?);
                    }
                    // Unknown keywords are ignored like other tools do.
                }
            }
        }
        let (size, three_d) = match (size3, size1) {
            (Some(n), _) => (n, true),
            (None, Some(n)) => (n, false),
            _ => return Err("missing LUT_3D_SIZE or LUT_1D_SIZE".into()),
        };
        if !(2..=256).contains(&size) {
            return Err(format!("unsupported LUT size {size} (2..256)"));
        }
        let want = if three_d { size * size * size } else { size };
        if data.len() != want {
            return Err(format!("expected {want} entries, found {}", data.len()));
        }
        for k in 0..3 {
            if dmax[k] <= dmin[k] {
                dmax[k] = dmin[k] + 1.0;
            }
        }
        Ok(Lut { size, three_d, data, dmin, dmax })
    }

    /// Maps a straight RGB colour.
    #[inline]
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let nm = (n - 1) as f32;
        let mut u = [0f32; 3];
        for k in 0..3 {
            u[k] = ((rgb[k] - self.dmin[k]) / (self.dmax[k] - self.dmin[k])).clamp(0.0, 1.0) * nm;
        }
        if !self.three_d {
            let mut o = [0f32; 3];
            for k in 0..3 {
                let i = (u[k].floor() as usize).min(n - 2);
                let f = u[k] - i as f32;
                o[k] = self.data[i][k] + (self.data[i + 1][k] - self.data[i][k]) * f;
            }
            return o;
        }
        let i = [(u[0].floor() as usize).min(n - 2), (u[1].floor() as usize).min(n - 2), (u[2].floor() as usize).min(n - 2)];
        let f = [u[0] - i[0] as f32, u[1] - i[1] as f32, u[2] - i[2] as f32];
        let at = |r: usize, g: usize, b: usize| self.data[r + g * n + b * n * n];
        let mut o = [0f32; 3];
        for k in 0..3 {
            let c000 = at(i[0], i[1], i[2])[k];
            let c100 = at(i[0] + 1, i[1], i[2])[k];
            let c010 = at(i[0], i[1] + 1, i[2])[k];
            let c110 = at(i[0] + 1, i[1] + 1, i[2])[k];
            let c001 = at(i[0], i[1], i[2] + 1)[k];
            let c101 = at(i[0] + 1, i[1], i[2] + 1)[k];
            let c011 = at(i[0], i[1] + 1, i[2] + 1)[k];
            let c111 = at(i[0] + 1, i[1] + 1, i[2] + 1)[k];
            let c00 = c000 + (c100 - c000) * f[0];
            let c10 = c010 + (c110 - c010) * f[0];
            let c01 = c001 + (c101 - c001) * f[0];
            let c11 = c011 + (c111 - c011) * f[0];
            let c0 = c00 + (c10 - c00) * f[1];
            let c1 = c01 + (c11 - c01) * f[1];
            o[k] = c0 + (c1 - c0) * f[2];
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_cube(n: usize) -> String {
        let mut s = format!("TITLE \"id\"\nLUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let f = |v: usize| v as f32 / (n - 1) as f32;
                    s += &format!("{} {} {}\n", f(r), f(g), f(b));
                }
            }
        }
        s
    }

    #[test]
    fn identity_and_invert() {
        let l = Lut::parse(&identity_cube(5)).unwrap();
        let o = l.apply([0.3, 0.6, 0.9]);
        assert!((o[0] - 0.3).abs() < 1e-5 && (o[1] - 0.6).abs() < 1e-5 && (o[2] - 0.9).abs() < 1e-5);
        let inv = "LUT_1D_SIZE 2\n1 1 1\n0 0 0\n";
        let l = Lut::parse(inv).unwrap();
        let o = l.apply([0.25, 0.5, 1.0]);
        assert!((o[0] - 0.75).abs() < 1e-6 && (o[2]).abs() < 1e-6);
        assert!(Lut::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
        assert!(Lut::parse("hello").is_err());
    }
}
