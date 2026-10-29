//! Декодиране за прегледа: кадри и звук през ffmpeg, звуков изход през cpal.

use crate::worker::{command, run_bytes, run_capture};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub struct Info {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub has_audio: bool,
}

pub fn probe(ffmpeg: &Path, path: &Path) -> Result<Info, String> {
    let mut cmd = command(&ffmpeg.with_file_name("ffprobe.exe"));
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type,width,height,avg_frame_rate,r_frame_rate:format=duration",
        "-of",
        "json",
    ])
    .arg(path);
    let v: Value = serde_json::from_str(&run_capture(cmd, &AtomicBool::new(false))?).map_err(|e| format!("ffprobe: {e}"))?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video").ok_or("Във файла няма видео")?;
    let rate = |key: &str| video[key].as_str().and_then(parse_rate).filter(|f| (1.0..=240.0).contains(f));
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .ok_or("Неизвестна дължина на видеото")?;
    Ok(Info {
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
        fps: rate("avg_frame_rate").or_else(|| rate("r_frame_rate")).unwrap_or(25.0),
        duration,
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    })
}

fn parse_rate(s: &str) -> Option<f64> {
    let (a, b) = s.split_once('/')?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (b > 0.0).then(|| a / b)
}

/// Целият звук на клипа (стерео f32) + върхове за waveform.
pub struct AudioData {
    pub samples: Arc<Vec<f32>>,
    pub peaks: Vec<f32>,
    pub peak_max: f32,
    /// Колко секунди покрива един връх.
    pub peak_secs: f64,
}

pub fn decode_audio(ffmpeg: &Path, path: &Path, rate: u32) -> Result<AudioData, String> {
    let mut cmd = command(ffmpeg);
    cmd.args(["-v", "error", "-i"]).arg(path).args(["-vn", "-ac", "2", "-ar", &rate.to_string(), "-f", "f32le", "-"]);
    let bytes = run_bytes(cmd, &AtomicBool::new(false))?;
    let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let frames_per_peak = (rate as usize / 200).max(1);
    let peaks: Vec<f32> =
        samples.chunks(frames_per_peak * 2).map(|c| c.iter().fold(0f32, |m, s| m.max(s.abs()))).collect();
    let peak_max = peaks.iter().copied().fold(0.0, f32::max).max(1e-4);
    Ok(AudioData { samples: Arc::new(samples), peaks, peak_max, peak_secs: frames_per_peak as f64 / rate as f64 })
}

pub struct Frame {
    pub t: f64,
    pub rgba: Vec<u8>,
}

fn frame_cmd(ffmpeg: &Path, path: &Path, t: f64, w: usize, h: usize, single: bool) -> Command {
    let mut cmd = command(ffmpeg);
    cmd.args(["-v", "error", "-ss", &format!("{t:.4}"), "-i"]).arg(path);
    if single {
        cmd.args(["-frames:v", "1"]);
    }
    cmd.args(["-an", "-sn", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    cmd
}

/// Непрекъснато декодиране от момент `t0` нататък — за пускане.
pub struct FrameStream {
    pub rx: Receiver<Frame>,
    child: Option<Child>,
}

impl FrameStream {
    pub fn start(ffmpeg: &Path, path: &Path, t0: f64, w: usize, h: usize, fps: f64) -> Self {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut cmd = frame_cmd(ffmpeg, path, t0, w, h, false);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let child = cmd.spawn().ok().map(|mut child| {
            let mut out = child.stdout.take().unwrap();
            thread::spawn(move || {
                for n in 0u64.. {
                    let mut buf = vec![0u8; w * h * 4];
                    if out.read_exact(&mut buf).is_err() || tx.send(Frame { t: t0 + n as f64 / fps, rgba: buf }).is_err() {
                        break;
                    }
                }
            });
            child
        });
        FrameStream { rx, child }
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Единични кадри при пауза/превъртане; винаги се изпълнява само последната заявка.
pub struct FrameFetcher {
    req: Arc<(Mutex<(Option<f64>, bool)>, Condvar)>,
    pub rx: Receiver<Frame>,
}

impl FrameFetcher {
    pub fn new(ffmpeg: PathBuf, path: PathBuf, w: usize, h: usize, ctx: eframe::egui::Context) -> Self {
        let req = Arc::new((Mutex::new((None, false)), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let r = req.clone();
        thread::spawn(move || loop {
            let t = {
                let (lock, cv) = &*r;
                let mut g = lock.lock().unwrap();
                while g.0.is_none() && !g.1 {
                    g = cv.wait(g).unwrap();
                }
                if g.1 {
                    return;
                }
                g.0.take().unwrap()
            };
            let cmd = frame_cmd(&ffmpeg, &path, t, w, h, true);
            if let Ok(buf) = run_bytes(cmd, &AtomicBool::new(false)) {
                if buf.len() == w * h * 4 {
                    if tx.send(Frame { t, rgba: buf }).is_err() {
                        return;
                    }
                    ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
                }
            }
        });
        FrameFetcher { req, rx }
    }

    pub fn request(&self, t: f64) {
        let (lock, cv) = &*self.req;
        lock.lock().unwrap().0 = Some(t);
        cv.notify_one();
    }
}

impl Drop for FrameFetcher {
    fn drop(&mut self) {
        let (lock, cv) = &*self.req;
        lock.lock().unwrap().1 = true;
        cv.notify_one();
    }
}

pub struct AudioShared {
    samples: Mutex<Arc<Vec<f32>>>,
    pos: AtomicU64,
    seek: AtomicI64,
    playing: AtomicBool,
    /// IN, OUT, fade in, fade out (секунди, f64 битове) — за fade-а при пускане.
    fades: [AtomicU64; 4],
}

impl AudioShared {
    pub fn set_samples(&self, s: Arc<Vec<f32>>) {
        *self.samples.lock().unwrap() = s;
    }

    pub fn set_fades(&self, in_t: f64, out_t: f64, fade_in: f64, fade_out: f64) {
        for (a, v) in self.fades.iter().zip([in_t, out_t, fade_in, fade_out]) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
    }

    fn fades(&self) -> [f64; 4] {
        self.fades.each_ref().map(|a| f64::from_bits(a.load(Ordering::Relaxed)))
    }
}

/// Звуков изход; позицията му е часовникът при пускане.
pub struct AudioOut {
    pub rate: u32,
    pub shared: Arc<AudioShared>,
    quit: Arc<AtomicBool>,
}

impl AudioOut {
    pub fn new() -> Option<Self> {
        let shared = Arc::new(AudioShared {
            samples: Mutex::new(Arc::new(Vec::new())),
            pos: AtomicU64::new(0),
            seek: AtomicI64::new(-1),
            playing: AtomicBool::new(false),
            fades: Default::default(),
        });
        let quit = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (sh, q) = (shared.clone(), quit.clone());
        // Потокът живее в собствена нишка (COM/WASAPI не зависят от прозореца).
        thread::spawn(move || {
            let Some((stream, rate)) = open_stream(sh) else {
                let _ = tx.send(None);
                return;
            };
            let _ = tx.send(Some(rate));
            while !q.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
        });
        let rate = rx.recv().ok().flatten()?;
        Some(AudioOut { rate, shared, quit })
    }

    pub fn play_from(&self, t: f64) {
        self.shared.seek.store((t.max(0.0) * self.rate as f64).round() as i64, Ordering::SeqCst);
        self.shared.playing.store(true, Ordering::SeqCst);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::SeqCst);
    }

    pub fn time(&self) -> f64 {
        let seek = self.shared.seek.load(Ordering::SeqCst);
        let frames = if seek >= 0 { seek as u64 } else { self.shared.pos.load(Ordering::SeqCst) };
        frames as f64 / self.rate as f64
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.pause();
        self.quit.store(true, Ordering::Relaxed);
    }
}

fn open_stream(sh: Arc<AudioShared>) -> Option<(cpal::Stream, u32)> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let cfg: cpal::StreamConfig = config.config();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &cfg, channels, rate, sh),
        cpal::SampleFormat::I16 => build::<i16>(&device, &cfg, channels, rate, sh),
        cpal::SampleFormat::U16 => build::<u16>(&device, &cfg, channels, rate, sh),
        _ => None,
    }?;
    stream.play().ok()?;
    Some((stream, rate))
}

fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    sh: Arc<AudioShared>,
) -> Option<cpal::Stream> {
    device
        .build_output_stream(
            cfg,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let seek = sh.seek.swap(-1, Ordering::SeqCst);
                let mut pos = if seek >= 0 { seek as u64 } else { sh.pos.load(Ordering::SeqCst) };
                let playing = sh.playing.load(Ordering::SeqCst);
                let samples = sh.samples.try_lock().ok().map(|s| s.clone());
                let [in_t, out_t, fade_in, fade_out] = sh.fades();
                for frame in data.chunks_mut(channels) {
                    let (l, r) = if playing {
                        let i = pos as usize * 2;
                        let g = crate::editor::fade_gain(pos as f64 / rate as f64, in_t, out_t, fade_in, fade_out);
                        pos += 1;
                        samples.as_ref().and_then(|s| Some((*s.get(i)? * g, *s.get(i + 1)? * g))).unwrap_or((0.0, 0.0))
                    } else {
                        (0.0, 0.0)
                    };
                    for (c, out) in frame.iter_mut().enumerate() {
                        let v = match (channels, c) {
                            (1, _) => (l + r) * 0.5,
                            (_, 0) => l,
                            (_, 1) => r,
                            _ => 0.0,
                        };
                        *out = T::from_sample(v);
                    }
                }
                sh.pos.store(pos, Ordering::SeqCst);
            },
            |e| eprintln!("audio: {e}"),
            None,
        )
        .ok()
}
