//! Проследяване на маска: пикселите под нея (образецът) се търсят във всеки следващ кадър
//! чрез нормализирана кръстосана корелация (ZNCC) — първо грубо в широк прозорец, после прецизно.

use crate::worker::command;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Под този резултат обектът се смята за изгубен.
const LOST: f32 = 0.5;
/// Над този резултат образецът леко се обновява (за бавни промени в осветлението/ъгъла).
const REFRESH: f32 = 0.8;
const MAX_SAMPLES: f32 = 1600.0;

/// Сива картина, един байт на пиксел.
pub struct Gray<'a> {
    pub data: &'a [u8],
    pub w: usize,
    pub h: usize,
}

impl Gray<'_> {
    fn at(&self, x: i32, y: i32) -> f32 {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        self.data[y * self.w + x] as f32
    }

    fn nearest(&self, x: f32, y: f32) -> f32 {
        self.at(x.round() as i32, y.round() as i32)
    }

    fn bilinear(&self, x: f32, y: f32) -> f32 {
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (xi, yi) = (x0 as i32, y0 as i32);
        let (a, b, c, d) = (self.at(xi, yi), self.at(xi + 1, yi), self.at(xi, yi + 1), self.at(xi + 1, yi + 1));
        a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy
    }
}

/// Средата на обекта (пиксели) и мащабът спрямо началния кадър.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub cx: f32,
    pub cy: f32,
    pub scale: f32,
}

pub struct Tracker {
    /// Точки на образеца: отместване от средата (при мащаб 1) и яркост.
    pts: Vec<[f32; 3]>,
    mean: f32,
    norm: f32,
    pub pose: Pose,
    vel: [f32; 2],
    radius: f32,
}

impl Tracker {
    /// Образецът е частта от кадъра вътре в `poly` (пиксели); средата е тази на обхващащата кутия.
    pub fn new(img: &Gray, poly: &[[f32; 2]]) -> Option<Self> {
        let b = poly.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| {
            [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])]
        });
        let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
        if bw < 4.0 || bh < 4.0 {
            return None;
        }
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let step = (bw * bh / MAX_SAMPLES).sqrt().max(1.0);
        let mut pts = Vec::new();
        let mut y = b[1] + step / 2.0;
        while y < b[3] {
            let mut x = b[0] + step / 2.0;
            while x < b[2] {
                let on_screen = x >= 0.0 && y >= 0.0 && x <= (img.w - 1) as f32 && y <= (img.h - 1) as f32;
                if on_screen && inside(poly, [x, y]) {
                    pts.push([x - cx, y - cy, img.bilinear(x, y)]);
                }
                x += step;
            }
            y += step;
        }
        if pts.len() < 16 {
            return None;
        }
        let mut t = Tracker { pts, mean: 0.0, norm: 0.0, pose: Pose { cx, cy, scale: 1.0 }, vel: [0.0, 0.0], radius: (bw.max(bh) * 0.5).clamp(12.0, 64.0) };
        t.restat();
        // Еднакъв цвят (стена, небе) няма какво да се следи.
        (t.norm / t.pts.len() as f32 > 4.0).then_some(t)
    }

    fn restat(&mut self) {
        let n = self.pts.len() as f32;
        self.mean = self.pts.iter().map(|p| p[2]).sum::<f32>() / n;
        self.norm = self.pts.iter().map(|p| (p[2] - self.mean).powi(2)).sum();
    }

    /// ZNCC (−1..1) на образеца, поставен в (cx, cy) с мащаб s.
    fn score(&self, sample: impl Fn(f32, f32) -> f32, cx: f32, cy: f32, s: f32) -> f32 {
        let (mut si, mut sii, mut sit) = (0.0f32, 0.0f32, 0.0f32);
        for p in &self.pts {
            let v = sample(cx + p[0] * s, cy + p[1] * s);
            si += v;
            sii += v * v;
            sit += v * (p[2] - self.mean);
        }
        let n = self.pts.len() as f32;
        let var = sii - si * si / n;
        if var < 1e-3 || self.norm < 1e-3 {
            return 0.0;
        }
        sit / (var * self.norm).sqrt()
    }

    /// Търси обекта в новия кадър. None — изгубен (положението не се променя).
    pub fn step(&mut self, img: &Gray, with_scale: bool) -> Option<(Pose, f32)> {
        let Pose { cx, cy, mut scale } = self.pose;
        let (px, py) = (cx + self.vel[0], cy + self.vel[1]);
        let near = |x: f32, y: f32| img.nearest(x, y);
        let lin = |x: f32, y: f32| img.bilinear(x, y);

        // Грубо: широк прозорец около предвиденото място, през няколко пиксела.
        let r = self.radius.round() as i32;
        let st = (r / 16).max(1);
        let mut best = (f32::MIN, px, py);
        for dy in (-r..=r).step_by(st as usize) {
            for dx in (-r..=r).step_by(st as usize) {
                let (x, y) = (px + dx as f32, py + dy as f32);
                let sc = self.score(near, x, y, scale);
                if sc > best.0 {
                    best = (sc, x, y);
                }
            }
        }
        // Прецизно: всеки пиксел около грубия резултат, после мащабът, после пак мястото.
        let refine = |best: (f32, f32, f32), reach: i32, scale: f32| {
            let mut b = best;
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    let (x, y) = (best.1 + dx as f32, best.2 + dy as f32);
                    let sc = self.score(lin, x, y, scale);
                    if sc > b.0 {
                        b = (sc, x, y);
                    }
                }
            }
            b
        };
        let mut best = refine((self.score(lin, best.1, best.2, scale), best.1, best.2), st, scale);
        if with_scale {
            let mut bs = scale;
            for k in [0.96f32, 0.98, 1.02, 1.04] {
                let sc = self.score(lin, best.1, best.2, scale * k);
                if sc > best.0 {
                    best.0 = sc;
                    bs = scale * k;
                }
            }
            scale = bs.clamp(0.2, 5.0);
            best = refine(best, 1, scale);
        }
        if best.0 < LOST {
            return None;
        }
        // Под пиксела: парабола през съседните резултати.
        let (sc, mut x, mut y) = best;
        let sub = |a: f32, c: f32| {
            let d = a - 2.0 * sc + c;
            if d < -1e-6 { (0.5 * (a - c) / d).clamp(-0.5, 0.5) } else { 0.0 }
        };
        x += sub(self.score(lin, x - 1.0, y, scale), self.score(lin, x + 1.0, y, scale));
        y += sub(self.score(lin, x, y - 1.0, scale), self.score(lin, x, y + 1.0, scale));
        x = x.clamp(0.0, img.w as f32 - 1.0);
        y = y.clamp(0.0, img.h as f32 - 1.0);

        self.vel = [x - cx, y - cy];
        self.pose = Pose { cx: x, cy: y, scale };
        if sc > REFRESH {
            for p in &mut self.pts {
                let v = img.bilinear(x + p[0] * scale, y + p[1] * scale);
                p[2] = 0.95 * p[2] + 0.05 * v;
            }
            self.restat();
        }
        Some((self.pose, sc))
    }
}

fn inside(poly: &[[f32; 2]], p: [f32; 2]) -> bool {
    let mut inside = false;
    let mut j = poly.len().wrapping_sub(1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

pub struct Request {
    pub ffmpeg: PathBuf,
    pub path: PathBuf,
    pub fps: f64,
    /// Началният кадър (секунди от клипа).
    pub t0: f64,
    /// До кой момент (изключително напред, включително назад).
    pub limit: f64,
    pub forward: bool,
    /// Размер на кадрите за проследяване.
    pub w: usize,
    pub h: usize,
    /// Маската в t0, в пиксели при размер w×h.
    pub poly: Vec<[f32; 2]>,
    pub with_scale: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// (момент, положение) — от t0 нататък по посоката на проследяването.
    pub samples: Vec<(f64, Pose)>,
    /// Кадърът, в който обектът е изгубен.
    pub lost_at: Option<f64>,
    pub cancelled: bool,
}

/// ffmpeg, който дава `frames` сиви кадъра w×h от момента `t`.
struct Decoder(Child);

impl Decoder {
    fn start(r: &Request, t: f64, frames: usize) -> Result<Self, String> {
        let mut cmd = command(&r.ffmpeg);
        // Четвърт кадър по-рано: кадърът точно в `t` не се изпуска заради закръгляне.
        let ss = (t - 0.25 / r.fps).max(0.0);
        cmd.args(["-v", "error", "-ss", &format!("{ss:.4}"), "-i"])
            .arg(&r.path)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-frames:v", &frames.to_string()])
            .args(["-vf", &format!("scale={}:{}", r.w, r.h), "-pix_fmt", "gray", "-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd.spawn().map(Decoder).map_err(|e| format!("ffmpeg не може да се стартира: {e}"))
    }

    fn next(&mut self, buf: &mut [u8]) -> bool {
        self.0.stdout.as_mut().is_some_and(|o| o.read_exact(buf).is_ok())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const NO_DETAIL: &str = "Под маската няма достатъчно детайл за проследяване — обхвани обекта по-плътно.";

pub fn run(r: &Request, cancel: &AtomicBool, on_progress: impl Fn(f32)) -> Result<Outcome, String> {
    let fl = 1.0 / r.fps;
    let total = (((r.limit - r.t0).abs() / fl).round() as usize).saturating_sub(usize::from(r.forward));
    let mut buf = vec![0u8; r.w * r.h];
    let mut out = Outcome::default();

    if r.forward {
        let mut dec = Decoder::start(r, r.t0, total + 1)?;
        if !dec.next(&mut buf) {
            return Err("Кадърът не може да се прочете.".into());
        }
        let mut tr = Tracker::new(&Gray { data: &buf, w: r.w, h: r.h }, &r.poly).ok_or(NO_DETAIL)?;
        out.samples.push((r.t0, tr.pose));
        for k in 1..=total {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            if !dec.next(&mut buf) {
                break;
            }
            let t = r.t0 + k as f64 * fl;
            match tr.step(&Gray { data: &buf, w: r.w, h: r.h }, r.with_scale) {
                Some((pose, _)) => out.samples.push((t, pose)),
                None => {
                    out.lost_at = Some(t);
                    break;
                }
            }
            on_progress(k as f32 / total.max(1) as f32);
        }
        return Ok(out);
    }

    // Назад: кадрите се четат на парчета напред и се обхождат отзад напред.
    let mut tr = {
        let mut dec = Decoder::start(r, r.t0, 1)?;
        if !dec.next(&mut buf) {
            return Err("Кадърът не може да се прочете.".into());
        }
        Tracker::new(&Gray { data: &buf, w: r.w, h: r.h }, &r.poly).ok_or(NO_DETAIL)?
    };
    out.samples.push((r.t0, tr.pose));
    const CHUNK: usize = 48;
    let mut done = 0;
    'chunks: while done < total {
        let n = CHUNK.min(total - done);
        let start = r.t0 - (done + n) as f64 * fl;
        let mut dec = Decoder::start(r, start, n)?;
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            if !dec.next(&mut buf) {
                break;
            }
            frames.push(buf.clone());
        }
        drop(dec);
        if frames.len() < n {
            break;
        }
        for (i, f) in frames.iter().enumerate().rev() {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break 'chunks;
            }
            let t = start + i as f64 * fl;
            match tr.step(&Gray { data: f, w: r.w, h: r.h }, r.with_scale) {
                Some((pose, _)) => out.samples.push((t, pose)),
                None => {
                    out.lost_at = Some(t);
                    break 'chunks;
                }
            }
            done += 1;
            on_progress(done as f32 / total.max(1) as f32);
        }
    }
    Ok(out)
}

/// Изглажда пътя (трептенето от кадър на кадър) и оставя само ключовете, без които линейната
/// интерполация би се отклонила с повече от `tol` пиксела (или 0.4% в мащаба). Първата точка не мърда.
pub fn simplify(samples: &[(f64, Pose)], tol: f32) -> Vec<(f64, Pose)> {
    let n = samples.len();
    if n <= 2 {
        return samples.to_vec();
    }
    let smooth: Vec<(f64, Pose)> = (0..n)
        .map(|i| {
            if i == 0 {
                return samples[0];
            }
            let r = 2.min(i).min(n - 1 - i);
            let w = &samples[i - r..=i + r];
            let k = w.len() as f32;
            let avg = |f: fn(&Pose) -> f32| w.iter().map(|s| f(&s.1)).sum::<f32>() / k;
            (samples[i].0, Pose { cx: avg(|p| p.cx), cy: avg(|p| p.cy), scale: avg(|p| p.scale) })
        })
        .collect();
    let fits = |a: usize, b: usize| {
        let (ta, pa) = smooth[a];
        let (tb, pb) = smooth[b];
        smooth[a + 1..b].iter().all(|(t, p)| {
            let u = ((t - ta) / (tb - ta)) as f32;
            let lerp = |x: f32, y: f32| x + (y - x) * u;
            (lerp(pa.cx, pb.cx) - p.cx).abs() <= tol
                && (lerp(pa.cy, pb.cy) - p.cy).abs() <= tol
                && (lerp(pa.scale, pb.scale) - p.scale).abs() <= 0.004
        })
    };
    let mut out = vec![smooth[0]];
    let mut a = 0;
    while a < n - 1 {
        let mut b = a + 1;
        while b + 1 < n && fits(a, b + 1) {
            b += 1;
        }
        out.push(smooth[b]);
        a = b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Шум с текстура; квадрат с друга текстура, който се движи и леко расте.
    fn scene(w: usize, h: usize, cx: f32, cy: f32, half: f32) -> Vec<u8> {
        let hash = |x: i32, y: i32, salt: u32| {
            let mut v = (x as u32).wrapping_mul(73_856_093) ^ (y as u32).wrapping_mul(19_349_663) ^ salt;
            v ^= v >> 13;
            v = v.wrapping_mul(0x5bd1_e995);
            (v >> 24) as u8
        };
        let mut img = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (u, v) = ((x as f32 - cx) / half, (y as f32 - cy) / half);
                img[y * w + x] = if u.abs() < 1.0 && v.abs() < 1.0 {
                    // Обектът носи текстурата си със себе си (координати спрямо него).
                    let (ox, oy) = ((u * 12.0).floor() as i32, (v * 12.0).floor() as i32);
                    hash(ox, oy, 7) / 2 + 100
                } else {
                    hash(x as i32 / 3, y as i32 / 3, 1) / 3
                };
            }
        }
        img
    }

    #[test]
    fn follows_moving_object() {
        let (w, h) = (320, 200);
        let path = |k: usize| (60.0 + k as f32 * 4.5, 80.0 + (k as f32 * 0.3).sin() * 20.0, 24.0 + k as f32 * 0.15);
        let (cx, cy, half) = path(0);
        let first = scene(w, h, cx, cy, half);
        let poly = [[cx - half, cy - half], [cx + half, cy - half], [cx + half, cy + half], [cx - half, cy + half]];
        let mut tr = Tracker::new(&Gray { data: &first, w, h }, &poly).expect("tracker");
        for k in 1..40 {
            let (x, y, hf) = path(k);
            let img = scene(w, h, x, y, hf);
            let (pose, score) = tr.step(&Gray { data: &img, w, h }, true).unwrap_or_else(|| panic!("изгубен на кадър {k}"));
            assert!((pose.cx - x).abs() < 1.5 && (pose.cy - y).abs() < 1.5, "кадър {k}: {pose:?} срещу ({x}, {y}), score {score}");
        }
        let (_, _, hf) = path(39);
        assert!((tr.pose.scale - hf / half).abs() < 0.08, "мащаб {} срещу {}", tr.pose.scale, hf / half);
    }

    #[test]
    fn flat_area_is_rejected_and_object_can_be_lost() {
        let (w, h) = (100, 80);
        let flat = vec![128u8; w * h];
        let poly = [[20.0, 20.0], [50.0, 20.0], [50.0, 50.0], [20.0, 50.0]];
        assert!(Tracker::new(&Gray { data: &flat, w, h }, &poly).is_none());
        let a = scene(w, h, 35.0, 35.0, 15.0);
        let mut tr = Tracker::new(&Gray { data: &a, w, h }, &poly).unwrap();
        // Обектът изчезва → резултатът пада под прага.
        let gone = scene(w, h, -100.0, -100.0, 15.0);
        assert!(tr.step(&Gray { data: &gone, w, h }, false).is_none());
    }

    /// Истинско видео през ffmpeg: обект се движи надясно; проследяване напред и после назад.
    #[test]
    #[ignore]
    fn tracks_real_video() {
        use crate::worker::{run_bytes, Tools};
        let ff = Tools::find().ffmpeg.expect("ffmpeg");
        let dir = std::env::temp_dir().join("pp_track_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let mut cmd = command(&ff);
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=0x406080:s=640x360:r=25:d=3,noise=alls=20:allf=t"])
            .args(["-f", "lavfi", "-i", "testsrc=s=80x80:r=25:d=3"])
            .args(["-filter_complex", "[0][1]overlay=x='60+t*150':y='140+t*10'", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&src);
        run_bytes(cmd, &AtomicBool::new(false)).unwrap();

        // Обектът в 0.4 сек: x 120–200, y 144–224.
        let (x, y) = (60.0 + 0.4 * 150.0, 140.0 + 0.4 * 10.0);
        let req = |t0: f64, limit: f64, forward: bool, at: (f32, f32)| Request {
            ffmpeg: ff.clone(),
            path: src.clone(),
            fps: 25.0,
            t0,
            limit,
            forward,
            w: 640,
            h: 360,
            poly: vec![[at.0, at.1], [at.0 + 80.0, at.1], [at.0 + 80.0, at.1 + 80.0], [at.0, at.1 + 80.0]],
            with_scale: false,
        };
        let started = std::time::Instant::now();
        let out = run(&req(0.4, 2.4, true, (x, y)), &AtomicBool::new(false), |_| {}).unwrap();
        println!("напред: {} кадъра за {:.2} сек, изгубен: {:?}", out.samples.len(), started.elapsed().as_secs_f64(), out.lost_at);
        assert!(out.lost_at.is_none() && out.samples.len() == 50);
        for (t, p) in &out.samples {
            let (ex, ey) = (100.0 + *t as f32 * 150.0, 180.0 + *t as f32 * 10.0);
            assert!((p.cx - ex).abs() < 2.0 && (p.cy - ey).abs() < 2.0, "{t}: {p:?} срещу ({ex}, {ey})");
        }
        // Назад от 2.0 сек до 0.
        let at = (60.0 + 2.0 * 150.0, 140.0 + 2.0 * 10.0);
        let back = run(&req(2.0, 0.0, false, at), &AtomicBool::new(false), |_| {}).unwrap();
        println!("назад: {} кадъра, изгубен: {:?}", back.samples.len(), back.lost_at);
        assert!(back.lost_at.is_none() && back.samples.len() == 51);
        for (t, p) in &back.samples {
            let ex = 100.0 + *t as f32 * 150.0;
            assert!((p.cx - ex).abs() < 2.0, "{t}: {p:?} срещу {ex}");
        }
        assert!(back.samples.last().unwrap().0.abs() < 1e-6);
    }

    #[test]
    fn simplify_keeps_shape_with_fewer_keys() {
        let pose = |cx: f32| Pose { cx, cy: 10.0, scale: 1.0 };
        // Права линия, после завой: остават краищата и ъгълът.
        let s: Vec<(f64, Pose)> = (0..60).map(|i| (i as f64 / 30.0, pose(if i < 30 { i as f32 } else { 30.0 - (i - 30) as f32 }))).collect();
        let k = simplify(&s, 0.5);
        assert!(k.len() <= 6, "{} ключа", k.len());
        assert_eq!(k[0], s[0]);
        assert!((k.last().unwrap().1.cx - 1.0).abs() < 1.0);
        assert!(k.iter().any(|(_, p)| p.cx > 27.0));
    }
}
