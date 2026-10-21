//! Растеризиране на ласо маска, feather и Gaussian blur (три последователни box blur-а).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub fn width(self) -> usize {
        self.x1.saturating_sub(self.x0)
    }
    pub fn height(self) -> usize {
        self.y1.saturating_sub(self.y0)
    }
    pub fn is_empty(self) -> bool {
        self.width() == 0 || self.height() == 0
    }
    pub fn expand(self, m: usize, w: usize, h: usize) -> Rect {
        Rect {
            x0: self.x0.saturating_sub(m),
            y0: self.y0.saturating_sub(m),
            x1: (self.x1 + m).min(w),
            y1: (self.y1 + m).min(h),
        }
    }
    /// Четни координати и размери (yuv420p).
    pub fn even(self, w: usize, h: usize) -> Rect {
        Rect { x0: self.x0 & !1, y0: self.y0 & !1, x1: ((self.x1 + 1) & !1).min(w), y1: ((self.y1 + 1) & !1).min(h) }
    }
}

/// Алфа маска (0..1) с размера на кадъра; извън `rect` е 0.
pub struct Alpha {
    pub data: Vec<f32>,
    pub rect: Rect,
}

/// Маска от многоъгълник (в пиксели), с омекотен ръб `feather` (sigma в пиксели).
pub fn mask_alpha(points: &[[f32; 2]], w: usize, h: usize, feather: f32) -> Alpha {
    let mut data = vec![0.0; w * h];
    let filled = fill_polygon(&mut data, w, h, points);
    if filled.is_empty() {
        return Alpha { data, rect: filled };
    }
    let rect = filled.expand((feather * 3.0).ceil() as usize + 1, w, h);
    if feather >= 0.3 {
        let mut buf = extract(&data, w, rect);
        gaussian(&mut buf, rect.width(), rect.height(), feather);
        for (row, y) in (rect.y0..rect.y1).enumerate() {
            let src = &buf[row * rect.width()..(row + 1) * rect.width()];
            data[y * w + rect.x0..y * w + rect.x1].copy_from_slice(src);
        }
    }
    Alpha { data, rect }
}

/// Размазва RGBA кадъра там, където маската е > 0, със сила `sigma` (пиксели).
pub fn apply_blur(frame: &mut [u8], w: usize, h: usize, alpha: &Alpha, sigma: f32) {
    if alpha.rect.is_empty() || sigma < 0.3 {
        return;
    }
    let reg = alpha.rect.expand((sigma * 2.0).ceil() as usize, w, h);
    let (rw, rh) = (reg.width(), reg.height());
    let mut planes: Vec<Vec<f32>> = (0..3).map(|_| Vec::with_capacity(rw * rh)).collect();
    for y in reg.y0..reg.y1 {
        for x in reg.x0..reg.x1 {
            let i = (y * w + x) * 4;
            for (c, plane) in planes.iter_mut().enumerate() {
                plane.push(frame[i + c] as f32);
            }
        }
    }
    for plane in &mut planes {
        gaussian(plane, rw, rh, sigma);
    }
    let r = alpha.rect;
    for y in r.y0..r.y1 {
        for x in r.x0..r.x1 {
            let a = alpha.data[y * w + x];
            if a <= 0.002 {
                continue;
            }
            let i = (y * w + x) * 4;
            let j = (y - reg.y0) * rw + (x - reg.x0);
            for (c, plane) in planes.iter().enumerate() {
                let o = frame[i + c] as f32;
                frame[i + c] = (o + (plane[j] - o) * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

fn extract(data: &[f32], w: usize, r: Rect) -> Vec<f32> {
    (r.y0..r.y1).flat_map(|y| data[y * w + r.x0..y * w + r.x1].iter().copied()).collect()
}

/// Запълва многоъгълника (even-odd) с 1.0; връща обхващащия правоъгълник.
fn fill_polygon(data: &mut [f32], w: usize, h: usize, pts: &[[f32; 2]]) -> Rect {
    let empty = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };
    if pts.len() < 3 {
        return empty;
    }
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        minx = minx.min(p[0]);
        maxx = maxx.max(p[0]);
        miny = miny.min(p[1]);
        maxy = maxy.max(p[1]);
    }
    let clamp = |v: f32, max: usize| (v.max(0.0) as usize).min(max);
    let rect = Rect { x0: clamp(minx.floor(), w), y0: clamp(miny.floor(), h), x1: clamp(maxx.ceil() + 1.0, w), y1: clamp(maxy.ceil() + 1.0, h) };
    if rect.is_empty() {
        return empty;
    }
    let mut xs: Vec<f32> = Vec::new();
    for y in rect.y0..rect.y1 {
        let yc = y as f32 + 0.5;
        xs.clear();
        for i in 0..pts.len() {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            if (a[1] <= yc) != (b[1] <= yc) {
                xs.push(a[0] + (yc - a[1]) * (b[0] - a[0]) / (b[1] - a[1]));
            }
        }
        xs.sort_by(f32::total_cmp);
        for pair in xs.chunks_exact(2) {
            let xa = clamp((pair[0] - 0.5).ceil(), w);
            let xb = clamp((pair[1] - 0.5).ceil(), w);
            data[y * w + xa..y * w + xb].fill(1.0);
        }
    }
    rect
}

/// Приближение на Gaussian blur с три box blur-а (ръбовете се повтарят).
pub fn gaussian(buf: &mut [f32], w: usize, h: usize, sigma: f32) {
    if w == 0 || h == 0 || sigma < 0.3 {
        return;
    }
    let mut tmp = vec![0.0; buf.len()];
    for size in box_sizes(sigma) {
        let r = (size - 1) / 2;
        if r == 0 {
            continue;
        }
        box_h(buf, &mut tmp, w, h, r);
        box_v(&tmp, buf, w, h, r);
    }
}

fn box_sizes(sigma: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let wlf = wl as f32;
    let m = ((12.0 * sigma * sigma - n * wlf * wlf - 4.0 * n * wlf - 3.0 * n) / (-4.0 * wlf - 4.0)).round() as i32;
    [0, 1, 2].map(|i| if i < m { wl as usize } else { wu as usize })
}

fn box_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut dst[y * w..(y + 1) * w];
        let mut acc = (r + 1) as f32 * row[0] + (1..=r).map(|k| row[k.min(w - 1)]).sum::<f32>();
        for x in 0..w {
            out[x] = acc * norm;
            acc += row[(x + r + 1).min(w - 1)] - row[x.saturating_sub(r)];
        }
    }
}

fn box_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for x in 0..w {
        let at = |y: usize| src[y.min(h - 1) * w + x];
        let mut acc = (r + 1) as f32 * at(0) + (1..=r).map(at).sum::<f32>();
        for y in 0..h {
            dst[y * w + x] = acc * norm;
            acc += at(y + r + 1) - at(y.saturating_sub(r));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_mask_and_blur() {
        let (w, h) = (100, 80);
        let a = mask_alpha(&[[20.0, 20.0], [60.0, 20.0], [60.0, 50.0], [20.0, 50.0]], w, h, 0.0);
        assert_eq!(a.data[30 * w + 40], 1.0);
        assert_eq!(a.data[10 * w + 10], 0.0);
        let soft = mask_alpha(&[[20.0, 20.0], [60.0, 20.0], [60.0, 50.0], [20.0, 50.0]], w, h, 4.0);
        let edge = soft.data[35 * w + 20];
        assert!(edge > 0.2 && edge < 0.8, "edge {edge}");

        // Шахматна дъска → след blur в маската става почти равномерно сиво.
        let mut frame: Vec<u8> = (0..w * h).flat_map(|i| {
            let v = if (i % w + i / w) % 2 == 0 { 255 } else { 0 };
            [v, v, v, 255]
        }).collect();
        apply_blur(&mut frame, w, h, &a, 5.0);
        let px = frame[(35 * w + 40) * 4] as i32;
        assert!((px - 128).abs() < 10, "blurred {px}");
        assert!(matches!(frame[(5 * w + 5) * 4], 0 | 255));
    }
}
