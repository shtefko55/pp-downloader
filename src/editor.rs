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

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Mask {
    /// Контур от ласото, нормализиран 0..1 спрямо кадъра.
    pub points: Vec<[f32; 2]>,
    pub offset: [f32; 2],
    pub scale: f32,
    /// Сила на размазването (sigma, пиксели в оригиналния размер).
    pub blur: f32,
    /// Омекотяване на ръба (пиксели в оригиналния размер).
    pub feather: f32,
}

impl Mask {
    fn new(points: Vec<[f32; 2]>) -> Self {
        Mask { points, offset: [0.0, 0.0], scale: 1.0, blur: 25.0, feather: 8.0 }
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
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Playhead,
    In,
    Out,
}

pub struct Editor {
    pub path: PathBuf,
    pub state: EditState,
    pub open: bool,
    pub viewport: egui::ViewportId,
    name: String,
    ffmpeg: PathBuf,
    crf: u32,
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
    frame_gen: u64,
    texture: Option<egui::TextureHandle>,
    shown_key: String,

    selected: Option<usize>,
    lasso_mode: bool,
    lasso: Vec<[f32; 2]>,
    moving: bool,
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
            EditState { in_t: a, out_t: b, masks: Vec::new() }
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
            frame_gen: 0,
            texture: None,
            shown_key: String::new(),
            selected: None,
            lasso_mode: false,
            lasso: Vec::new(),
            moving: false,
            show_blur: true,
            show_outlines: true,
            mask_cache: Vec::new(),
            drag: Drag::None,
            export: Default::default(),
            export_cancel: Default::default(),
        })
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
            egui::TopBottomPanel::top(egui::Id::new(("ed_top", id))).show(ctx, |ui| self.toolbar_ui(ui));
            egui::SidePanel::right(egui::Id::new(("ed_side", id)))
                .exact_width(300.0)
                .show(ctx, |ui| self.masks_ui(ui));
            egui::TopBottomPanel::bottom(egui::Id::new(("ed_timeline", id))).show(ctx, |ui| self.timeline_ui(ui));
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
        let t = t.max(0.0) + 1e-6;
        let secs = t.floor() as u64;
        let fr = self.info.fps.round().max(1.0) as u64;
        let ff = (((t - secs as f64) * self.info.fps).floor() as u64).min(fr - 1);
        format!("{:02}:{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60, ff)
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

    fn set_frame(&mut self, rgba: Vec<u8>) {
        self.frame = Some(rgba);
        self.frame_gen += 1;
    }

    fn tick(&mut self, ctx: &egui::Context) {
        while let Ok(f) = self.fetcher.rx.try_recv() {
            if !self.playing {
                self.set_frame(f.rgba);
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
                latest = Some(f.rgba);
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
                        Some(f) if f.t <= now + half => latest = self.pending.take().map(|f| f.rgba),
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
        if let Some(rgba) = latest {
            self.set_frame(rgba);
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
            self.lasso_mode = false;
            self.lasso.clear();
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

        if self.lasso_mode {
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            }
            if resp.drag_started() {
                self.lasso.clear();
                if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                    self.lasso.push(to_norm(p));
                }
            }
            if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let far = self.lasso.last().map_or(true, |l| to_screen(l).distance(p) > 3.0);
                    if far {
                        self.lasso.push(to_norm(p));
                    }
                }
            }
            if resp.drag_stopped() {
                if self.lasso.len() >= 3 {
                    self.state.masks.push(Mask::new(std::mem::take(&mut self.lasso)));
                    self.selected = Some(self.state.masks.len() - 1);
                }
                self.lasso.clear();
                self.lasso_mode = false;
            }
        } else {
            if resp.clicked() {
                self.selected = resp.interact_pointer_pos().and_then(|p| hit(&self.state.masks, p));
            }
            if resp.drag_started() {
                let origin = ui.input(|i| i.pointer.press_origin());
                let h = origin.and_then(|p| hit(&self.state.masks, p));
                self.moving = h.is_some();
                if h.is_some() {
                    self.selected = h;
                }
            }
            if resp.dragged() && self.moving {
                if let Some(m) = self.selected.and_then(|i| self.state.masks.get_mut(i)) {
                    let d = resp.drag_delta();
                    m.offset[0] += d.x / rect.width();
                    m.offset[1] += d.y / rect.height();
                }
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            } else if resp.hover_pos().is_some_and(|p| hit(&self.state.masks, p).is_some()) {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
            if resp.drag_stopped() {
                self.moving = false;
            }
        }

        let painter = painter.with_clip_rect(rect);
        for (i, m) in self.state.masks.iter().enumerate() {
            let sel = self.selected == Some(i);
            if !(self.show_outlines || sel) {
                continue;
            }
            let pts: Vec<Pos2> = m.effective().iter().map(to_screen).collect();
            let stroke = if sel { Stroke::new(2.0_f32, Color32::YELLOW) } else { Stroke::new(1.0_f32, Color32::from_white_alpha(170)) };
            painter.add(Shape::closed_line(pts, stroke));
        }
        if self.lasso.len() >= 2 {
            painter.add(Shape::line(self.lasso.iter().map(to_screen).collect(), Stroke::new(2.0_f32, Color32::from_rgb(0, 220, 255))));
        }
        if self.lasso_mode {
            painter.text(
                rect.min + vec2(12.0, 12.0),
                egui::Align2::LEFT_TOP,
                "Ласо: задръж левия бутон и очертай обекта (Esc = отказ)",
                egui::FontId::proportional(16.0),
                Color32::from_rgb(0, 220, 255),
            );
        }
    }

    // ---------- панели ----------

    fn toolbar_ui(&mut self, ui: &mut egui::Ui) {
        let fl = self.frame_len();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
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

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.export_ui(ui));
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
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
        let dir = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(out) = rfd::FileDialog::new()
            .set_title("Експорт — име на готовия файл")
            .set_directory(&dir)
            .set_file_name(format!("{}.mp4", clean_name(&self.name)))
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
        ui.add_space(8.0);
        ui.heading("Blur маски");
        ui.add_space(4.0);
        let label = if self.lasso_mode { "✖  Откажи ласото" } else { "✏  Нова маска (ласо)" };
        if ui.add(crate::colored_button(label, crate::PURPLE).min_size(vec2(ui.available_width(), 32.0))).clicked() {
            self.lasso_mode = !self.lasso_mode;
            self.lasso.clear();
            if self.playing {
                self.pause();
                self.seek(self.playhead);
            }
        }
        ui.weak("Очертай обекта в картината. Влачи маската, за да я преместиш. Delete изтрива избраната.");
        ui.checkbox(&mut self.show_blur, "Покажи blur в прегледа");
        ui.checkbox(&mut self.show_outlines, "Покажи контурите на всички маски");
        ui.separator();

        let mut remove = None;
        let mut duplicate = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if self.state.masks.is_empty() {
                ui.weak("Няма маски.");
            }
            for i in 0..self.state.masks.len() {
                let sel = self.selected == Some(i);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if ui.selectable_label(sel, egui::RichText::new(format!("Маска {}", i + 1)).strong()).clicked() {
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
                        ui.add(egui::Slider::new(&mut m.scale, 0.2..=3.0).text("Размер").custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                        ui.add(egui::Slider::new(&mut m.blur, 1.0..=120.0).text("Blur").integer());
                        ui.add(egui::Slider::new(&mut m.feather, 0.0..=80.0).text("Feather").integer());
                        if ui.small_button("Върни позиция и размер").clicked() {
                            m.offset = [0.0, 0.0];
                            m.scale = 1.0;
                        }
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
                    let hgt = (p / a.peak_max) * wave.height() * 0.46;
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
        if let Some(p) = resp.hover_pos() {
            if self.drag == Drag::None && (near(xi, p) || near(xo, p)) {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
        }
        if resp.drag_started() {
            let p = ui.input(|i| i.pointer.press_origin()).unwrap_or_default();
            self.drag = if near(xi, p) {
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

fn snap(t: f64, fps: f64) -> f64 {
    (t * fps).round() / fps
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
