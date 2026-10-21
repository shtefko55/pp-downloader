#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod worker;

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use worker::{fmt_secs, Shared, Status, Task, Tools};

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq)]
struct Tc {
    h: u32,
    m: u32,
    s: u32,
}

impl Tc {
    fn secs(self) -> u32 {
        self.h * 3600 + self.m * 60 + self.s
    }
    /// "1:23", "01:02:03", "83" → таймкод.
    fn parse(text: &str) -> Option<Tc> {
        let parts: Vec<u32> = text.trim().split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
        let t = match parts.as_slice() {
            [s] => *s,
            [m, s] => m * 60 + s,
            [h, m, s] => h * 3600 + m * 60 + s,
            _ => return None,
        };
        Some(Tc { h: t / 3600, m: t / 60 % 60, s: t % 60 })
    }
    fn file_label(self) -> String {
        format!("{:02}-{:02}-{:02}", self.h, self.m, self.s)
    }
}

#[derive(Serialize, Deserialize)]
struct Job {
    id: u64,
    url: String,
    name: String,
    start: Tc,
    end: Tc,
    #[serde(skip)]
    state: Shared,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    out_dir: String,
    handles: u32,
    parallel: usize,
    precise: bool,
    max_res: u32,
    crf: u32,
}

impl Default for Settings {
    fn default() -> Self {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        Settings {
            out_dir: format!("{home}\\Downloads\\PP_Clips"),
            handles: 5,
            parallel: 2,
            precise: true,
            max_res: 0,
            crf: 16,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    settings: Settings,
    jobs: Vec<Job>,
    next_id: u64,
}

struct App {
    saved: Saved,
    tools: Tools,
    queue: Arc<Mutex<VecDeque<Task>>>,
    active: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    update_msg: Arc<Mutex<String>>,
    scroll_to_add: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
            fonts.font_data.insert("segoe".into(), egui::FontData::from_owned(bytes));
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "segoe".into());
        }
        cc.egui_ctx.set_fonts(fonts);
        cc.egui_ctx.style_mut(|s| {
            s.text_styles.iter_mut().for_each(|(_, f)| f.size *= 1.1);
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        });

        let mut saved: Saved = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        if saved.jobs.is_empty() {
            saved.next_id += 1;
            saved.jobs.push(new_job(saved.next_id));
        }
        App {
            saved,
            tools: Tools::find(),
            queue: Default::default(),
            active: Default::default(),
            cancel: Default::default(),
            update_msg: Default::default(),
            scroll_to_add: false,
        }
    }

    fn add_job(&mut self, url: String, start: Tc, end: Tc) {
        self.saved.next_id += 1;
        let mut job = new_job(self.saved.next_id);
        job.url = url;
        job.start = start;
        job.end = end;
        self.saved.jobs.push(job);
    }

    fn start_all(&mut self, ctx: &egui::Context) {
        let s = &self.saved.settings;
        let opts = worker::Options {
            out_dir: PathBuf::from(&s.out_dir),
            precise: s.precise,
            max_res: s.max_res,
            crf: s.crf,
        };
        let mut queue = self.queue.lock().unwrap();
        for job in &self.saved.jobs {
            let mut st = job.state.lock().unwrap();
            if matches!(st.status, Status::Queued | Status::Working | Status::Done) || job.url.trim().is_empty() {
                continue;
            }
            if job.end.secs() <= job.start.secs() {
                st.status = Status::Failed;
                st.info = "Краят трябва да е след началото".into();
                continue;
            }
            st.status = Status::Queued;
            st.progress = 0.0;
            st.info = "В опашката".into();
            st.output = None;
            queue.push_back(Task {
                url: job.url.trim().to_string(),
                name: job.name.clone(),
                cut_start: job.start.secs().saturating_sub(s.handles),
                cut_end: job.end.secs() + s.handles,
                label: format!("{}_{}", job.start.file_label(), job.end.file_label()),
                opts: opts.clone(),
                state: job.state.clone(),
            });
        }
        drop(queue);
        worker::ensure_workers(&self.queue, &self.active, &self.cancel, s.parallel, &self.tools, ctx);
    }

    fn stop_all(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        for task in self.queue.lock().unwrap().drain(..) {
            let mut st = task.state.lock().unwrap();
            st.status = Status::Idle;
            st.info.clear();
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.saved.settings;
        ui.horizontal(|ui| {
            ui.label("Папка за запис:");
            ui.add(egui::TextEdit::singleline(&mut s.out_dir).desired_width(420.0));
            if ui.button("📁 Избери…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&s.out_dir).pick_folder() {
                    s.out_dir = dir.display().to_string();
                }
            }
            if ui.button("Отвори").clicked() {
                let _ = std::fs::create_dir_all(&s.out_dir);
                let _ = std::process::Command::new("explorer").arg(&s.out_dir).spawn();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Handles (± сек):");
            ui.add(egui::DragValue::new(&mut s.handles).range(0..=60));
            ui.separator();
            ui.label("Едновременно:");
            ui.add(egui::DragValue::new(&mut s.parallel).range(1..=6));
            ui.separator();
            ui.label("Макс. резолюция:");
            let res_text = |r: u32| if r == 0 { "Най-добра".to_string() } else { format!("{r}p") };
            egui::ComboBox::from_id_salt("maxres").selected_text(res_text(s.max_res)).show_ui(ui, |ui| {
                for r in [0, 2160, 1440, 1080, 720] {
                    ui.selectable_value(&mut s.max_res, r, res_text(r));
                }
            });
            ui.separator();
            ui.radio_value(&mut s.precise, true, "Прецизно (прекодиране)")
                .on_hover_text("Рязане точно до кадър, винаги H.264 (libx264). По-бавно.");
            ui.radio_value(&mut s.precise, false, "Бързо (копиране)").on_hover_text(
                "Ако източникът вече е H.264, видеото се копира без загуба; началото пада на най-близкия ключов кадър преди таймкода. Иначе се прекодира.",
            );
            if s.precise {
                ui.label("CRF:");
                ui.add(egui::DragValue::new(&mut s.crf).range(10..=28))
                    .on_hover_text("По-малко = по-високо качество. 16 е практически без видима загуба.");
            }
        });
    }

    fn jobs_ui(&mut self, ui: &mut egui::Ui) {
        let handles = self.saved.settings.handles;
        let mut remove = None;
        let mut duplicate = None;
        for (idx, job) in self.saved.jobs.iter_mut().enumerate() {
            let (status, progress, info, output) = {
                let st = job.state.lock().unwrap();
                (st.status, st.progress, st.info.clone(), st.output.clone())
            };
            let busy = matches!(status, Status::Queued | Status::Working);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("{:>2}.", idx + 1));
                        let w = (ui.available_width() - 330.0).max(200.0);
                        let r = ui.add(egui::TextEdit::singleline(&mut job.url).hint_text("Линк (YouTube, bnt.bg …)").desired_width(w));
                        ui.add(egui::TextEdit::singleline(&mut job.name).hint_text("Име на файла (по избор)").desired_width(200.0));
                        if ui.button("⧉").on_hover_text("Дублирай (същият линк, друг откъс)").clicked() {
                            duplicate = Some(idx);
                        }
                        if ui.button("✖").on_hover_text("Премахни").clicked() {
                            remove = Some(idx);
                        }
                        if r.changed() && matches!(status, Status::Done | Status::Failed | Status::Cancelled) {
                            *job.state.lock().unwrap() = Default::default();
                        }
                    });
                    ui.horizontal(|ui| {
                        let changed_a = tc_ui(ui, ("start", job.id), "Начало:", &mut job.start);
                        ui.add_space(16.0);
                        let changed_b = tc_ui(ui, ("end", job.id), "Край:", &mut job.end);
                        if (changed_a || changed_b) && matches!(status, Status::Done | Status::Failed | Status::Cancelled) {
                            *job.state.lock().unwrap() = Default::default();
                        }
                        ui.add_space(16.0);
                        let (a, b) = (job.start.secs(), job.end.secs());
                        if b > a {
                            let (ca, cb) = (a.saturating_sub(handles), b + handles);
                            ui.weak(format!("Реже се: {} – {}  ({})", fmt_secs(ca), fmt_secs(cb), fmt_secs(cb - ca)));
                        } else if a > 0 || b > 0 {
                            ui.colored_label(egui::Color32::from_rgb(220, 120, 0), "Краят трябва да е след началото");
                        }
                    });
                });
                match status {
                    Status::Idle => {}
                    Status::Queued => {
                        ui.weak(info);
                    }
                    Status::Working => {
                        ui.add(egui::ProgressBar::new(progress).text(info).animate(true));
                    }
                    Status::Done => {
                        ui.horizontal(|ui| {
                            ui.colored_label(egui::Color32::from_rgb(40, 170, 80), format!("✔ {info}"));
                            if let Some(p) = output {
                                if ui.small_button("Покажи файла").clicked() {
                                    let _ = std::process::Command::new("explorer")
                                        .raw_arg(format!("/select,\"{}\"", p.display()))
                                        .spawn();
                                }
                            }
                        });
                    }
                    Status::Failed => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), format!("✖ {info}"));
                    }
                    Status::Cancelled => {
                        ui.weak(info);
                    }
                }
            });
        }
        if let Some(i) = duplicate {
            let (url, name, end) = {
                let j = &self.saved.jobs[i];
                (j.url.clone(), j.name.clone(), j.end)
            };
            self.saved.next_id += 1;
            let mut job = new_job(self.saved.next_id);
            job.url = url;
            job.name = name;
            job.start = end;
            job.end = end;
            self.saved.jobs.insert(i + 1, job);
        }
        if let Some(i) = remove {
            self.saved.jobs.remove(i);
        }

        ui.add_space(4.0);
        let add = ui.add(colored_button("➕  Добави линк", BLUE).min_size(egui::vec2(160.0, 30.0)));
        if std::mem::take(&mut self.scroll_to_add) {
            add.scroll_to_me(Some(egui::Align::BOTTOM));
        }
        if add.clicked() {
            self.add_job(String::new(), Tc::default(), Tc::default());
            self.scroll_to_add = true;
        }
        ui.add_space(8.0);
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 140, 70);
const RED: egui::Color32 = egui::Color32::from_rgb(180, 55, 55);
const BLUE: egui::Color32 = egui::Color32::from_rgb(45, 105, 180);
const AMBER: egui::Color32 = egui::Color32::from_rgb(170, 115, 30);
const SLATE: egui::Color32 = egui::Color32::from_rgb(75, 85, 100);

fn colored_button(text: &str, fill: egui::Color32) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE).strong()).fill(fill)
}

fn new_job(id: u64) -> Job {
    Job { id, url: String::new(), name: String::new(), start: Tc::default(), end: Tc::default(), state: Default::default() }
}

/// Три падащи менюта ЧЧ:ММ:СС + поле за бързо въвеждане на таймкод.
fn tc_ui(ui: &mut egui::Ui, id: (&str, u64), label: &str, tc: &mut Tc) -> bool {
    let before = *tc;
    ui.label(label);
    let combo = |ui: &mut egui::Ui, salt: &str, v: &mut u32, max: u32, hint: &str| {
        egui::ComboBox::from_id_salt((id, salt))
            .width(46.0)
            .selected_text(format!("{:02}", *v))
            .height(320.0)
            .show_ui(ui, |ui| {
                for i in 0..=max {
                    ui.selectable_value(v, i, format!("{i:02}"));
                }
            })
            .response
            .on_hover_text(hint);
    };
    combo(ui, "h", &mut tc.h, 23, "часове");
    ui.label(":");
    combo(ui, "m", &mut tc.m, 59, "минути");
    ui.label(":");
    combo(ui, "s", &mut tc.s, 59, "секунди");

    let text_id = egui::Id::new((id, "text"));
    let mut text: String = ui.data_mut(|d| d.get_temp(text_id)).unwrap_or_default();
    let r = ui.add(egui::TextEdit::singleline(&mut text).hint_text("или 1:23:45").desired_width(70.0));
    if r.lost_focus() {
        if let Some(t) = Tc::parse(&text) {
            *tc = t;
        }
        text.clear();
    }
    ui.data_mut(|d| d.insert_temp(text_id, text));
    *tc != before
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let active = self.active.load(Ordering::SeqCst);
        if active == 0 {
            self.cancel.store(false, Ordering::SeqCst);
        } else {
            worker::ensure_workers(&self.queue, &self.active, &self.cancel, self.saved.settings.parallel, &self.tools, ctx);
        }

        let busy = active > 0 || !self.queue.lock().unwrap().is_empty();
        let big = egui::vec2(150.0, 32.0);

        egui::TopBottomPanel::top("settings").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("PP Downloader");
                ui.weak("— сваляне и рязане на откъси по таймкод");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, colored_button("Обнови yt-dlp", SLATE).min_size(big)).clicked() {
                        let (tools, msg, ctx) = (self.tools.clone(), self.update_msg.clone(), ctx.clone());
                        *msg.lock().unwrap() = "Обновяване…".into();
                        std::thread::spawn(move || {
                            *msg.lock().unwrap() = worker::update_ytdlp(&tools);
                            ctx.request_repaint();
                        });
                    }
                    if ui.add(colored_button("Изчисти готовите", AMBER).min_size(big)).clicked() {
                        self.saved.jobs.retain(|j| j.state.lock().unwrap().status != Status::Done);
                        if self.saved.jobs.is_empty() {
                            self.add_job(String::new(), Tc::default(), Tc::default());
                        }
                    }
                    ui.weak(self.update_msg.lock().unwrap().as_str());
                });
            });
            if self.tools.ytdlp.is_none() || self.tools.ffmpeg.is_none() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 60, 60),
                    "Не са намерени yt-dlp.exe / ffmpeg.exe — сложи ги в папка „bin“ до програмата.",
                );
            }
            self.settings_ui(ui);
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(colored_button("⬇  Свали всички", GREEN).min_size(big)).clicked() {
                    self.start_all(ctx);
                }
                if ui.add_enabled(busy, colored_button("⏹ Спри", RED).min_size(egui::vec2(80.0, 32.0))).clicked() {
                    self.stop_all();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let jobs = &self.saved.jobs;
                    let count = |s: Status| jobs.iter().filter(|j| j.state.lock().unwrap().status == s).count();
                    ui.weak(format!(
                        "{} откъса · {} готови · {} грешки",
                        jobs.len(),
                        count(Status::Done),
                        count(Status::Failed)
                    ));
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.jobs_ui(ui));
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.saved);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]).with_min_inner_size([800.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("PP Downloader", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
