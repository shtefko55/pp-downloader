//! Прозорец за прецизиране на клип: преглед, waveform, IN/OUT, blur маски и експорт.

use crate::blur::{self, Alpha};
use crate::export::{self, ExportJob, ExportStatus};
use crate::media::{self, AudioData, AudioOut, Frame, FrameFetcher, FrameStream, Info};
use crate::track;
use eframe::egui::{self, pos2, vec2, Color32, CursorIcon, Key, Modifiers, Pos2, Rect, Sense, Shape, Stroke};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const IN_COLOR: Color32 = Color32::from_rgb(70, 200, 110);
const OUT_COLOR: Color32 = Color32::from_rgb(230, 90, 80);
const PREVIEW_MAX_W: usize = 960;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum Kind {
    #[default]
    Lasso,
    Rect,
    Ellipse,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Lasso => "Ласо",
            Kind::Rect => "Правоъгълник",
            Kind::Ellipse => "Кръг",
        }
    }
}

/// Положение на маската в даден момент (секунди от клипа); между ключовете — линейно.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Keyframe {
    pub t: f64,
    pub offset: [f32; 2],
    pub scale: f32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Mask {
    #[serde(default)]
    pub kind: Kind,
    /// Контурът (ласо, правоъгълник или кръг като многоъгълник), нормализиран 0..1 спрямо кадъра.
    pub points: Vec<[f32; 2]>,
    pub offset: [f32; 2],
    pub scale: f32,
    /// Сила на размазването (sigma, пиксели в оригиналния размер).
    pub blur: f32,
    /// Омекотяване на ръба (пиксели в оригиналния размер).
    pub feather: f32,
    /// Кога се вижда маската: подредени участъци [от, до) в секунди от клипа. Празно = през целия клип.
    #[serde(default)]
    pub spans: Vec<[f64; 2]>,
    /// Движение: подредени по време ключове. Празно = неподвижна (`offset`/`scale`), иначе те не се ползват.
    #[serde(default)]
    pub keys: Vec<Keyframe>,
}

impl Mask {
    fn new(kind: Kind, points: Vec<[f32; 2]>) -> Self {
        Mask { kind, points, offset: [0.0, 0.0], scale: 1.0, blur: 15.0, feather: 3.0, spans: Vec::new(), keys: Vec::new() }
    }

    /// Преместване и мащаб в момента `t`.
    pub fn transform_at(&self, t: f64) -> ([f32; 2], f32) {
        let k = &self.keys;
        let (Some(first), Some(last)) = (k.first(), k.last()) else { return (self.offset, self.scale) };
        if t <= first.t {
            return (first.offset, first.scale);
        }
        if t >= last.t {
            return (last.offset, last.scale);
        }
        let i = k.partition_point(|key| key.t <= t);
        let (a, b) = (k[i - 1], k[i]);
        let u = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
        let lerp = |x: f32, y: f32| x + (y - x) * u;
        ([lerp(a.offset[0], b.offset[0]), lerp(a.offset[1], b.offset[1])], lerp(a.scale, b.scale))
    }

    /// Ключът точно в този кадър (до половин кадър разлика).
    pub fn key_at(&self, t: f64, fps: f64) -> Option<usize> {
        self.keys.iter().position(|k| (k.t - t).abs() < 0.5 / fps)
    }

    /// Задава положението в `t`: без ключове — за целия клип, иначе създава/сменя ключ в този кадър.
    pub fn set_transform_at(&mut self, t: f64, fps: f64, offset: [f32; 2], scale: f32) {
        if self.keys.is_empty() {
            self.offset = offset;
            self.scale = scale;
            return;
        }
        let key = Keyframe { t: snap(t, fps), offset, scale };
        match self.key_at(t, fps) {
            Some(i) => self.keys[i] = key,
            None => {
                let i = self.keys.partition_point(|k| k.t < key.t);
                self.keys.insert(i, key);
            }
        }
    }

    /// Добавя ключ в `t` с текущото положение (първият ключ включва движението).
    pub fn add_key(&mut self, t: f64, fps: f64) {
        let (o, s) = self.transform_at(t);
        if self.keys.is_empty() {
            self.keys.push(Keyframe { t: snap(t, fps), offset: o, scale: s });
        } else {
            self.set_transform_at(t, fps, o, s);
        }
    }

    /// Маха ключа в `t`; без ключове маската остава неподвижна там, където е била в този кадър.
    pub fn remove_key(&mut self, t: f64, fps: f64) {
        if let Some(i) = self.key_at(t, fps) {
            let k = self.keys.remove(i);
            if self.keys.is_empty() {
                self.offset = k.offset;
                self.scale = k.scale;
            }
        }
    }

    /// Маха всички ключове; маската остава както е в `t`.
    pub fn clear_keys(&mut self, t: f64) {
        (self.offset, self.scale) = self.transform_at(t);
        self.keys.clear();
    }

    /// Премества маската (и всички ключове) с `d`.
    fn nudge(&mut self, d: [f32; 2]) {
        self.offset = [self.offset[0] + d[0], self.offset[1] + d[1]];
        for k in &mut self.keys {
            k.offset = [k.offset[0] + d[0], k.offset[1] + d[1]];
        }
    }

    /// Нова форма, която в момента `t` да изглежда точно като `eff`.
    /// Без ключове — записва се директно; с ключове — точките се пресмятат обратно през положението в `t`.
    fn set_shape_at(&mut self, t: f64, eff: Vec<[f32; 2]>) {
        if self.keys.is_empty() {
            self.points = eff;
            self.offset = [0.0, 0.0];
            self.scale = 1.0;
            return;
        }
        let (o, s) = self.transform_at(t);
        let n = eff.len().max(1) as f32;
        let m = eff.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
        let c = [m[0] - o[0], m[1] - o[1]];
        let s = s.max(1e-4);
        self.points = eff.iter().map(|p| [c[0] + (p[0] - o[0] - c[0]) / s, c[1] + (p[1] - o[1] - c[1]) / s]).collect();
    }

    pub fn active_at(&self, t: f64) -> bool {
        self.spans.is_empty() || self.spans.iter().any(|s| t >= s[0] - 1e-6 && t < s[1] - 1e-6)
    }

    fn span_at(&self, t: f64) -> Option<usize> {
        self.spans.iter().position(|s| t >= s[0] - 1e-6 && t < s[1] - 1e-6)
    }

    /// „Появи се тук“: видима от `t` до следващото скриване (или до края).
    /// Ако досега е била през целия клип — вече се появява чак от `t`.
    fn show_from(&mut self, t: f64, duration: f64) {
        if self.spans.is_empty() {
            self.spans.push([t, duration]);
            return;
        }
        if self.span_at(t).is_some() {
            return;
        }
        self.spans.retain(|s| s[1] - s[0] > 1e-3);
        match self.spans.iter_mut().find(|s| s[0] > t) {
            Some(next) => next[0] = t,
            None => self.spans.push([t, duration]),
        }
    }

    /// „Скрий тук“: участъкът, в който е `t`, свършва в `t`.
    fn hide_from(&mut self, t: f64, duration: f64) {
        if self.spans.is_empty() {
            self.spans.push([0.0, duration]);
        }
        if let Some(i) = self.span_at(t) {
            self.spans[i][1] = t;
            if self.spans[i][1] - self.spans[i][0] < 1e-3 {
                self.spans.remove(i);
                // Без нито един участък маската би станала видима навсякъде — оставя се невидим отрязък.
                if self.spans.is_empty() {
                    self.spans.push([t, t]);
                }
            }
        }
    }

    /// Правоъгълник или елипса, вписани в кутията между два ъгъла.
    fn shape_points(kind: Kind, a: [f32; 2], b: [f32; 2]) -> Vec<[f32; 2]> {
        let (x0, x1, y0, y1) = (a[0].min(b[0]), a[0].max(b[0]), a[1].min(b[1]), a[1].max(b[1]));
        match kind {
            Kind::Ellipse => {
                let (cx, cy, rx, ry) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0, (y1 - y0) / 2.0);
                (0..72)
                    .map(|i| {
                        let t = i as f32 / 72.0 * std::f32::consts::TAU;
                        [cx + rx * t.cos(), cy + ry * t.sin()]
                    })
                    .collect()
            }
            _ => vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
        }
    }

    /// Контурът след мащаб (около центъра) и преместване — без ключовете.
    pub fn effective(&self) -> Vec<[f32; 2]> {
        self.shaped(self.offset, self.scale)
    }

    /// Контурът в момента `t` (с движението от ключовете).
    pub fn effective_at(&self, t: f64) -> Vec<[f32; 2]> {
        let (o, s) = self.transform_at(t);
        self.shaped(o, s)
    }

    fn shaped(&self, offset: [f32; 2], scale: f32) -> Vec<[f32; 2]> {
        let n = self.points.len().max(1) as f32;
        let c = self.points.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
        self.points
            .iter()
            .map(|p| [c[0] + (p[0] - c[0]) * scale + offset[0], c[1] + (p[1] - c[1]) * scale + offset[1]])
            .collect()
    }

    /// Средата на обхващащата кутия на основната форма и центърът, около който се мащабира.
    fn anchors(&self) -> ([f32; 2], [f32; 2]) {
        let n = self.points.len().max(1) as f32;
        let c = self.points.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
        let b = bbox(&self.points);
        ([(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0], c)
    }

    /// Преместването, при което средата на кутията при мащаб `scale` е в `center`.
    fn offset_for(&self, center: [f32; 2], scale: f32) -> [f32; 2] {
        let (bc, c) = self.anchors();
        [center[0] - c[0] - (bc[0] - c[0]) * scale, center[1] - c[1] - (bc[1] - c[1]) * scale]
    }

    /// Неподвижна алфа маска (без ключове).
    pub fn alpha(&self, w: usize, h: usize, px_scale: f32) -> Alpha {
        let pts: Vec<[f32; 2]> = self.effective().iter().map(|p| [p[0] * w as f32, p[1] * h as f32]).collect();
        blur::mask_alpha(&pts, w, h, self.feather * px_scale)
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct EditState {
    pub in_t: f64,
    /// Краят е изключващ: последният кадър в експорта е out_t − 1 кадър.
    pub out_t: f64,
    pub masks: Vec<Mask>,
    /// Последният експорт — за корекции се предлага същото име.
    #[serde(default)]
    pub exported: Option<PathBuf>,
    /// Плавно усилване/затихване на звука в началото и края (секунди).
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
}

impl EditState {
    /// Откъсът е сменен с нов таймкод: IN/OUT стават новите граници, а маските се местят
    /// с `shift` секунди (новото начало на файла е с толкова по-рано), за да останат върху същите кадри.
    pub fn retime(&mut self, shift: f64, in_t: f64, out_t: f64) {
        self.in_t = in_t;
        self.out_t = out_t;
        self.masks.retain_mut(|m| {
            for k in &mut m.keys {
                k.t += shift;
            }
            if m.spans.is_empty() {
                return true; // през целия клип
            }
            m.spans.retain_mut(|s| {
                *s = [(s[0] + shift).max(0.0), s[1] + shift];
                s[1] > s[0]
            });
            // Всичките ѝ участъци са извън новия откъс — иначе празното би значело „през целия клип“.
            !m.spans.is_empty()
        });
    }

    /// Усилване на звука в момент `t` от клипа (извън IN–OUT: 1).
    pub fn gain_at(&self, t: f64) -> f32 {
        fade_gain(t, self.in_t, self.out_t, self.fade_in, self.fade_out)
    }
}

/// Крива „qsin“ (като afade във ffmpeg) — приблизително постоянна мощност.
pub fn fade_gain(t: f64, in_t: f64, out_t: f64, fade_in: f64, fade_out: f64) -> f32 {
    if t < in_t || t >= out_t {
        return 1.0;
    }
    let ramp = |x: f64| (x.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2).sin();
    let a = if fade_in > 0.0 { ramp((t - in_t) / fade_in) } else { 1.0 };
    let b = if fade_out > 0.0 { ramp((out_t - t) / fade_out) } else { 1.0 };
    (a * b) as f32
}

/// Свален клип от проекта — за списъка в редактора.
#[derive(Clone)]
pub struct ClipRef {
    pub path: PathBuf,
    /// „3. Име“
    pub label: String,
    /// Таймкодове или „видео от диска“.
    pub detail: String,
    pub edited: bool,
    pub exported: bool,
}

/// Какво чертае влаченето в картината.
#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Select,
    Draw(Kind),
}

/// Преоразмеряване с точка: маска, точка (0..8), кутия и точки в началото.
struct Resize {
    mask: usize,
    handle: usize,
    start_box: [f32; 4],
    start_points: Vec<[f32; 2]>,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Playhead,
    In,
    Out,
    FadeIn,
    FadeOut,
    /// Ръб на участък от видимостта на избраната маска: (участък, края ли е).
    Span(usize, bool),
}

/// Проследяване, което върви във фонова нишка.
#[derive(Default)]
struct TrackStatus {
    running: bool,
    progress: f32,
    message: String,
    done: Option<TrackDone>,
}

struct TrackDone {
    mask: usize,
    /// Формата на маската при старта — ако междувременно е сменена, резултатът не става.
    points: Vec<[f32; 2]>,
    w: usize,
    h: usize,
    result: Result<track::Outcome, String>,
}

pub struct Editor {
    pub path: PathBuf,
    pub state: EditState,
    pub open: bool,
    pub viewport: egui::ViewportId,
    name: String,
    ffmpeg: PathBuf,
    crf: u32,
    /// Папка по подразбиране за първия експорт (Готови в проекта).
    export_dir: PathBuf,
    info: Info,
    pw: usize,
    ph: usize,

    playhead: f64,
    playing: bool,
    play_end: f64,
    waiting_first: bool,
    clock: Option<(Instant, f64)>,
    stream: Option<FrameStream>,
    pending: Option<Frame>,
    fetcher: FrameFetcher,
    audio: Option<AudioOut>,
    /// Audio scrubbing при влачене по timeline-а и кадър напред/назад.
    scrub_on: bool,
    last_scrub: Option<f64>,
    /// Прозорецът е създаден скрит; след първия кадър се показва в запомненото състояние (виж `winshow`).
    pub shown: bool,
    first_pass: Option<u64>,
    /// Прозорецът в Windows и текущото му състояние (размер, позиция, цял екран) — за запомняне.
    pub hwnd: isize,
    pub place: Option<crate::winshow::Place>,
    audio_data: Arc<Mutex<Option<Result<AudioData, String>>>>,

    frame: Option<Vec<u8>>,
    /// Моментът на показания кадър — за видимостта на маските.
    frame_t: f64,
    frame_gen: u64,
    texture: Option<egui::TextureHandle>,
    shown_key: String,

    selected: Option<usize>,
    tool: Tool,
    /// Чертаната в момента форма (ласо: всички точки; правоъгълник/кръг: двата ъгъла).
    draft: Vec<[f32; 2]>,
    moving: bool,
    resize: Option<Resize>,
    show_blur: bool,
    show_outlines: bool,
    /// По маска: контурът и feather-ът, за които е сметната алфата.
    mask_cache: Vec<Option<(Vec<[f32; 2]>, f32, Alpha)>>,
    track: Arc<Mutex<TrackStatus>>,
    track_cancel: Arc<AtomicBool>,
    track_scale: bool,

    view: (f64, f64),
    drag: Drag,

    export: Arc<Mutex<ExportStatus>>,
    export_cancel: Arc<AtomicBool>,

    /// Свалените клипове на проекта; изборът на друг се записва в `switch_to`.
    clips: Vec<ClipRef>,
    pub switch_to: Option<PathBuf>,
    pub switch_error: String,
    clip_scrolled: bool,
}

impl Editor {
    pub fn open(
        path: PathBuf,
        ffmpeg: PathBuf,
        crf: u32,
        saved: Option<EditState>,
        marks: Option<(f64, f64)>,
        export_dir: PathBuf,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let info = media::probe(&ffmpeg, &path)?;
        if info.width < 2 || info.height < 2 {
            return Err("Неизвестен размер на кадъра".into());
        }
        let pw = (info.width as usize).min(PREVIEW_MAX_W) / 2 * 2;
        let ph = (((pw as f64 * info.height as f64 / info.width as f64).round() as usize) / 2 * 2).max(2);

        let fl = 1.0 / info.fps;
        let mut state = saved.unwrap_or_else(|| {
            let (a, b) = marks.unwrap_or((0.0, info.duration));
            EditState { in_t: a, out_t: b, ..Default::default() }
        });
        state.in_t = snap(state.in_t, info.fps).clamp(0.0, (info.duration - fl).max(0.0));
        state.out_t = snap(state.out_t, info.fps).min(info.duration).max(state.in_t + fl);

        let audio = if info.has_audio { AudioOut::new() } else { None };
        let audio_data = Arc::new(Mutex::new(None));
        if info.has_audio {
            let rate = audio.as_ref().map_or(48_000, |a| a.rate);
            let shared = audio.as_ref().map(|a| a.shared.clone());
            let (ff, p, ad, ctx) = (ffmpeg.clone(), path.clone(), audio_data.clone(), ctx.clone());
            std::thread::spawn(move || {
                let r = media::decode_audio(&ff, &p, rate);
                if let (Ok(d), Some(sh)) = (&r, shared) {
                    sh.set_samples(d.samples.clone());
                }
                *ad.lock().unwrap() = Some(r);
                ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
            });
        }

        let fetcher = FrameFetcher::new(ffmpeg.clone(), path.clone(), pw, ph, ctx.clone());
        fetcher.request(state.in_t);
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        Ok(Editor {
            // Уникален за всеки прозорец: при смяна на клипа новият редактор наследява прозореца.
            viewport: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                egui::ViewportId::from_hash_of(("editor", NEXT.fetch_add(1, Ordering::Relaxed)))
            },
            path,
            name,
            ffmpeg,
            crf,
            export_dir,
            pw,
            ph,
            playhead: state.in_t,
            view: (0.0, info.duration),
            state,
            info,
            open: true,
            playing: false,
            play_end: 0.0,
            waiting_first: false,
            clock: None,
            stream: None,
            pending: None,
            fetcher,
            audio,
            scrub_on: true,
            last_scrub: None,
            shown: false,
            first_pass: None,
            hwnd: 0,
            place: None,
            audio_data,
            frame: None,
            frame_t: 0.0,
            frame_gen: 0,
            texture: None,
            shown_key: String::new(),
            selected: None,
            tool: Tool::Select,
            draft: Vec::new(),
            moving: false,
            resize: None,
            show_blur: true,
            show_outlines: true,
            mask_cache: Vec::new(),
            track: Default::default(),
            track_cancel: Default::default(),
            track_scale: false,
            drag: Drag::None,
            export: Default::default(),
            clips: Vec::new(),
            switch_to: None,
            switch_error: String::new(),
            clip_scrolled: false,
            export_cancel: Default::default(),
        })
    }

    pub fn exporting(&self) -> bool {
        self.export.lock().unwrap().running
    }

    /// При затваряне на програмата: спира експорта и проследяването (техните ffmpeg процеси се убиват).
    pub fn shutdown(&mut self) {
        self.export_cancel.store(true, Ordering::SeqCst);
        self.track_cancel.store(true, Ordering::SeqCst);
        self.pause();
    }

    pub fn show(&mut self, ctx: &egui::Context, clips: &[ClipRef]) {
        self.clips = clips.to_vec();
        let title = format!("Редакция — {}", self.name);
        let builder = egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([1320.0, 900.0])
            .with_min_inner_size([960.0, 620.0])
            .with_visible(self.shown);
        let id = self.viewport;
        ctx.show_viewport_immediate(id, builder, |ctx, _| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.open = false;
            }
            // Първият кадър се рисува в скрития прозорец, в следващия той се показва както е бил затворен.
            if self.shown {
                self.place = crate::winshow::get(self.hwnd).or(self.place);
            } else {
                let pass = ctx.cumulative_pass_nr();
                match self.first_pass {
                    Some(p) if pass > p => {
                        self.shown = true;
                        self.hwnd = crate::winshow::find_by_title(&title);
                        crate::winshow::show(ctx, self.hwnd, self.place);
                    }
                    Some(_) => {}
                    None => self.first_pass = Some(pass),
                }
                ctx.request_repaint();
            }
            self.tick(ctx);
            self.keys(ctx);
            self.update_texture(ctx);
            egui::SidePanel::right(egui::Id::new(("ed_side", id)))
                .exact_width(300.0)
                .show(ctx, |ui| {
                    // Отгоре маските (толкова място, колкото им трябва), под тях списъкът с клипове
                    // до долния ръб на прозореца.
                    let with_clips = self.clips.len() > 1;
                    self.masks_ui(ui, if with_clips { 260.0 } else { 0.0 });
                    if with_clips {
                        ui.separator();
                        self.clips_ui(ui);
                    }
                });
            // Бутоните и таймкодовете стоят точно над waveform-а.
            egui::TopBottomPanel::bottom(egui::Id::new(("ed_timeline", id))).show(ctx, |ui| {
                self.toolbar_ui(ui);
                self.timeline_ui(ui);
            });
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(Color32::from_gray(12)))
                .show(ctx, |ui| self.preview_ui(ui));
        });
    }

    // ---------- време ----------

    fn frame_len(&self) -> f64 {
        1.0 / self.info.fps
    }

    fn last_frame_t(&self) -> f64 {
        snap((self.info.duration - self.frame_len()).max(0.0), self.info.fps)
    }

    /// ЧЧ:ММ:СС:КК
    fn tc(&self, t: f64) -> String {
        fmt_tc(t, self.info.fps)
    }

    fn now(&self) -> f64 {
        if self.waiting_first {
            return self.playhead;
        }
        match (&self.audio, self.clock) {
            (Some(a), _) => a.time(),
            (None, Some((start, t))) => t + start.elapsed().as_secs_f64(),
            _ => self.playhead,
        }
    }

    // ---------- пускане ----------

    fn play(&mut self, from: f64, until: f64) {
        self.pause();
        let from = snap(from, self.info.fps).clamp(0.0, self.last_frame_t());
        self.playhead = from;
        self.play_end = until.min(self.info.duration);
        self.stream = Some(FrameStream::start(&self.ffmpeg, &self.path, from, self.pw, self.ph, self.info.fps));
        self.playing = true;
        self.waiting_first = true;
    }

    fn pause(&mut self) {
        self.playing = false;
        self.stream = None;
        self.pending = None;
        self.clock = None;
        self.waiting_first = false;
        if let Some(a) = &self.audio {
            a.pause();
        }
    }

    fn toggle_play(&mut self) {
        if self.playing {
            self.pause();
            self.seek(self.playhead);
        } else {
            let from = if self.playhead >= self.last_frame_t() - 1e-6 { 0.0 } else { self.playhead };
            self.play(from, self.info.duration);
        }
    }

    fn seek(&mut self, t: f64) {
        let t = snap(t, self.info.fps).clamp(0.0, self.last_frame_t());
        self.playhead = t;
        if self.playing {
            let end = self.play_end;
            self.play(t, end);
        } else {
            self.fetcher.request(t);
        }
    }

    /// Audio scrubbing: кратко парче звук от текущия кадър (само при пауза).
    fn scrub_audio(&mut self) {
        if self.playing || !self.scrub_on || self.last_scrub == Some(self.playhead) {
            return;
        }
        self.last_scrub = Some(self.playhead);
        if let Some(a) = &self.audio {
            a.scrub(self.playhead, (self.frame_len() * 2.0).max(0.08));
        }
    }

    fn set_frame(&mut self, f: Frame) {
        self.frame = Some(f.rgba);
        self.frame_t = f.t;
        self.frame_gen += 1;
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.poll_track();
        if let Some(a) = &self.audio {
            let s = &self.state;
            a.shared.set_fades(s.in_t, s.out_t, s.fade_in, s.fade_out);
        }
        while let Ok(f) = self.fetcher.rx.try_recv() {
            if !self.playing {
                self.set_frame(f);
            }
        }
        if !self.playing {
            return;
        }
        ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
        let mut latest = None;
        if self.waiting_first {
            // Звукът тръгва чак когато първият кадър е готов — иначе картината изостава.
            if let Some(f) = self.stream.as_ref().and_then(|s| s.rx.try_recv().ok()) {
                self.waiting_first = false;
                match &self.audio {
                    Some(a) => a.play_from(self.playhead),
                    None => self.clock = Some((Instant::now(), self.playhead)),
                }
                latest = Some(f);
            }
        } else {
            let now = self.now();
            let half = 0.5 * self.frame_len();
            if let Some(stream) = &self.stream {
                loop {
                    if self.pending.is_none() {
                        self.pending = stream.rx.try_recv().ok();
                    }
                    match &self.pending {
                        Some(f) if f.t <= now + half => latest = self.pending.take(),
                        _ => break,
                    }
                }
            }
            self.playhead = now.min(self.info.duration);
            if now >= self.play_end {
                self.pause();
                self.playhead = snap(self.play_end - self.frame_len(), self.info.fps).max(0.0);
            }
        }
        if let Some(f) = latest {
            self.set_frame(f);
        }
    }

    // ---------- IN/OUT ----------

    fn set_in(&mut self, t: f64) {
        self.state.in_t = snap(t, self.info.fps).clamp(0.0, self.state.out_t - self.frame_len());
    }

    fn set_out(&mut self, t: f64) {
        self.state.out_t = snap(t, self.info.fps).clamp(self.state.in_t + self.frame_len(), self.info.duration);
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let fl = self.frame_len();
        let take = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        let space = take(Modifiers::NONE, Key::Space);
        let left = take(Modifiers::NONE, Key::ArrowLeft);
        let right = take(Modifiers::NONE, Key::ArrowRight);
        let left_s = take(Modifiers::SHIFT, Key::ArrowLeft);
        let right_s = take(Modifiers::SHIFT, Key::ArrowRight);
        let key_i = take(Modifiers::NONE, Key::I);
        let key_o = take(Modifiers::NONE, Key::O);
        let home = take(Modifiers::NONE, Key::Home);
        let end = take(Modifiers::NONE, Key::End);
        let del = take(Modifiers::NONE, Key::Delete);
        let prev_clip = take(Modifiers::NONE, Key::PageUp);
        let next_clip = take(Modifiers::NONE, Key::PageDown);
        if (prev_clip || next_clip) && !self.exporting() {
            if let Some(i) = self.clips.iter().position(|c| c.path == self.path) {
                let j = if next_clip { i + 1 } else { i.wrapping_sub(1) };
                self.switch_to = self.clips.get(j).map(|c| c.path.clone());
            }
        }
        let esc = take(Modifiers::NONE, Key::Escape);
        if take(Modifiers::NONE, Key::K) {
            let (t, fps) = (self.playhead, self.info.fps);
            if let Some(m) = self.selected.and_then(|i| self.state.masks.get_mut(i)) {
                m.add_key(t, fps);
            }
        }
        if space {
            self.toggle_play();
        }
        for (pressed, delta) in [(left, -fl), (right, fl), (left_s, -1.0), (right_s, 1.0)] {
            if pressed {
                if self.playing {
                    self.pause();
                }
                self.seek(self.playhead + delta);
                self.last_scrub = None;
                self.scrub_audio();
            }
        }
        if key_i {
            self.set_in(self.playhead);
        }
        if key_o {
            self.set_out(self.playhead + fl);
        }
        if home {
            self.seek(self.state.in_t);
        }
        if end {
            self.seek(self.state.out_t - fl);
        }
        if del {
            if let Some(i) = self.selected.take() {
                self.state.masks.remove(i);
            }
        }
        if esc {
            self.tool = Tool::Select;
            self.draft.clear();
        }
    }

    // ---------- картина ----------

    fn update_texture(&mut self, ctx: &egui::Context) {
        let masks_key = if self.show_blur { serde_json::to_string(&self.state.masks).unwrap_or_default() } else { String::new() };
        let key = format!("{}|{masks_key}", self.frame_gen);
        if key == self.shown_key {
            return;
        }
        let Some(frame) = &self.frame else { return };
        let mut img = frame.clone();
        if self.show_blur {
            let sp = self.pw as f32 / self.info.width as f32;
            self.mask_cache.resize_with(self.state.masks.len(), || None);
            for (m, cache) in self.state.masks.iter().zip(self.mask_cache.iter_mut()) {
                if !m.active_at(self.frame_t) {
                    continue;
                }
                let eff = m.effective_at(self.frame_t);
                if cache.as_ref().map_or(true, |(e, f, _)| *e != eff || *f != m.feather) {
                    let pts: Vec<[f32; 2]> = eff.iter().map(|p| [p[0] * self.pw as f32, p[1] * self.ph as f32]).collect();
                    let alpha = blur::mask_alpha(&pts, self.pw, self.ph, m.feather * sp);
                    *cache = Some((eff, m.feather, alpha));
                }
                if let Some((_, _, alpha)) = cache {
                    blur::apply_blur(&mut img, self.pw, self.ph, alpha, m.blur * sp);
                }
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([self.pw, self.ph], &img);
        match &mut self.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture(format!("preview-{}", self.name), image, egui::TextureOptions::LINEAR)),
        }
        self.shown_key = key;
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let area = resp.rect.shrink(8.0);
        let k = (area.width() / self.pw as f32).min(area.height() / self.ph as f32);
        let rect = Rect::from_center_size(area.center(), vec2(self.pw as f32 * k, self.ph as f32 * k));
        match &self.texture {
            Some(t) => {
                painter.image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Зареждане…", egui::FontId::proportional(20.0), Color32::GRAY);
            }
        }
        let to_norm = |p: Pos2| [(p.x - rect.min.x) / rect.width(), (p.y - rect.min.y) / rect.height()];
        let to_screen = |q: &[f32; 2]| pos2(rect.min.x + q[0] * rect.width(), rect.min.y + q[1] * rect.height());
        // Маските се показват и редактират такива, каквито са в показания кадър.
        let (ft, fps) = (self.frame_t, self.info.fps);
        let hit = |masks: &[Mask], p: Pos2| masks.iter().rposition(|m| point_in_poly(to_norm(p), &m.effective_at(ft)));

        // Точката за преоразмеряване на избраната маска под курсора.
        let handle_at = |masks: &[Mask], sel: Option<usize>, p: Pos2| {
            let b = bbox(&masks.get(sel?)?.effective_at(ft));
            (0..8).find(|&h| to_screen(&handle_pos(b, h)).distance(p) <= 8.0)
        };
        let shift = ui.input(|i| i.modifiers.shift);
        // Ъгъл на правоъгълник/кръг; със Shift — квадрат/кръг (равни страни в пиксели).
        let corner = |a: Pos2, p: Pos2| {
            if !shift {
                return p;
            }
            let d = p - a;
            let s = d.x.abs().max(d.y.abs());
            a + vec2(s * d.x.signum(), s * d.y.signum())
        };

        match self.tool {
            Tool::Draw(kind) => {
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                }
                let origin = ui.input(|i| i.pointer.press_origin());
                if resp.drag_started() {
                    self.draft.clear();
                    if let Some(p) = origin {
                        self.draft.push(to_norm(p));
                    }
                }
                if resp.dragged() {
                    if let (Some(p), Some(o)) = (resp.interact_pointer_pos(), origin) {
                        if kind == Kind::Lasso {
                            if self.draft.last().map_or(true, |l| to_screen(l).distance(p) > 3.0) {
                                self.draft.push(to_norm(p));
                            }
                        } else {
                            self.draft.truncate(1);
                            self.draft.push(to_norm(corner(o, p)));
                        }
                    }
                }
                if resp.drag_stopped() {
                    let points = match kind {
                        Kind::Lasso if self.draft.len() >= 3 => Some(std::mem::take(&mut self.draft)),
                        Kind::Rect | Kind::Ellipse if self.draft.len() == 2 => {
                            let (a, b) = (to_screen(&self.draft[0]), to_screen(&self.draft[1]));
                            ((a.x - b.x).abs() > 4.0 && (a.y - b.y).abs() > 4.0)
                                .then(|| Mask::shape_points(kind, self.draft[0], self.draft[1]))
                        }
                        _ => None,
                    };
                    if let Some(points) = points {
                        self.state.masks.push(Mask::new(kind, points));
                        self.selected = Some(self.state.masks.len() - 1);
                    }
                    self.draft.clear();
                    self.tool = Tool::Select;
                }
            }
            Tool::Select => {
                if resp.clicked() {
                    self.selected = resp.interact_pointer_pos().and_then(|p| hit(&self.state.masks, p));
                }
                if resp.drag_started() {
                    let origin = ui.input(|i| i.pointer.press_origin());
                    if let Some((i, h)) = origin.and_then(|p| Some((self.selected?, handle_at(&self.state.masks, self.selected, p)?))) {
                        let eff = self.state.masks[i].effective_at(ft);
                        self.resize = Some(Resize { mask: i, handle: h, start_box: bbox(&eff), start_points: eff });
                    } else {
                        let h = origin.and_then(|p| hit(&self.state.masks, p));
                        self.moving = h.is_some();
                        if h.is_some() {
                            self.selected = h;
                        }
                    }
                }
                if resp.dragged() {
                    if let Some(r) = &self.resize {
                        if let (Some(p), Some(m)) = (resp.interact_pointer_pos(), self.state.masks.get_mut(r.mask)) {
                            let nb = resized_box(r.start_box, r.handle, to_norm(p));
                            m.set_shape_at(ft, remap(&r.start_points, r.start_box, nb));
                        }
                        ui.ctx().set_cursor_icon(handle_cursor(r.handle));
                    } else if self.moving {
                        if let Some(m) = self.selected.and_then(|i| self.state.masks.get_mut(i)) {
                            let d = resp.drag_delta();
                            let (o, sc) = m.transform_at(ft);
                            m.set_transform_at(ft, fps, [o[0] + d.x / rect.width(), o[1] + d.y / rect.height()], sc);
                        }
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    }
                } else if let Some(p) = resp.hover_pos() {
                    if let Some(h) = handle_at(&self.state.masks, self.selected, p) {
                        ui.ctx().set_cursor_icon(handle_cursor(h));
                    } else if hit(&self.state.masks, p).is_some() {
                        ui.ctx().set_cursor_icon(CursorIcon::Grab);
                    }
                }
                if resp.drag_stopped() {
                    self.moving = false;
                    self.resize = None;
                }
            }
        }

        let painter = painter.with_clip_rect(rect);
        let cyan = Color32::from_rgb(0, 220, 255);
        for (i, m) in self.state.masks.iter().enumerate() {
            let sel = self.selected == Some(i);
            if !(self.show_outlines || sel) {
                continue;
            }
            let eff = m.effective_at(ft);
            let pts: Vec<Pos2> = eff.iter().map(to_screen).collect();
            let stroke = if sel { Stroke::new(2.0_f32, Color32::YELLOW) } else { Stroke::new(1.0_f32, Color32::from_white_alpha(170)) };
            if m.active_at(self.frame_t) {
                painter.add(Shape::closed_line(pts, stroke));
            } else {
                // Скрита в този кадър: пунктир, за да може пак да се избере.
                let mut closed = pts;
                closed.extend(closed.first().copied());
                painter.extend(Shape::dashed_line(&closed, stroke, 6.0, 5.0));
            }
            if sel {
                let b = bbox(&eff);
                let frame = Rect::from_min_max(to_screen(&[b[0], b[1]]), to_screen(&[b[2], b[3]]));
                painter.rect_stroke(frame, 0.0, Stroke::new(1.0_f32, Color32::from_white_alpha(90)));
                for h in 0..8 {
                    let c = to_screen(&handle_pos(b, h));
                    painter.rect_filled(Rect::from_center_size(c, vec2(9.0, 9.0)), 1.0, Color32::WHITE);
                    painter.rect_stroke(Rect::from_center_size(c, vec2(9.0, 9.0)), 1.0, Stroke::new(1.0_f32, Color32::BLACK));
                }
            }
        }
        match self.tool {
            Tool::Draw(Kind::Lasso) if self.draft.len() >= 2 => {
                painter.add(Shape::line(self.draft.iter().map(to_screen).collect(), Stroke::new(2.0_f32, cyan)));
            }
            Tool::Draw(kind) if self.draft.len() == 2 => {
                let pts = Mask::shape_points(kind, self.draft[0], self.draft[1]);
                painter.add(Shape::closed_line(pts.iter().map(to_screen).collect(), Stroke::new(2.0_f32, cyan)));
            }
            _ => {}
        }
        if let Tool::Draw(kind) = self.tool {
            let hint = match kind {
                Kind::Lasso => "Ласо: задръж левия бутон и очертай обекта (Esc = отказ)",
                _ => "Натисни, задръж и влачи. Shift = квадрат/кръг (Esc = отказ)",
            };
            painter.text(rect.min + vec2(12.0, 12.0), egui::Align2::LEFT_TOP, hint, egui::FontId::proportional(16.0), cyan);
        }
    }

    // ---------- панели ----------

    fn toolbar_ui(&mut self, ui: &mut egui::Ui) {
        let fl = self.frame_len();
        ui.add_space(6.0);
        let transport_id = egui::Id::new(("transport", self.viewport));
        let marks_id = egui::Id::new(("marks", self.viewport));
        ui.horizontal(|ui| {
            let x0 = center_start(ui, transport_id);
            if ui.button("⏮").on_hover_text("Към IN (Home)").clicked() {
                self.seek(self.state.in_t);
            }
            if ui.button("◀").on_hover_text("Кадър назад (←), Shift+← = 1 сек").clicked() {
                self.seek(self.playhead - fl);
            }
            let label = if self.playing { "⏸  Пауза" } else { "▶  Пусни" };
            if ui.add(crate::colored_button(label, crate::GREEN).min_size(vec2(110.0, 28.0))).on_hover_text("Интервал").clicked() {
                self.toggle_play();
            }
            if ui.button("▶").on_hover_text("Кадър напред (→), Shift+→ = 1 сек").clicked() {
                self.seek(self.playhead + fl);
            }
            if ui.button("⏭").on_hover_text("Към OUT (End)").clicked() {
                self.seek(self.state.out_t - fl);
            }
            if ui.add(crate::colored_button("▶ IN → OUT", crate::BLUE).min_size(vec2(0.0, 28.0))).on_hover_text("Пусни само избрания участък").clicked() {
                self.play(self.state.in_t, self.state.out_t);
            }
            ui.separator();
            ui.label(egui::RichText::new(self.tc(self.playhead)).monospace().size(20.0).strong());
            ui.weak(format!("/ {}", self.tc(self.info.duration)));
            if self.info.has_audio {
                ui.toggle_value(&mut self.scrub_on, "🔊 Scrub")
                    .on_hover_text("Audio scrubbing: чуваш звука, докато влачиш по timeline-а или местиш кадър по кадър");
            }
            center_end(ui, transport_id, x0);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.export_ui(ui));
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let x0 = center_start(ui, marks_id);
            ui.label(egui::RichText::new("IN").color(IN_COLOR).strong());
            ui.monospace(self.tc(self.state.in_t));
            if ui.small_button("−1").on_hover_text("IN един кадър назад").clicked() {
                self.set_in(self.state.in_t - fl);
                self.seek(self.state.in_t);
            }
            if ui.small_button("+1").on_hover_text("IN един кадър напред").clicked() {
                self.set_in(self.state.in_t + fl);
                self.seek(self.state.in_t);
            }
            if ui.button("Задай IN  [I]").clicked() {
                self.set_in(self.playhead);
            }
            ui.separator();
            ui.label(egui::RichText::new("OUT").color(OUT_COLOR).strong());
            ui.monospace(self.tc(self.state.out_t - fl));
            if ui.small_button("−1").on_hover_text("OUT един кадър назад").clicked() {
                self.set_out(self.state.out_t - fl);
                self.seek(self.state.out_t - fl);
            }
            if ui.small_button("+1").on_hover_text("OUT един кадър напред").clicked() {
                self.set_out(self.state.out_t + fl);
                self.seek(self.state.out_t - fl);
            }
            if ui.button("Задай OUT  [O]").clicked() {
                self.set_out(self.playhead + fl);
            }
            ui.separator();
            ui.label(format!("Дължина: {}", self.tc(self.state.out_t - self.state.in_t)));
            center_end(ui, marks_id, x0);
        });
        ui.add_space(4.0);
    }

    fn export_ui(&mut self, ui: &mut egui::Ui) {
        let (running, progress, message, done) = {
            let st = self.export.lock().unwrap();
            (st.running, st.progress, st.message.clone(), st.done.clone())
        };
        if running {
            if ui.add(crate::colored_button("Спри", crate::RED)).clicked() {
                self.export_cancel.store(true, Ordering::SeqCst);
            }
            ui.add(egui::ProgressBar::new(progress).desired_width(220.0).text(format!("Експорт {:.0}%", progress * 100.0)));
            ui.ctx().request_repaint_after_for(std::time::Duration::from_millis(200), egui::ViewportId::ROOT);
            return;
        }
        if ui.add(crate::colored_button("💾  Експортирай…", crate::GREEN).min_size(vec2(170.0, 28.0))).clicked() {
            self.start_export(ui.ctx());
        }
        if let Some(p) = done {
            if self.state.exported.as_ref() != Some(&p) {
                self.state.exported = Some(p.clone());
            }
            if ui.small_button("Покажи файла").clicked() {
                crate::show_in_explorer(&p);
            }
        }
        if !message.is_empty() {
            ui.label(message);
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        if self.playing {
            self.pause();
        }
        // Корекция на вече експортиран клип: предлага същото име и папка.
        let (dir, file) = match &self.state.exported {
            Some(prev) => (
                prev.parent().map(Path::to_path_buf).unwrap_or_else(|| self.export_dir.clone()),
                prev.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            ),
            None => (self.export_dir.clone(), format!("{}.mp4", clean_name(&self.name))),
        };
        let _ = std::fs::create_dir_all(&dir);
        let Some(out) = rfd::FileDialog::new()
            .set_title("Експорт — име на готовия файл")
            .set_directory(&dir)
            .set_file_name(file)
            .add_filter("MP4 видео", &["mp4"])
            .save_file()
        else {
            return;
        };
        let out = if out.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
            out
        } else {
            let mut s = out.into_os_string();
            s.push(".mp4");
            PathBuf::from(s)
        };
        if out == self.path {
            self.export.lock().unwrap().message = "Не може да се презапише изходният клип — избери друго име.".into();
            return;
        }
        let job = ExportJob {
            ffmpeg: self.ffmpeg.clone(),
            src: self.path.clone(),
            out: out.clone(),
            width: self.info.width as usize,
            height: self.info.height as usize,
            fps: self.info.fps,
            has_audio: self.info.has_audio,
            state: self.state.clone(),
            crf: self.crf,
        };
        *self.export.lock().unwrap() = ExportStatus { running: true, ..Default::default() };
        self.export_cancel.store(false, Ordering::SeqCst);
        let (status, cancel, ctx) = (self.export.clone(), self.export_cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let st = status.clone();
            let result = export::run(&job, &cancel, |p| st.lock().unwrap().progress = p);
            let mut s = status.lock().unwrap();
            s.running = false;
            match result {
                Ok(()) => {
                    s.message = format!("✔ {}", out.file_name().unwrap_or_default().to_string_lossy());
                    s.done = Some(out);
                }
                Err(e) => s.message = format!("✖ {e}"),
            }
            ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
        });
    }

    // ---------- проследяване ----------

    fn start_track(&mut self, i: usize, forward: bool, ctx: &egui::Context) {
        if self.playing {
            self.pause();
            self.seek(self.playhead);
        }
        let (fps, fl) = (self.info.fps, self.frame_len());
        let t0 = snap(self.playhead, fps);
        let Some(m) = self.state.masks.get(i) else { return };
        let say = |msg: &str| self.track.lock().unwrap().message = msg.to_string();
        if !m.active_at(t0) {
            return say("Маската е скрита в този кадър — проследяването тръгва от кадър, в който се вижда.");
        }
        // Докъдето стига откъсът IN–OUT (ако сме в него) и видимият участък на маската.
        let s = &self.state;
        let (mut lo, mut hi) = if t0 >= s.in_t && t0 < s.out_t { (s.in_t, s.out_t) } else { (0.0, self.info.duration) };
        if let Some(k) = m.span_at(t0) {
            lo = lo.max(m.spans[k][0]);
            hi = hi.min(m.spans[k][1]);
        }
        let limit = if forward { hi } else { lo };
        let frames = ((limit - t0).abs() / fl).round() as usize;
        if frames <= usize::from(forward) {
            return say(if forward { "Това е последният кадър — няма накъде напред." } else { "Това е първият кадър — няма накъде назад." });
        }
        let w = self.pw.min(640) / 2 * 2;
        let h = (((w as f64 * self.ph as f64 / self.pw as f64).round() as usize) / 2 * 2).max(2);
        let req = track::Request {
            ffmpeg: self.ffmpeg.clone(),
            path: self.path.clone(),
            fps,
            t0,
            limit,
            forward,
            w,
            h,
            poly: m.effective_at(t0).iter().map(|p| [p[0] * w as f32, p[1] * h as f32]).collect(),
            with_scale: self.track_scale,
        };
        let points = m.points.clone();
        *self.track.lock().unwrap() = TrackStatus { running: true, ..Default::default() };
        self.track_cancel.store(false, Ordering::SeqCst);
        let (status, cancel, ctx) = (self.track.clone(), self.track_cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (st, c) = (status.clone(), ctx.clone());
            let result = track::run(&req, &cancel, |p| {
                st.lock().unwrap().progress = p;
                c.request_repaint_of(egui::ViewportId::ROOT);
            });
            let mut s = status.lock().unwrap();
            s.running = false;
            s.done = Some(TrackDone { mask: i, points, w, h, result });
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        });
    }

    /// Готовото проследяване става ключове на маската (заменят старите в проследения участък).
    fn poll_track(&mut self) {
        let Some(d) = self.track.lock().unwrap().done.take() else { return };
        let fps = self.info.fps;
        let mut seek_to = None;
        let msg = match (d.result, self.state.masks.get_mut(d.mask)) {
            (Err(e), _) => format!("✖ {e}"),
            (Ok(_), None) => "Маската е изтрита по време на проследяването.".into(),
            (Ok(_), Some(m)) if m.points != d.points => "Маската е променена по време на проследяването — пусни го пак.".into(),
            (Ok(out), Some(m)) => {
                let (t_first, t_last) = (out.samples[0].0, out.samples.last().map_or(0.0, |s| s.0));
                let s0 = m.transform_at(t_first).1;
                let (lo, hi) = (t_first.min(t_last) - 0.5 / fps, t_first.max(t_last) + 0.5 / fps);
                m.keys.retain(|k| k.t < lo || k.t > hi);
                let keys = track::simplify(&out.samples, 0.5);
                let n = keys.len();
                for (t, p) in keys {
                    let scale = s0 * p.scale;
                    let offset = m.offset_for([p.cx / d.w as f32, p.cy / d.h as f32], scale);
                    m.keys.push(Keyframe { t: snap(t, fps), offset, scale });
                }
                m.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
                let tc = fmt_tc(t_last, fps);
                if let Some(t) = out.lost_at {
                    seek_to = Some(t_last);
                    format!("⚠ Обектът се изгуби при {} — нагласи маската там и пусни проследяването оттам.", fmt_tc(t, fps))
                } else if out.cancelled {
                    format!("Спряно при {tc} ({n} ключа).")
                } else {
                    format!("✔ Проследено до {tc} ({n} ключа).")
                }
            }
        };
        self.track.lock().unwrap().message = msg;
        if let Some(t) = seek_to {
            self.seek(t);
        }
    }

    fn clips_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(format!("Клипове ({})", self.clips.len()));
            ui.weak("PgUp / PgDn");
        });
        let busy = self.exporting();
        if busy {
            ui.weak("Изчакай експорта, за да смениш клипа.");
        }
        if !self.switch_error.is_empty() {
            ui.colored_label(Color32::from_rgb(220, 60, 60), &self.switch_error);
        }
        let mut pick = None;
        egui::ScrollArea::vertical().id_salt("clips").auto_shrink(false).show(ui, |ui| {
            for c in &self.clips {
                let current = c.path == self.path;
                let mut marks = Vec::new();
                if c.edited {
                    marks.push("✂");
                }
                if c.exported {
                    marks.push("✔ експортиран");
                }
                let mut text = egui::RichText::new(format!("{}\n{}   {}", c.label, c.detail, marks.join("  ")));
                if current {
                    text = text.strong();
                }
                let r = ui
                    .add_enabled(!busy, egui::SelectableLabel::new(current, text))
                    .on_hover_text(c.path.file_name().unwrap_or_default().to_string_lossy());
                if current && !self.clip_scrolled {
                    r.scroll_to_me(Some(egui::Align::Center));
                    self.clip_scrolled = true;
                }
                if r.clicked() && !current {
                    pick = Some(c.path.clone());
                }
            }
        });
        if pick.is_some() {
            self.switch_to = pick;
        }
    }

    /// `reserve`: колко място да остане отдолу (за списъка с клипове).
    fn masks_ui(&mut self, ui: &mut egui::Ui, reserve: f32) {
        ui.add_space(8.0);
        ui.heading("Blur маски");
        ui.add_space(4.0);
        ui.label("Нова маска:");
        let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        ui.horizontal(|ui| {
            for (kind, label) in [(Kind::Rect, "▭ Правоъгълник"), (Kind::Ellipse, "○ Кръг")] {
                let active = self.tool == Tool::Draw(kind);
                let fill = if active { Color32::from_rgb(0, 150, 190) } else { crate::PURPLE };
                if ui.add(crate::colored_button(label, fill).min_size(vec2(w, 32.0))).clicked() {
                    self.tool = if active { Tool::Select } else { Tool::Draw(kind) };
                    self.draft.clear();
                    if self.playing {
                        self.pause();
                        self.seek(self.playhead);
                    }
                }
            }
        });
        ui.weak("Избери форма, после натисни, задръж и влачи в картината. Маската се мести с влачене, а размерът се сменя от белите точки. Delete изтрива избраната.");
        ui.checkbox(&mut self.show_blur, "Покажи blur в прегледа");
        ui.checkbox(&mut self.show_outlines, "Покажи контурите на всички маски");
        ui.separator();

        let mut remove = None;
        let mut duplicate = None;
        let mut seek_to = None;
        let mut track_go = None;
        let (t, dur, fps) = (self.playhead, self.info.duration, self.info.fps);
        let (trk_running, trk_progress, trk_msg) = {
            let st = self.track.lock().unwrap();
            (st.running, st.progress, st.message.clone())
        };
        if trk_running {
            ui.ctx().request_repaint_after_for(std::time::Duration::from_millis(200), egui::ViewportId::ROOT);
        }
        egui::ScrollArea::vertical()
            .id_salt("masks")
            .auto_shrink([false, true])
            .max_height((ui.available_height() - reserve).max(150.0))
            .show(ui, |ui| {
            if self.state.masks.is_empty() {
                ui.weak("Няма маски.");
            }
            for i in 0..self.state.masks.len() {
                let sel = self.selected == Some(i);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if ui.selectable_label(sel, egui::RichText::new(format!("{} {}", self.state.masks[i].kind.label(), i + 1)).strong()).clicked() {
                            self.selected = if sel { None } else { Some(i) };
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✖").on_hover_text("Изтрий").clicked() {
                                remove = Some(i);
                            }
                            if ui.small_button("Дублирай").on_hover_text("Копие на маската до нея").clicked() {
                                duplicate = Some(i);
                            }
                        });
                    });
                    if sel {
                        let m = &mut self.state.masks[i];
                        let (o, mut scale) = m.transform_at(t);
                        if ui.add(egui::Slider::new(&mut scale, 0.2..=3.0).text("Мащаб").custom_formatter(|v, _| format!("{:.0}%", v * 100.0))).changed() {
                            m.set_transform_at(t, fps, o, scale);
                        }
                        ui.add(egui::Slider::new(&mut m.blur, 1.0..=120.0).text("Blur").integer());
                        ui.add(egui::Slider::new(&mut m.feather, 0.0..=80.0).text("Feather").integer());
                        if ui.small_button("Върни позиция и размер").on_hover_text("Маха и ключовете за движение").clicked() {
                            m.offset = [0.0, 0.0];
                            m.scale = 1.0;
                            m.keys.clear();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Движение").strong());
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                            if ui
                                .add_enabled(!trk_running, crate::colored_button("◀ Проследи назад", crate::BLUE).min_size(vec2(w, 26.0)))
                                .on_hover_text("Маската следва обекта под нея от този кадър към началото")
                                .clicked()
                            {
                                track_go = Some((i, false));
                            }
                            if ui
                                .add_enabled(!trk_running, crate::colored_button("Проследи напред ▶", crate::BLUE).min_size(vec2(w, 26.0)))
                                .on_hover_text("Маската следва обекта под нея от този кадър към края (до OUT или до скриването ѝ)")
                                .clicked()
                            {
                                track_go = Some((i, true));
                            }
                        });
                        if trk_running {
                            ui.horizontal(|ui| {
                                if ui.add(crate::colored_button("Спри", crate::RED)).on_hover_text("Проследеното дотук се запазва").clicked() {
                                    self.track_cancel.store(true, Ordering::SeqCst);
                                }
                                ui.add(egui::ProgressBar::new(trk_progress).text(format!("Проследяване {:.0}%", trk_progress * 100.0)));
                            });
                        }
                        ui.checkbox(&mut self.track_scale, "Следи и размера (приближаване)");
                        ui.horizontal(|ui| {
                            if ui.small_button("◀").on_hover_text("Предишен ключ").clicked() {
                                seek_to = m.keys.iter().rev().find(|k| k.t < t - 0.5 / fps).map(|k| k.t);
                            }
                            if m.key_at(t, fps).is_some() {
                                if ui.button("✖ Махни ключа").on_hover_text("Махни ключа в този кадър").clicked() {
                                    m.remove_key(t, fps);
                                }
                            } else if ui.button("◆ Ключ тук  [K]").on_hover_text("Запомни положението на маската в този кадър").clicked() {
                                m.add_key(t, fps);
                            }
                            if ui.small_button("▶").on_hover_text("Следващ ключ").clicked() {
                                seek_to = m.keys.iter().find(|k| k.t > t + 0.5 / fps).map(|k| k.t);
                            }
                        });
                        if m.keys.is_empty() {
                            ui.weak("Неподвижна. С „Ключ тук“ или с проследяване маската започва да се движи.");
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(format!("Ключове: {}", m.keys.len()));
                                if ui.small_button("Изтрий всички").on_hover_text("Маската остава неподвижна, както е в този кадър").clicked() {
                                    m.clear_keys(t);
                                }
                            });
                            ui.weak("Местене или мащаб в кадър без ключ добавя ключ там; между ключовете маската се движи плавно.");
                        }
                        if !trk_msg.is_empty() {
                            ui.label(&trk_msg);
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Кога се вижда").strong());
                        let active = m.active_at(t);
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                            if ui
                                .add_enabled(!active || m.spans.is_empty(), crate::colored_button("Покажи от тук", crate::GREEN).min_size(vec2(w, 26.0)))
                                .on_hover_text("Маската се появява от текущия кадър до следващото скриване")
                                .clicked()
                            {
                                m.show_from(t, dur);
                            }
                            if ui
                                .add_enabled(active, crate::colored_button("Скрий от тук", crate::RED).min_size(vec2(w, 26.0)))
                                .on_hover_text("Маската изчезва от текущия кадър")
                                .clicked()
                            {
                                m.hide_from(t, dur);
                            }
                        });
                        let visible: Vec<(usize, [f64; 2])> =
                            m.spans.iter().copied().enumerate().filter(|(_, s)| s[1] - s[0] > 1e-3).collect();
                        if m.spans.is_empty() {
                            ui.weak("Видима през целия клип.");
                        } else if visible.is_empty() {
                            ui.weak("Скрита през целия клип.");
                        }
                        let mut drop_span = None;
                        for (k, s) in visible {
                            ui.horizontal(|ui| {
                                if ui.link(format!("{} – {}", fmt_tc(s[0], fps), fmt_tc(s[1], fps))).on_hover_text("Иди в началото").clicked() {
                                    seek_to = Some(s[0]);
                                }
                                if ui.small_button("✖").on_hover_text("Махни този участък").clicked() {
                                    drop_span = Some(k);
                                }
                            });
                        }
                        if let Some(k) = drop_span {
                            m.spans.remove(k);
                            if m.spans.iter().all(|s| s[1] - s[0] <= 1e-3) && !m.spans.is_empty() {
                                m.spans = vec![[0.0, 0.0]];
                            }
                        }
                        if !m.spans.is_empty() && ui.small_button("Видима през целия клип").clicked() {
                            m.spans.clear();
                        }
                        ui.weak("Ръбовете се местят с влачене в жълтата лента най-долу в timeline-а.");
                    }
                });
            }
        });
        if let Some(i) = duplicate {
            let mut m = self.state.masks[i].clone();
            m.nudge([0.03, 0.03]);
            self.state.masks.insert(i + 1, m);
            self.selected = Some(i + 1);
        }
        if let Some(i) = remove {
            self.state.masks.remove(i);
            self.selected = None;
        }
        if let Some(t) = seek_to {
            if self.playing {
                self.pause();
            }
            self.seek(t);
        }
        if let Some((i, forward)) = track_go {
            self.start_track(i, forward, ui.ctx());
        }
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let dur = self.info.duration;
        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), 140.0), Sense::click_and_drag());
        let r = resp.rect;
        let ruler_h = 20.0;
        let wave = Rect::from_min_max(pos2(r.left(), r.top() + ruler_h), r.max);

        // Мащаб с колелцето, превъртане с Shift+колелце.
        if let Some(hover) = resp.hover_pos() {
            let (dx, dy) = ui.input(|i| (i.smooth_scroll_delta.x, i.smooth_scroll_delta.y));
            let (v0, v1) = self.view;
            let span = v1 - v0;
            if dy != 0.0 {
                let pivot = v0 + ((hover.x - r.left()) / r.width()) as f64 * span;
                let new_span = (span * (-dy as f64 * 0.003).exp()).clamp(0.5_f64.min(dur), dur);
                let start = pivot - (pivot - v0) * new_span / span;
                self.view = (start, start + new_span);
            } else if dx != 0.0 {
                let shift = -dx as f64 / r.width() as f64 * span;
                self.view = (v0 + shift, v1 + shift);
            }
        }
        if self.playing && (self.playhead > self.view.1 || self.playhead < self.view.0) {
            let span = self.view.1 - self.view.0;
            self.view = (self.playhead, self.playhead + span);
        }
        let span = (self.view.1 - self.view.0).min(dur);
        let v0 = self.view.0.clamp(0.0, dur - span);
        self.view = (v0, v0 + span);
        let tx = |t: f64| r.left() + ((t - v0) / span) as f32 * r.width();
        let xt = |x: f32| v0 + ((x - r.left()) / r.width()) as f64 * span;

        painter.rect_filled(r, 4.0, Color32::from_gray(22));
        painter.rect_filled(Rect::from_min_max(r.min, pos2(r.right(), r.top() + ruler_h)), 4.0, Color32::from_gray(34));

        // Скала
        let px_per_sec = r.width() as f64 / span;
        let step = [0.04, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0]
            .into_iter()
            .find(|s| s * px_per_sec >= 70.0)
            .unwrap_or(600.0);
        let mut t = (v0 / step).ceil() * step;
        while t <= v0 + span {
            let x = tx(t);
            painter.line_segment([pos2(x, r.top() + ruler_h - 6.0), pos2(x, r.top() + ruler_h)], Stroke::new(1.0_f32, Color32::GRAY));
            let label = if step < 1.0 { self.tc(t) } else { self.tc(t)[..8].to_string() };
            painter.text(pos2(x + 3.0, r.top() + 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::monospace(11.0), Color32::LIGHT_GRAY);
            t += step;
        }

        // Waveform
        let mid = wave.center().y;
        match &*self.audio_data.lock().unwrap() {
            Some(Ok(a)) => {
                let color = Color32::from_rgb(80, 150, 220);
                let mut x = wave.left();
                while x < wave.right() {
                    let (t0, t1) = (xt(x), xt(x + 1.0));
                    let i0 = (t0 / a.peak_secs).max(0.0) as usize;
                    let i1 = ((t1 / a.peak_secs).ceil() as usize).max(i0 + 1).min(a.peaks.len());
                    let p = a.peaks.get(i0..i1).map_or(0.0, |s| s.iter().copied().fold(0.0, f32::max));
                    let hgt = (p / a.peak_max) * wave.height() * 0.46 * self.state.gain_at(t0);
                    if hgt > 0.3 {
                        painter.line_segment([pos2(x, mid - hgt), pos2(x, mid + hgt)], Stroke::new(1.0_f32, color));
                    }
                    x += 1.0;
                }
            }
            Some(Err(e)) => {
                painter.text(wave.center(), egui::Align2::CENTER_CENTER, format!("Звукът не може да се зареди: {e}"), egui::FontId::proportional(13.0), Color32::GRAY);
            }
            None if self.info.has_audio => {
                painter.text(wave.center(), egui::Align2::CENTER_CENTER, "Зареждане на звука…", egui::FontId::proportional(13.0), Color32::GRAY);
            }
            None => {
                painter.text(wave.center(), egui::Align2::CENTER_CENTER, "Няма звук", egui::FontId::proportional(13.0), Color32::GRAY);
            }
        }

        // Затъмняване извън IN–OUT и маркерите
        let (xi, xo) = (tx(self.state.in_t), tx(self.state.out_t));
        let shade = Color32::from_black_alpha(160);
        painter.rect_filled(Rect::from_min_max(wave.min, pos2(xi.clamp(wave.left(), wave.right()), wave.bottom())), 0.0, shade);
        painter.rect_filled(Rect::from_min_max(pos2(xo.clamp(wave.left(), wave.right()), wave.top()), wave.max), 0.0, shade);
        for (x, color, label, align) in [(xi, IN_COLOR, "IN", egui::Align2::LEFT_TOP), (xo, OUT_COLOR, "OUT", egui::Align2::RIGHT_TOP)] {
            if x >= r.left() - 1.0 && x <= r.right() + 1.0 {
                painter.line_segment([pos2(x, wave.top()), pos2(x, wave.bottom())], Stroke::new(2.0_f32, color));
                let dir = if label == "IN" { 1.0 } else { -1.0 };
                painter.rect_filled(Rect::from_two_pos(pos2(x, wave.top()), pos2(x + dir * 30.0, wave.top() + 16.0)), 2.0, color);
                painter.text(pos2(x + dir * 4.0, wave.top() + 1.0), align, label, egui::FontId::proportional(12.0), Color32::BLACK);
            }
        }
        // Fade на звука: крива от ръба до кръгчето, което се влачи.
        let fade_y = wave.top() + 26.0;
        let (xfi, xfo) = (tx(self.state.in_t + self.state.fade_in), tx(self.state.out_t - self.state.fade_out));
        // При нулев fade кръгчето стои малко навътре от маркера, за да не се бърка с него.
        let fade_handles = [
            (Drag::FadeIn, xfi + if self.state.fade_in > 0.0 { 0.0 } else { 7.0 }),
            (Drag::FadeOut, xfo - if self.state.fade_out > 0.0 { 0.0 } else { 7.0 }),
        ];
        if self.info.has_audio {
            let fade_color = Color32::from_rgb(255, 170, 60);
            for ((x_edge, x_end, len), (_, x_handle)) in
                [(xi, xfi, self.state.fade_in), (xo, xfo, self.state.fade_out)].into_iter().zip(fade_handles)
            {
                if len > 0.0 {
                    let curve: Vec<Pos2> = (0..=24)
                        .map(|k| {
                            let u = k as f32 / 24.0;
                            let g = (u * std::f32::consts::FRAC_PI_2).sin();
                            pos2(x_edge + (x_end - x_edge) * u, wave.bottom() - (wave.bottom() - fade_y) * g)
                        })
                        .collect();
                    painter.add(Shape::line(curve, Stroke::new(1.5_f32, fade_color)));
                }
                if x_handle >= r.left() - 6.0 && x_handle <= r.right() + 6.0 {
                    painter.circle(pos2(x_handle, fade_y), 5.0, fade_color, Stroke::new(1.0_f32, Color32::BLACK));
                }
            }
        }

        // Видимост на избраната маска: лента най-долу.
        let lane = Rect::from_min_max(pos2(wave.left(), wave.bottom() - 16.0), wave.max);
        // Маска през целия клип = един участък 0–край; ръбовете му се влачат веднага.
        // Ключовете на избраната маска: ромбчета над лентата (клик = отиди на ключа).
        let key_y = lane.top() - 7.0;
        let key_xs: Vec<(f32, f64)> = self
            .selected
            .and_then(|i| self.state.masks.get(i))
            .map(|m| m.keys.iter().map(|k| (tx(k.t), k.t)).filter(|(x, _)| *x >= r.left() - 5.0 && *x <= r.right() + 5.0).collect())
            .unwrap_or_default();
        for (x, _) in &key_xs {
            let d = 4.5;
            painter.add(Shape::convex_polygon(
                vec![pos2(*x, key_y - d), pos2(x + d, key_y), pos2(*x, key_y + d), pos2(x - d, key_y)],
                Color32::from_rgb(0, 220, 255),
                Stroke::new(1.0_f32, Color32::BLACK),
            ));
        }
        let sel_mask = self.selected.and_then(|i| {
            let spans = &self.state.masks.get(i)?.spans;
            Some((i, if spans.is_empty() { vec![[0.0, dur]] } else { spans.clone() }))
        });
        if let Some((i, spans)) = &sel_mask {
            painter.rect_filled(lane, 0.0, Color32::from_black_alpha(190));
            let on = Color32::from_rgba_unmultiplied(255, 215, 0, 150);
            for sp in spans {
                let (a, b) = (tx(sp[0]).max(lane.left()), tx(sp[1]).min(lane.right()));
                if b > a {
                    painter.rect_filled(Rect::from_min_max(pos2(a, lane.top() + 2.0), pos2(b, lane.bottom() - 2.0)), 2.0, on);
                }
                // Ръбовете са дръжки: по-широки и навътре от края на лентата, за да се хващат и в 0 и в края.
                for (x, dir) in [(tx(sp[0]), 1.0_f32), (tx(sp[1]), -1.0)] {
                    if x >= lane.left() - 1.0 && x <= lane.right() + 1.0 {
                        let x = x.clamp(lane.left() + 2.0, lane.right() - 2.0);
                        let grip = Rect::from_two_pos(pos2(x, lane.top() + 1.0), pos2(x + dir * 5.0, lane.bottom() - 1.0));
                        painter.rect_filled(grip, 1.0, Color32::YELLOW);
                    }
                }
            }
            painter.text(
                lane.left_center() + vec2(4.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("Маска {} — кога се вижда", i + 1),
                egui::FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
        let span_edge_at = |p: Pos2| -> Option<(usize, bool)> {
            let (_, spans) = sel_mask.as_ref()?;
            if !lane.expand(2.0).contains(p) {
                return None;
            }
            spans
                .iter()
                .enumerate()
                .flat_map(|(k, sp)| [(k, false, sp[0]), (k, true, sp[1])])
                .find_map(|(k, end, t)| {
                    let x = tx(t);
                    let visible = x >= lane.left() - 1.0 && x <= lane.right() + 1.0;
                    (visible && (x.clamp(lane.left() + 4.0, lane.right() - 4.0) - p.x).abs() < 7.0).then_some((k, end))
                })
        };

        let xp = tx(self.playhead);
        if xp >= r.left() && xp <= r.right() {
            painter.line_segment([pos2(xp, r.top()), pos2(xp, r.bottom())], Stroke::new(2.0_f32, Color32::from_rgb(255, 220, 60)));
            painter.add(Shape::convex_polygon(
                vec![pos2(xp - 6.0, r.top()), pos2(xp + 6.0, r.top()), pos2(xp, r.top() + 9.0)],
                Color32::from_rgb(255, 220, 60),
                Stroke::NONE,
            ));
        }

        // Мишка: IN/OUT маркерите се влачат, другаде — позиция.
        let near = |x: f32, p: Pos2| (x - p.x).abs() < 8.0 && p.y > wave.top();
        let has_audio = self.info.has_audio;
        let fade_at = |p: Pos2| fade_handles.iter().find(|(_, x)| has_audio && pos2(*x, fade_y).distance(p) < 8.0).map(|(d, _)| *d);
        if let Some(p) = resp.hover_pos() {
            if self.drag == Drag::None && (near(xi, p) || near(xo, p) || fade_at(p).is_some() || span_edge_at(p).is_some()) {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            let fade = match self.drag {
                Drag::FadeIn | Drag::FadeOut => Some(self.drag),
                _ => fade_at(p),
            };
            if let Some(d) = fade {
                let (name, v) = if d == Drag::FadeIn { ("Fade in", self.state.fade_in) } else { ("Fade out", self.state.fade_out) };
                resp.clone().on_hover_text_at_pointer(format!("{name}: {v:.2} сек"));
            }
        }
        if resp.drag_started() {
            let p = ui.input(|i| i.pointer.press_origin()).unwrap_or_default();
            self.drag = if let Some(d) = fade_at(p) {
                d
            } else if let Some((k, end)) = span_edge_at(p) {
                if let Some(m) = self.selected.and_then(|i| self.state.masks.get_mut(i)) {
                    if m.spans.is_empty() {
                        m.spans.push([0.0, dur]);
                    }
                }
                Drag::Span(k, end)
            } else if near(xi, p) {
                Drag::In
            } else if near(xo, p) {
                Drag::Out
            } else {
                Drag::Playhead
            };
            if self.playing {
                self.pause();
            }
        }
        if resp.dragged() || resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = xt(p.x).clamp(0.0, dur);
                match self.drag {
                    Drag::In => {
                        self.set_in(t);
                        self.seek(self.state.in_t);
                    }
                    Drag::Out => {
                        self.set_out(t);
                        self.seek(self.state.out_t - self.frame_len());
                    }
                    Drag::FadeIn => {
                        let s = &mut self.state;
                        s.fade_in = (t - s.in_t).clamp(0.0, (s.out_t - s.in_t - s.fade_out).max(0.0));
                    }
                    Drag::FadeOut => {
                        let s = &mut self.state;
                        s.fade_out = (s.out_t - t).clamp(0.0, (s.out_t - s.in_t - s.fade_in).max(0.0));
                    }
                    Drag::Span(k, end) => {
                        let (fps, fl) = (self.info.fps, self.frame_len());
                        let spans = self.selected.and_then(|i| self.state.masks.get_mut(i)).map(|m| &mut m.spans);
                        if let Some(spans) = spans.filter(|s| k < s.len()) {
                            let t = snap(t, fps);
                            if end {
                                let max = spans.get(k + 1).map_or(dur, |n| n[0]).max(spans[k][0] + fl);
                                spans[k][1] = t.clamp(spans[k][0] + fl, max);
                            } else {
                                let min = if k > 0 { spans[k - 1][1] } else { 0.0 };
                                spans[k][0] = t.clamp(min, (spans[k][1] - fl).max(min));
                            }
                            let edge = spans[k][usize::from(end)];
                            self.seek(edge);
                        }
                    }
                    Drag::Playhead | Drag::None => {
                        let on_key = resp.clicked() && (p.y - key_y).abs() < 8.0;
                        let key = key_xs.iter().filter(|_| on_key).find(|(x, _)| (x - p.x).abs() < 6.0);
                        self.seek(key.map_or(t, |k| k.1));
                    }
                }
            }
        }
        if resp.dragged() || resp.clicked() {
            if matches!(self.drag, Drag::Playhead | Drag::None | Drag::In | Drag::Out | Drag::Span(..)) {
                self.scrub_audio();
            }
        }
        if resp.drag_stopped() {
            self.drag = Drag::None;
            self.last_scrub = None;
        }
        ui.horizontal(|ui| {
            ui.weak("Колелце: мащаб · Shift+колелце: превъртане · Влачи IN/OUT маркерите · ← → кадър · I / O задава IN/OUT · K: ключ на маската · Интервал: пусни/пауза");
        });
        ui.add_space(2.0);
    }
}

/// ЧЧ:ММ:СС:КК
fn fmt_tc(t: f64, fps: f64) -> String {
    let t = t.max(0.0) + 1e-6;
    let secs = t.floor() as u64;
    let fr = fps.round().max(1.0) as u64;
    let ff = (((t - secs as f64) * fps).floor() as u64).min(fr - 1);
    format!("{:02}:{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60, ff)
}

fn snap(t: f64, fps: f64) -> f64 {
    (t * fps).round() / fps
}

/// Центриране на група в хоризонтален ред: отстъп по ширината от предишния кадър.
fn center_start(ui: &mut egui::Ui, id: egui::Id) -> f32 {
    let width: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
    ui.cursor().min.x
}

fn center_end(ui: &mut egui::Ui, id: egui::Id, x0: f32) {
    let width = ui.cursor().min.x - ui.spacing().item_spacing.x - x0;
    let old: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    if (width - old).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, width));
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }
}

/// [x0, y0, x1, y1]
fn bbox(pts: &[[f32; 2]]) -> [f32; 4] {
    pts.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
}

/// Точки за размер: 0 горе-ляво, после по часовниковата стрелка; 7 = ляво.
fn handle_pos(b: [f32; 4], h: usize) -> [f32; 2] {
    let (xm, ym) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    match h {
        0 => [b[0], b[1]],
        1 => [xm, b[1]],
        2 => [b[2], b[1]],
        3 => [b[2], ym],
        4 => [b[2], b[3]],
        5 => [xm, b[3]],
        6 => [b[0], b[3]],
        _ => [b[0], ym],
    }
}

fn handle_cursor(h: usize) -> CursorIcon {
    match h {
        0 | 4 => CursorIcon::ResizeNwSe,
        2 | 6 => CursorIcon::ResizeNeSw,
        1 | 5 => CursorIcon::ResizeVertical,
        _ => CursorIcon::ResizeHorizontal,
    }
}

/// Кутията след влачене на точка `h` до `p`; страните не могат да се разминат.
fn resized_box(b: [f32; 4], h: usize, p: [f32; 2]) -> [f32; 4] {
    const MIN: f32 = 0.01;
    let mut n = b;
    if matches!(h, 0 | 6 | 7) {
        n[0] = p[0].min(b[2] - MIN);
    }
    if matches!(h, 2 | 3 | 4) {
        n[2] = p[0].max(b[0] + MIN);
    }
    if matches!(h, 0 | 1 | 2) {
        n[1] = p[1].min(b[3] - MIN);
    }
    if matches!(h, 4 | 5 | 6) {
        n[3] = p[1].max(b[1] + MIN);
    }
    n
}

/// Пренася точките от кутия `from` в кутия `to`.
fn remap(pts: &[[f32; 2]], from: [f32; 4], to: [f32; 4]) -> Vec<[f32; 2]> {
    let sx = (to[2] - to[0]) / (from[2] - from[0]).max(1e-6);
    let sy = (to[3] - to[1]) / (from[3] - from[1]).max(1e-6);
    pts.iter().map(|p| [to[0] + (p[0] - from[0]) * sx, to[1] + (p[1] - from[1]) * sy]).collect()
}

fn point_in_poly(p: [f32; 2], poly: &[[f32; 2]]) -> bool {
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

/// Името на клипа без добавените от програмата таймкодове „ [ЧЧ-ММ-СС_ЧЧ-ММ-СС]“.
pub fn clean_name(stem: &str) -> String {
    match stem.rfind(" [") {
        Some(i) if stem.ends_with(']') => stem[..i].trim().to_string(),
        _ => stem.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_handles() {
        let rect = Mask::shape_points(Kind::Rect, [0.6, 0.5], [0.2, 0.1]);
        assert_eq!(bbox(&rect), [0.2, 0.1, 0.6, 0.5]);
        let ell = Mask::shape_points(Kind::Ellipse, [0.2, 0.2], [0.4, 0.6]);
        let b = bbox(&ell);
        assert!((b[0] - 0.2).abs() < 1e-3 && (b[3] - 0.6).abs() < 1e-3);
        assert!(point_in_poly([0.3, 0.4], &ell) && !point_in_poly([0.21, 0.21], &ell));

        // Долният десен ъгъл (4) разширява кутията; горният ляв не мърда.
        let nb = resized_box([0.2, 0.1, 0.6, 0.5], 4, [0.8, 0.9]);
        assert_eq!(nb, [0.2, 0.1, 0.8, 0.9]);
        let moved = remap(&rect, [0.2, 0.1, 0.6, 0.5], nb);
        assert_eq!(bbox(&moved), nb);
        // Лявата страна (7) не може да мине вдясно от дясната.
        let nb = resized_box([0.2, 0.1, 0.6, 0.5], 7, [0.9, 0.3]);
        assert!(nb[0] < nb[2] && nb[1] == 0.1 && nb[3] == 0.5);

        // Нова форма без ключове: мащабът и преместването влизат в точките.
        let mut m = Mask::new(Kind::Rect, rect.clone());
        m.scale = 2.0;
        m.offset = [0.1, 0.0];
        let before = m.effective();
        m.set_shape_at(0.0, before.clone());
        assert_eq!(m.points, before);
        let close = |a: &[[f32; 2]], b: &[[f32; 2]]| a.iter().zip(b).all(|(p, q)| (p[0] - q[0]).abs() < 1e-5 && (p[1] - q[1]).abs() < 1e-5);
        assert!(close(&m.effective(), &before));

        // С ключове: в този момент формата е точно новата, а движението остава.
        m.keys = vec![Keyframe { t: 0.0, offset: [0.0, 0.0], scale: 1.0 }, Keyframe { t: 2.0, offset: [0.2, 0.1], scale: 1.5 }];
        let now = m.effective_at(1.0);
        let target = remap(&now, bbox(&now), [0.1, 0.1, 0.5, 0.3]);
        m.set_shape_at(1.0, target.clone());
        assert!(close(&m.effective_at(1.0), &target));
        assert_eq!(m.keys.len(), 2);
    }

    #[test]
    fn keyframes() {
        let mut m = Mask::new(Kind::Rect, Mask::shape_points(Kind::Rect, [0.1, 0.1], [0.3, 0.2]));
        let fps = 25.0;
        // Без ключове местенето е за целия клип.
        m.set_transform_at(1.0, fps, [0.1, 0.0], 1.0);
        assert!(m.keys.is_empty() && m.transform_at(7.0).0 == [0.1, 0.0]);
        // Първи ключ, после местене в друг кадър → втори ключ; между тях — линейно.
        m.add_key(1.0, fps);
        m.set_transform_at(3.0, fps, [0.3, 0.2], 2.0);
        assert_eq!(m.keys.len(), 2);
        let (o, s) = m.transform_at(2.0);
        assert!((o[0] - 0.2).abs() < 1e-6 && (o[1] - 0.1).abs() < 1e-6 && (s - 1.5).abs() < 1e-6);
        assert_eq!(m.transform_at(0.0).0, [0.1, 0.0]);
        assert_eq!(m.transform_at(9.0).0, [0.3, 0.2]);
        // Същият кадър (до половин кадър) сменя ключа, не добавя нов.
        m.set_transform_at(3.01, fps, [0.4, 0.2], 2.0);
        assert_eq!(m.keys.len(), 2);
        // offset_for поставя средата на кутията точно там, където е поискана.
        let o = m.offset_for([0.5, 0.5], 1.7);
        let b = bbox(&m.shaped(o, 1.7));
        assert!(((b[0] + b[2]) / 2.0 - 0.5).abs() < 1e-5 && ((b[1] + b[3]) / 2.0 - 0.5).abs() < 1e-5);
        // Махането на последния ключ оставя маската там, където е била.
        m.remove_key(3.0, fps);
        m.remove_key(1.0, fps);
        assert!(m.keys.is_empty() && m.offset == [0.1, 0.0]);
        // Смяна на таймкода мести и ключовете.
        m.add_key(2.0, fps);
        let mut st = EditState { masks: vec![m], ..Default::default() };
        st.retime(1.5, 0.0, 10.0);
        assert_eq!(st.masks[0].keys[0].t, 3.5);
    }

    #[test]
    fn mask_show_hide() {
        let mut m = Mask::new(Kind::Rect, vec![[0.1, 0.1], [0.2, 0.1], [0.2, 0.2]]);
        assert!(m.active_at(0.0) && m.active_at(9.0));
        // Скрий в 4 → видима 0–4.
        m.hide_from(4.0, 10.0);
        assert_eq!(m.spans, vec![[0.0, 4.0]]);
        assert!(m.active_at(3.9) && !m.active_at(4.0));
        // Покажи в 7 → и 7–10.
        m.show_from(7.0, 10.0);
        assert_eq!(m.spans, vec![[0.0, 4.0], [7.0, 10.0]]);
        // Скрий в 8, после покажи в 5 → 5 се слива със следващия участък.
        m.hide_from(8.0, 10.0);
        m.show_from(5.0, 10.0);
        assert_eq!(m.spans, vec![[0.0, 4.0], [5.0, 8.0]]);
        // Скриване точно в началото на участък го премахва.
        m.hide_from(0.0, 10.0);
        assert_eq!(m.spans, vec![[5.0, 8.0]]);
        m.hide_from(5.0, 10.0);
        assert!(!m.spans.is_empty() && !(0..100).any(|k| m.active_at(k as f64 * 0.1)));
        m.show_from(2.0, 10.0);
        assert_eq!(m.spans, vec![[2.0, 10.0]]);
    }

    #[test]
    fn fade_curve() {
        assert_eq!(fade_gain(0.5, 1.0, 3.0, 0.5, 0.5), 1.0);
        assert_eq!(fade_gain(1.0, 1.0, 3.0, 0.5, 0.5), 0.0);
        assert!((fade_gain(1.25, 1.0, 3.0, 0.5, 0.5) - 0.7071).abs() < 1e-3);
        assert_eq!(fade_gain(2.0, 1.0, 3.0, 0.5, 0.5), 1.0);
        assert!(fade_gain(2.99, 1.0, 3.0, 0.5, 0.5) < 0.05);
        assert_eq!(fade_gain(2.0, 1.0, 3.0, 0.0, 0.0), 1.0);
    }

    #[test]
    fn old_masks_still_load() {
        let old = r#"{"in_t":1.0,"out_t":2.0,"masks":[{"points":[[0.1,0.1],[0.2,0.1],[0.2,0.2]],"offset":[0.0,0.0],"scale":1.0,"blur":25.0,"feather":8.0}]}"#;
        let s: EditState = serde_json::from_str(old).unwrap();
        assert_eq!(s.masks[0].kind, Kind::Lasso);
    }
}
