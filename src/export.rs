//! Финален файл: точен trim + blur маски, H.264/AAC.

use crate::blur::{self, Alpha};
use crate::editor::EditState;
use crate::worker::{command, run_ffmpeg, CANCELLED};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

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
    let mut part = job.out.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let s = &job.state;
    // Движещите се маски не стават с неподвижен crop/overlay — тогава кадрите минават през програмата.
    let moving = s.masks.iter().any(|m| !m.keys.is_empty() && enable_expr(&m.spans, s.in_t, s.out_t, job.fps).is_some());
    let result = if moving { encode_frames(job, &part, cancel, on_progress) } else { encode_filter(job, &part, cancel, on_progress) };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if job.out.exists() {
        std::fs::remove_file(&job.out).map_err(|e| format!("Старият файл не може да се замени: {e}"))?;
    }
    std::fs::rename(&part, &job.out).map_err(|e| format!("Преименуване: {e}"))
}

/// Неподвижни маски: всичко във ffmpeg (crop → gblur → alphamerge → overlay).
fn encode_filter(job: &ExportJob, part: &Path, cancel: &AtomicBool, on_progress: impl Fn(f32)) -> Result<(), String> {
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
        cmd.args(["-map", "0:a:0"]);
    }
    output_args(&mut cmd, job, part);
    let result = run_ffmpeg(cmd, dur, cancel, on_progress);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    result
}

/// Звук (AAC с fade-овете), H.264 и изходният файл — еднакви за двата начина.
fn output_args(cmd: &mut std::process::Command, job: &ExportJob, part: &Path) {
    let s = &job.state;
    if job.has_audio {
        cmd.args(["-c:a", "aac", "-b:a", "320k"]);
        if let Some(af) = audio_fades(s.fade_in, s.fade_out, s.out_t - s.in_t) {
            cmd.arg("-af").arg(af);
        }
    }
    cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf"])
        .arg(job.crf.to_string())
        .args(["-pix_fmt", "yuv420p", "-profile:v", "high", "-movflags", "+faststart", "-f", "mp4"])
        .arg(part);
}

/// Процес, който се спира при излизане (грешка, отказ).
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(mut cmd: std::process::Command) -> Result<(Proc, std::thread::JoinHandle<String>), String> {
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg не може да се стартира: {e}"))?;
    let mut err = child.stderr.take().unwrap();
    let t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        String::from_utf8_lossy(&b).into_owned()
    });
    Ok((Proc(child), t))
}

fn last_lines(text: &str) -> String {
    let v: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    v[v.len().saturating_sub(10)..].join("\n")
}

/// Маските с движение: ffmpeg декодира към суров yuv420p, тук всяка маска се размазва в момента на
/// кадъра (Y в пълен размер, U и V в половин), друг ffmpeg кодира. Пикселите извън маските не се пипат.
fn encode_frames(job: &ExportJob, part: &Path, cancel: &AtomicBool, on_progress: impl Fn(f32)) -> Result<(), String> {
    let s = &job.state;
    let dur = s.out_t - s.in_t;
    let (w, h) = (job.width, job.height);
    let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
    let (ysz, csz) = (w * h, cw * ch);
    let frames = ((dur * job.fps).round() as usize).max(1);
    let fps = format!("{}", job.fps);

    let mut dec = command(&job.ffmpeg);
    dec.args(["-hide_banner", "-nostdin", "-v", "error", "-ss"])
        .arg(format!("{:.4}", s.in_t))
        .arg("-t")
        .arg(format!("{dur:.4}"))
        .arg("-i")
        .arg(&job.src)
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-r", &fps, "-frames:v", &frames.to_string()])
        .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut enc = command(&job.ffmpeg);
    enc.args(["-hide_banner", "-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &format!("{w}x{h}")])
        .args(["-framerate", &fps, "-i", "-"]);
    if job.has_audio {
        enc.arg("-ss").arg(format!("{:.4}", s.in_t)).arg("-t").arg(format!("{dur:.4}")).arg("-i").arg(&job.src);
        enc.args(["-map", "0:v", "-map", "1:a:0"]);
    } else {
        enc.args(["-map", "0:v"]);
    }
    output_args(&mut enc, job, part);
    enc.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());

    let (mut dec, dec_err) = spawn(dec)?;
    let (mut enc, enc_err) = spawn(enc)?;
    let mut src = dec.0.stdout.take().unwrap();
    let mut sink = enc.0.stdin.take();

    // По маска: контурът в пиксели и алфата за него (Y и UV), за да не се смятат наново, ако не мърда.
    let mut cache: Vec<Option<(Vec<[f32; 2]>, Alpha, Alpha)>> = (0..s.masks.len()).map(|_| None).collect();
    let mut buf = vec![0u8; ysz + 2 * csz];
    let mut written = 0;
    for n in 0..frames {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        if src.read_exact(&mut buf).is_err() {
            break;
        }
        let t = s.in_t + n as f64 / job.fps;
        for (m, slot) in s.masks.iter().zip(cache.iter_mut()) {
            if !m.active_at(t) {
                continue;
            }
            let eff = m.effective_at(t);
            if slot.as_ref().map_or(true, |c| c.0 != eff) {
                let px = |w: usize, h: usize| eff.iter().map(|p| [p[0] * w as f32, p[1] * h as f32]).collect::<Vec<_>>();
                let ya = blur::mask_alpha(&px(w, h), w, h, m.feather);
                let ca = blur::mask_alpha(&px(cw, ch), cw, ch, m.feather * cw as f32 / w as f32);
                *slot = Some((eff, ya, ca));
            }
            let Some((_, ya, ca)) = slot else { continue };
            let (y, uv) = buf.split_at_mut(ysz);
            let (u, v) = uv.split_at_mut(csz);
            let cs = m.blur * cw as f32 / w as f32;
            std::thread::scope(|sc| {
                sc.spawn(|| blur::blur_plane(y, w, h, ya, m.blur));
                sc.spawn(|| blur::blur_plane(u, cw, ch, ca, cs));
                blur::blur_plane(v, cw, ch, ca, cs);
            });
        }
        if sink.as_mut().map_or(true, |p| p.write_all(&buf).is_err()) {
            break;
        }
        written += 1;
        on_progress(written as f32 / frames as f32);
    }
    drop(sink);
    drop(src);
    let dec_status = dec.0.wait();
    let status = enc.0.wait().map_err(|e| e.to_string())?;
    let enc_err = enc_err.join().unwrap_or_default();
    let dec_err = dec_err.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("ffmpeg: {}", last_lines(&enc_err)));
    }
    if written == 0 {
        let why = if dec_status.is_ok_and(|s| s.success()) { "няма кадри".to_string() } else { last_lines(&dec_err) };
        return Err(format!("ffmpeg (декодиране): {why}"));
    }
    Ok(())
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
        let mask = Mask { kind: crate::editor::Kind::Ellipse, points: vec![[0.05, 0.1], [0.45, 0.1], [0.45, 0.9], [0.05, 0.9]], offset: [0.0, 0.0], scale: 1.0, blur: 30.0, feather: 6.0, spans: vec![[1.0, 2.0]], keys: Vec::new() };
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

    /// Маска с ключове: размазването тръгва отляво и стига вдясно; дължина и звук се пазят.
    #[test]
    #[ignore]
    fn export_moving_mask() {
        use crate::editor::{Keyframe, Kind};
        let ff = Tools::find().ffmpeg.expect("ffmpeg");
        let dir = std::env::temp_dir().join("pp_export_moving_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let mut cmd = command(&ff);
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=3,noise=alls=40:allf=t", "-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&src);
        run_bytes(cmd, &AtomicBool::new(false)).unwrap();

        let mask = Mask {
            kind: Kind::Rect,
            points: vec![[0.05, 0.1], [0.35, 0.1], [0.35, 0.9], [0.05, 0.9]],
            offset: [0.0, 0.0],
            scale: 1.0,
            blur: 30.0,
            feather: 4.0,
            spans: Vec::new(),
            keys: vec![Keyframe { t: 0.5, offset: [0.0, 0.0], scale: 1.0 }, Keyframe { t: 2.5, offset: [0.55, 0.0], scale: 1.0 }],
        };
        let out = dir.join("moving.mp4");
        let job = ExportJob {
            ffmpeg: ff.clone(),
            src: src.clone(),
            out: out.clone(),
            width: 1280,
            height: 720,
            fps: 25.0,
            has_audio: true,
            state: EditState { in_t: 0.5, out_t: 2.5, masks: vec![mask], ..Default::default() },
            crf: 16,
        };
        let started = std::time::Instant::now();
        run(&job, &AtomicBool::new(false), |_| {}).unwrap();
        println!("експорт: {:.2} сек", started.elapsed().as_secs_f64());
        let info = media::probe(&ff, &out).unwrap();
        assert!((info.duration - 2.0).abs() < 0.08, "duration {}", info.duration);
        assert!(info.has_audio);

        let gray = |path: &std::path::Path, at: &str| {
            let mut c = command(&ff);
            c.args(["-v", "error", "-ss", at, "-i"]).arg(path).args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"]);
            run_bytes(c, &AtomicBool::new(false)).unwrap()
        };
        let detail = |g: &[u8], x0: usize, x1: usize| -> f64 {
            let mut s = 0.0;
            for y in 150..550 {
                for x in x0..x1 {
                    s += (g[y * 1280 + x] as f64 - g[y * 1280 + x + 1] as f64).abs();
                }
            }
            s / ((x1 - x0) * 400) as f64
        };
        // Спрямо същия кадър от източника (експортът започва от 0.5 сек):
        // в началото маската е вляво (x 64–448), накрая — вдясно (x ≈ 730–1110).
        for (at, src_at, blurred, sharp) in [("0.04", "0.54", (150, 400), (800, 1050)), ("1.96", "2.46", (800, 1050), (150, 400))] {
            let (o, i) = (gray(&out, at), gray(&src, src_at));
            let (ob, ib) = (detail(&o, blurred.0, blurred.1), detail(&i, blurred.0, blurred.1));
            let (os, is) = (detail(&o, sharp.0, sharp.1), detail(&i, sharp.0, sharp.1));
            println!("{at}: под маската {ob:.2} (източник {ib:.2}), извън {os:.2} (източник {is:.2})");
            assert!(ob < ib * 0.35, "под маската не е размазано");
            assert!((os - is).abs() < is * 0.15, "извън маската е променено");
        }
    }
}
