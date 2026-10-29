//! Финален файл: точен trim + blur маски, H.264/AAC.

use crate::editor::EditState;
use crate::worker::{command, run_ffmpeg};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub struct ExportStatus {
    pub running: bool,
    pub progress: f32,
    pub message: String,
    pub done: Option<PathBuf>,
}

pub struct ExportJob {
    pub ffmpeg: PathBuf,
    pub src: PathBuf,
    pub out: PathBuf,
    pub width: usize,
    pub height: usize,
    pub fps: f64,
    pub has_audio: bool,
    pub state: EditState,
    pub crf: u32,
}

pub fn run(job: &ExportJob, cancel: &AtomicBool, on_progress: impl Fn(f32)) -> Result<(), String> {
    let s = &job.state;
    let dur = s.out_t - s.in_t;
    let (w, h) = (job.width, job.height);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp_dir = std::env::temp_dir().join(format!("pp_export_{}_{nanos}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Временна папка: {e}"))?;

    let mut cmd = command(&job.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats"]);
    cmd.arg("-ss").arg(format!("{:.4}", s.in_t)).arg("-t").arg(format!("{dur:.4}")).arg("-i").arg(&job.src);

    // Всяка маска: изрязана област → gblur → алфа от PGM маската → overlay обратно.
    let mut parts = Vec::new();
    for (i, m) in s.masks.iter().enumerate() {
        let Some(enable) = enable_expr(&m.spans, s.in_t, s.out_t, job.fps) else { continue };
        let alpha = m.alpha(w, h, 1.0);
        if alpha.rect.is_empty() {
            continue;
        }
        let r = alpha.rect.expand((m.blur * 2.0).ceil() as usize, w, h).even(w, h);
        let mut pgm = format!("P5\n{} {}\n255\n", r.width(), r.height()).into_bytes();
        for y in r.y0..r.y1 {
            pgm.extend(alpha.data[y * w + r.x0..y * w + r.x1].iter().map(|a| (a * 255.0).round().clamp(0.0, 255.0) as u8));
        }
        let file = tmp_dir.join(format!("mask{i}.pgm"));
        std::fs::write(&file, pgm).map_err(|e| format!("Маска: {e}"))?;
        cmd.args(["-loop", "1", "-framerate"]).arg(format!("{}", job.fps)).arg("-t").arg(format!("{dur:.4}")).arg("-i").arg(&file);
        parts.push((r, m.blur, enable));
    }

    let filter = if parts.is_empty() {
        "[0:v]format=yuv420p[v]".to_string()
    } else {
        let mut f = format!("[0:v]split={}[base]", parts.len() + 1);
        for i in 0..parts.len() {
            f += &format!("[c{i}]");
        }
        for (i, (r, sigma, _)) in parts.iter().enumerate() {
            f += &format!(
                ";[c{i}]crop={}:{}:{}:{},gblur=sigma={sigma:.2}:steps=3[b{i}];[{}:v]format=gray[m{i}];[b{i}][m{i}]alphamerge[bm{i}]",
                r.width(),
                r.height(),
                r.x0,
                r.y0,
                i + 1
            );
        }
        let mut prev = "base".to_string();
        for (i, (r, _, enable)) in parts.iter().enumerate() {
            f += &format!(";[{prev}][bm{i}]overlay={}:{}:shortest=1{enable}[o{i}]", r.x0, r.y0);
            prev = format!("o{i}");
        }
        f + &format!(";[{prev}]format=yuv420p[v]")
    };
    cmd.arg("-filter_complex").arg(filter).args(["-map", "[v]"]);
    if job.has_audio {
        cmd.args(["-map", "0:a:0", "-c:a", "aac", "-b:a", "320k"]);
        if let Some(af) = audio_fades(s.fade_in, s.fade_out, dur) {
            cmd.arg("-af").arg(af);
        }
    }
    let mut part = job.out.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf"])
        .arg(job.crf.to_string())
        .args(["-pix_fmt", "yuv420p", "-profile:v", "high", "-movflags", "+faststart", "-f", "mp4"])
        .arg(&part);

    let result = run_ffmpeg(cmd, dur, cancel, on_progress);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if job.out.exists() {
        std::fs::remove_file(&job.out).map_err(|e| format!("Старият файл не може да се замени: {e}"))?;
    }
    std::fs::rename(&part, &job.out).map_err(|e| format!("Преименуване: {e}"))
}

/// `:enable=…` за overlay — само в участъците, когато маската се вижда (времето в експорта започва от IN).
/// Празно — маската е през целия клип; None — не се вижда никъде в IN–OUT.
fn enable_expr(spans: &[[f64; 2]], in_t: f64, out_t: f64, fps: f64) -> Option<String> {
    if spans.is_empty() {
        return Some(String::new());
    }
    // Половин кадър отстъп: кадърът в момента на ръба попада в правилната страна.
    let half = 0.5 / fps;
    let terms: Vec<String> = spans
        .iter()
        .filter(|s| s[1] > in_t && s[0] < out_t && s[1] - s[0] > 1e-3)
        .map(|s| format!("gte(t,{:.4})*lt(t,{:.4})", s[0] - in_t - half, s[1] - in_t - half))
        .collect();
    (!terms.is_empty()).then(|| format!(":enable='{}'", terms.join("+")))
}

/// afade с крива qsin — същата като в прегледа.
fn audio_fades(fade_in: f64, fade_out: f64, dur: f64) -> Option<String> {
    let mut f = Vec::new();
    if fade_in > 0.0 {
        f.push(format!("afade=t=in:st=0:d={:.4}:curve=qsin", fade_in.min(dur)));
    }
    if fade_out > 0.0 {
        let d = fade_out.min(dur);
        f.push(format!("afade=t=out:st={:.4}:d={d:.4}:curve=qsin", dur - d));
    }
    (!f.is_empty()).then(|| f.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Mask;
    use crate::media;
    use crate::worker::{run_bytes, Tools};

    /// Генерира тестов клип, експортира с IN/OUT и маска, проверява дължина и размазване.
    #[test]
    #[ignore]
    fn export_with_mask() {
        let ff = Tools::find().ffmpeg.expect("ffmpeg");
        let dir = std::env::temp_dir().join("pp_export_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let mut cmd = command(&ff);
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=4", "-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&src);
        run_bytes(cmd, &AtomicBool::new(false)).unwrap();

        let info = media::probe(&ff, &src).unwrap();
        assert_eq!((info.width, info.height, info.fps.round() as u32), (1280, 720, 25));
        let audio = media::decode_audio(&ff, &src, 48000).unwrap();
        println!("audio peak_max={} peaks={} samples={}", audio.peak_max, audio.peaks.len(), audio.samples.len());
        assert!(audio.peak_max > 0.05 && audio.peaks.len() > 700);

        // Маска върху лявата половина на кадъра.
        let mask = Mask { kind: crate::editor::Kind::Ellipse, points: vec![[0.05, 0.1], [0.45, 0.1], [0.45, 0.9], [0.05, 0.9]], offset: [0.0, 0.0], scale: 1.0, blur: 30.0, feather: 6.0, spans: vec![[1.0, 2.0]] };
        let out = dir.join("Моят откъс.mp4");
        let job = ExportJob {
            ffmpeg: ff.clone(),
            src: src.clone(),
            out: out.clone(),
            width: 1280,
            height: 720,
            fps: 25.0,
            has_audio: true,
            state: EditState { in_t: 1.0, out_t: 3.0, masks: vec![mask], fade_in: 0.5, fade_out: 0.5, ..Default::default() },
            crf: 16,
        };
        run(&job, &AtomicBool::new(false), |_| {}).unwrap();
        let out_info = media::probe(&ff, &out).unwrap();
        assert!((out_info.duration - 2.0).abs() < 0.08, "duration {}", out_info.duration);
        assert!(out_info.has_audio);

        // Размазаната половина има много по-малко детайл (разлика между съседни пиксели).
        let gray = |path: &std::path::Path, at: &str| {
            let mut c = command(&ff);
            c.args(["-v", "error", "-ss", at, "-i"]).arg(path).args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"]);
            run_bytes(c, &AtomicBool::new(false)).unwrap()
        };
        let detail = |g: &[u8], x0: usize, x1: usize| -> f64 {
            let mut s = 0.0;
            for y in 200..500 {
                for x in x0..x1 {
                    s += (g[y * 1280 + x] as f64 - g[y * 1280 + x + 1] as f64).abs();
                }
            }
            s / ((x1 - x0) * 300) as f64
        };
        // Маската се вижда само от 1 до 2 сек на източника = 0–1 сек в експорта.
        let g = gray(&out, "0.5");
        let (blurred, sharp) = (detail(&g, 200, 500), detail(&g, 800, 1100));
        println!("detail blurred={blurred:.2} sharp={sharp:.2}");
        assert!(blurred * 3.0 < sharp);
        // След скриването лявата половина е същата като в източника (1.5 сек в експорта = 2.5 в източника).
        let hidden = detail(&gray(&out, "1.5"), 200, 500);
        let original = detail(&gray(&src, "2.5"), 200, 500);
        println!("след скриване: {hidden:.2}, източник: {original:.2}");
        assert!((hidden - original).abs() < 0.1 && hidden > blurred * 1.3);

        // Fade in/out по 0.5 сек: краищата са много по-тихи от средата.
        let a = media::decode_audio(&ff, &out, 48000).unwrap();
        let peak = |t0: f64, t1: f64| a.peaks[(t0 / a.peak_secs) as usize..(t1 / a.peak_secs) as usize].iter().copied().fold(0.0, f32::max);
        let (start, mid, end) = (peak(0.0, 0.05), peak(0.9, 1.1), peak(1.95, 1.99));
        println!("звук: начало={start:.3} среда={mid:.3} край={end:.3}");
        assert!(start < mid * 0.25 && end < mid * 0.25);
    }
}
