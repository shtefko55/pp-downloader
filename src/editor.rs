//! Прозорец за прецизиране на клип: преглед, waveform, IN/OUT, blur маски и експорт.

use crate::blur::{self, Alpha};
use crate::export::{self, ExportJob, ExportStatus};
use crate::media::{self, AudioData, AudioOut, Frame, FrameFetcher, FrameStream, Info};
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
}

impl Mask {
    fn new(kind: Kind, points: Vec<[f32; 2]>) -> Self {
        Mask { kind, points, offset: [0.0, 0.0], scale: 1.0, blur: 25.0, feather: 8.0, spans: Vec::new() }
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

    /// Записва мащаба и преместването директно в точките.
    fn bake(&mut self) {
        self.points = self.effective();
        self.offset = [0.0, 0.0];
        self.scale = 1.0;
    }

    /// Контурът след мащаб (около центъра) и преместване.
    pub fn effective(&self) -> Vec<[f32; 2]> {
        let n = self.points.len().max(1) as f32;
        let c = self.points.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
        self.points
            .iter()
            .map(|p| {
                [c[0] + (p[0] - c[0]) * self.scale + self.offset[0], c[1] + (p[1] - c[1]) * self.scale + self.offset[1]]
            })
            .collect()
    }

    /// Алфа маска с размер w×h; `px_scale` = w / ширината на оригинала.
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
    mask_cache: Vec<Option<(Mask, Alpha)>>,

    view: (f64, f64),
    drag: Drag,

    export: Arc<Mutex<ExportStatus>>,
    export_cancel: Arc<AtomicBool>,
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
            viewport: egui::ViewportId::from_hash_of(("editor", &path)),
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
            drag: Drag::None,
            export: Default::default(),
            export_cancel: Default::default(),
        })
    }

    pub fn exporting(&self) -> bool {
        self.export.lock().unwrap().running
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Редакция — {}", self.name))
            .with_inner_size([1320.0, 900.0])
            .with_min_inner_size([960.0, 620.0]);
        let id = self.viewport;
        ctx.show_viewport_immediate(id, builder, |ctx, _| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.open = false;
            }
            self.tick(ctx);
            self.keys(ctx);
            self.update_texture(ctx);
            egui::SidePanel::right(egui::Id::new(("ed_side", id)))
                .exact_width(300.0)
                .show(ctx, |ui| self.masks_ui(ui));
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

    fn set_frame(&mut self, f: Frame) {
        self.frame = Some(f.rgba);
        self.frame_t = f.t;
        self.frame_gen += 1;
    }

    fn tick(&mut self, ctx: &egui::Context) {
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
        let esc = take(Modifiers::NONE, Key::Escape);
        if space {
            self.toggle_play();
        }
        for (pressed, delta) in [(left, -fl), (right, fl), (left_s, -1.0), (right_s, 1.0)] {
            if pressed {
                if self.playing {
                    self.pause();
                }
                self.seek(self.playhead + delta);
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
                if cache.as_ref().map_or(true, |(cm, _)| cm != m) {
                    *cache = Some((m.clone(), m.alpha(self.pw, self.ph, sp)));
                }
                if let Some((_, alpha)) = cache {
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
        let hit = |masks: &[Mask], p: Pos2| masks.iter().rposition(|m| point_in_poly(to_norm(p), &m.effective()));

        // Точката за преоразмеряване на избраната маска под курсора.
        let handle_at = |masks: &[Mask], sel: Option<usize>, p: Pos2| {
            let b = bbox(&masks.get(sel?)?.effective());
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
                        let m = &mut self.state.masks[i];
                        m.bake();
                        self.resize = Some(Resize { mask: i, handle: h, start_box: bbox(&m.points), start_points: m.points.clone() });
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
                            m.points = remap(&r.start_points, r.start_box, nb);
                        }
                        ui.ctx().set_cursor_icon(handle_cursor(r.handle));
                    } else if self.moving {
                        if let Some(m) = self.selected.and_then(|i| self.state.masks.get_mut(i)) {
                            let d = resp.drag_delta();
                            m.offset[0] += d.x / rect.width();
                            m.offset[1] += d.y / rect.height();
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
            let eff = m.effective();
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

    fn masks_ui(&mut self, ui: &mut egui::Ui) {
        if self.info.has_audio {
            ui.add_space(8.0);
            ui.heading("Звук");
            let max = self.state.out_t - self.state.in_t;
            let s = &mut self.state;
            egui::Grid::new(("fades", self.viewport)).num_columns(3).show(ui, |ui| {
                for fade_in in [true, false] {
                    let other = if fade_in { s.fade_out } else { s.fade_in };
                    let (label, v) = if fade_in { ("Fade in:", &mut s.fade_in) } else { ("Fade out:", &mut s.fade_out) };
                    let limit = (max - other).max(0.0);
                    ui.label(label);
                    ui.add(egui::DragValue::new(v).range(0.0..=limit).speed(0.02).fixed_decimals(2).suffix(" сек"));
                    ui.horizontal(|ui| {
                        for preset in [0.0_f64, 0.5, 1.0] {
                            if ui.small_button(if preset == 0.0 { "няма".to_string() } else { format!("{preset}") }).clicked() {
                                *v = preset.min(limit);
                            }
                        }
                    });
                    ui.end_row();
                }
            });
            ui.weak("Звукът плавно се усилва след IN и затихва преди OUT. Може и с влачене на кръгчетата в timeline-а.");
            ui.separator();
        }
        ui.add_space(8.0);
        ui.heading("Blur маски");
        ui.add_space(4.0);
        ui.label("Нова маска:");
        let w = (ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0;
        ui.horizontal(|ui| {
            for (kind, label) in [(Kind::Rect, "▭ Правоъг."), (Kind::Ellipse, "○ Кръг"), (Kind::Lasso, "✏ Ласо")] {
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
        let (t, dur, fps) = (self.playhead, self.info.duration, self.info.fps);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
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
                            if ui.small_button("⧉").on_hover_text("Дублирай").clicked() {
                                duplicate = Some(i);
                            }
                        });
                    });
                    if sel {
                        let m = &mut self.state.masks[i];
                        ui.add(egui::Slider::new(&mut m.scale, 0.2..=3.0).text("Мащаб").custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                        ui.add(egui::Slider::new(&mut m.blur, 1.0..=120.0).text("Blur").integer());
                        ui.add(egui::Slider::new(&mut m.feather, 0.0..=80.0).text("Feather").integer());
                        if ui.small_button("Върни позиция и размер").clicked() {
                            m.offset = [0.0, 0.0];
                            m.scale = 1.0;
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
            m.offset[0] += 0.03;
            m.offset[1] += 0.03;
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
                    Drag::Playhead | Drag::None => self.seek(t),
                }
            }
        }
        if resp.drag_stopped() {
            self.drag = Drag::None;
        }
        ui.horizontal(|ui| {
            ui.weak("Колелце: мащаб · Shift+колелце: превъртане · Влачи IN/OUT маркерите · ← → кадър · I / O задава IN/OUT · Интервал: пусни/пауза");
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
fn clean_name(stem: &str) -> String {
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

        // bake запазва видимия контур.
        let mut m = Mask::new(Kind::Rect, rect.clone());
        m.scale = 2.0;
        m.offset = [0.1, 0.0];
        let before = m.effective();
        m.bake();
        assert_eq!(m.points, before);
        let close = |a: &[[f32; 2]], b: &[[f32; 2]]| a.iter().zip(b).all(|(p, q)| (p[0] - q[0]).abs() < 1e-5 && (p[1] - q[1]).abs() < 1e-5);
        assert!(close(&m.effective(), &before));
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
